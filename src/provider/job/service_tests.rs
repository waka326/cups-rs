//! Lifecycle tests against the scripted backend. No CUPS job is created.

use std::num::NonZeroU32;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::provider::error::ProviderError;
use crate::provider::worker::ProviderHandle;

use super::backend::CreateAccepted;
use super::fake::{Calls, FakeBackend, Op, no_answer, not_sent, rejected};
use super::handle::ProviderJobHandle;
use super::options::{AttributeValue, ProviderJobOptions, Sides};
use super::service::JobService;
use super::types::{
    CancelOutcome, CancelProgress, JobOperation, JobRejection, ProviderJobId, ProviderJobStage,
    ProviderJobStatus, SubmitPhase,
};

const PRINTER: &str = "Canon_TS5400_series";
const GENEROUS: Duration = Duration::from_secs(5);
/// Long enough that a blocked fake operation outlasts it every time.
const SHORT: Duration = Duration::from_millis(60);
/// Larger than one write chunk, so a document takes several writes.
const DOCUMENT_BYTES: usize = 200 * 1024;

fn pdf() -> Vec<u8> {
    let mut document = b"%PDF-1.7\n".to_vec();
    document.extend((0..DOCUMENT_BYTES).map(|index| (index % 251) as u8));
    document
}

fn options() -> ProviderJobOptions {
    let mut options = ProviderJobOptions::new(NonZeroU32::new(1).expect("non-zero"));
    options.sides = Some(Sides::OneSided);
    options
}

fn job42() -> ProviderJobId {
    ProviderJobId::new(PRINTER, 42).expect("valid")
}

struct Rig {
    worker: ProviderHandle,
    service: JobService<FakeBackend>,
    calls: Arc<Mutex<Calls>>,
}

impl Rig {
    fn new(configure: impl FnOnce(&mut FakeBackend)) -> Self {
        let (mut backend, calls) = FakeBackend::succeeding();
        configure(&mut backend);
        let worker = ProviderHandle::new().expect("thread starts");
        let service = JobService::new(worker.clone(), backend).expect("service");
        Self {
            worker,
            service,
            calls,
        }
    }

    fn calls(&self) -> Calls {
        self.calls.lock().expect("calls").clone()
    }

    fn created(&self) -> ProviderJobHandle {
        self.service
            .create_job(PRINTER, "postcard", &options(), GENEROUS)
            .expect("create")
            .handle
    }

    fn submitted(&self) -> ProviderJobHandle {
        let handle = self.created();
        self.service
            .submit_pdf_bytes(handle, &pdf(), GENEROUS)
            .expect("submit");
        handle
    }

    fn wait_for_stage(
        &self,
        handle: ProviderJobHandle,
        wanted: impl Fn(&ProviderJobStage) -> bool,
    ) {
        let start = Instant::now();
        loop {
            let stage = self.service.stage(handle).expect("handle known");
            if wanted(&stage) {
                return;
            }
            assert!(start.elapsed() < GENEROUS, "stage stuck at {stage:?}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Hold the provider thread until the returned sender is dropped or fired.
    fn occupy(&self) -> (mpsc::Sender<()>, thread::JoinHandle<()>) {
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (started_tx, started_rx) = mpsc::channel();
        let worker = self.worker.clone();
        let occupant = thread::spawn(move || {
            worker
                .execute("occupant", GENEROUS, move || {
                    let _ = started_tx.send(());
                    let _ = release_rx.recv_timeout(GENEROUS);
                    Ok(())
                })
                .expect("occupant runs");
        });
        started_rx.recv_timeout(GENEROUS).expect("occupant started");
        (release_tx, occupant)
    }
}

// --- Lifecycle success ------------------------------------------------------

#[test]
fn create_submit_close_calls_each_step_exactly_once() {
    let rig = Rig::new(|_| {});
    let document = pdf();

    let created = rig
        .service
        .create_job(PRINTER, "postcard", &options(), GENEROUS)
        .expect("create");
    assert_eq!(created.job, job42());
    rig.service
        .submit_pdf_bytes(created.handle, &document, GENEROUS)
        .expect("submit");
    let closed = rig
        .service
        .close_job(created.handle, GENEROUS)
        .expect("close");

    assert_eq!(closed, job42());
    assert_eq!(closed.printer(), PRINTER);
    assert_eq!(closed.scheduler_job_id(), 42);
    assert_eq!(
        rig.service.stage(created.handle).expect("known"),
        ProviderJobStage::Closed { job: job42() }
    );

    let calls = rig.calls();
    assert_eq!(calls.count(Op::Create), 1);
    assert_eq!(calls.count(Op::Start), 1);
    assert_eq!(calls.count(Op::Finish), 1);
    assert_eq!(calls.count(Op::Close), 1);
    assert_eq!(calls.count(Op::Cancel), 0);
    // Every byte, once, in order.
    assert_eq!(calls.written, document);
    assert_eq!(calls.printer.as_deref(), Some(PRINTER));
    assert_eq!(calls.title.as_deref(), Some("postcard"));
    assert!(
        calls
            .attributes
            .iter()
            .any(|attribute| attribute.name == "sides"
                && attribute.value == AttributeValue::Keyword("one-sided".into()))
    );
    assert!(calls.attributes.iter().any(
        |attribute| attribute.name == "copies" && attribute.value == AttributeValue::Integer(1)
    ));
}

#[test]
fn ignored_attributes_are_reported_not_hidden() {
    let rig = Rig::new(|backend| {
        backend.create = Ok(CreateAccepted {
            job_id: 42,
            ignored_attributes: vec!["media-col".into()],
        });
    });
    let created = rig
        .service
        .create_job(PRINTER, "postcard", &options(), GENEROUS)
        .expect("the job exists, so creation succeeded");
    assert_eq!(created.ignored_attributes, vec!["media-col".to_string()]);
}

// --- Invalid sequences: refused locally, nothing sent -----------------------

#[test]
fn invalid_sequences_are_refused_without_touching_cups() {
    let rig = Rig::new(|_| {});

    // Close before submit.
    let fresh = rig.created();
    assert!(matches!(
        rig.service.close_job(fresh, GENEROUS),
        Err(ProviderError::JobStateConflict {
            operation: JobOperation::Close,
            ..
        })
    ));

    // Submit twice.
    rig.service
        .submit_pdf_bytes(fresh, &pdf(), GENEROUS)
        .expect("first submit");
    assert!(matches!(
        rig.service.submit_pdf_bytes(fresh, &pdf(), GENEROUS),
        Err(ProviderError::JobStateConflict {
            operation: JobOperation::Submit,
            ..
        })
    ));

    // Close twice, and submit after close.
    rig.service.close_job(fresh, GENEROUS).expect("close");
    assert!(matches!(
        rig.service.close_job(fresh, GENEROUS),
        Err(ProviderError::JobStateConflict { .. })
    ));
    assert!(matches!(
        rig.service.submit_pdf_bytes(fresh, &pdf(), GENEROUS),
        Err(ProviderError::JobStateConflict { .. })
    ));

    // A handle this provider never issued.
    let stranger = Rig::new(|_| {}).created();
    assert!(matches!(
        rig.service.submit_pdf_bytes(stranger, &pdf(), GENEROUS),
        Err(ProviderError::UnknownJobHandle { .. })
    ));

    let calls = rig.calls();
    assert_eq!(calls.count(Op::Create), 1);
    assert_eq!(calls.count(Op::Start), 1);
    assert_eq!(calls.count(Op::Finish), 1);
    assert_eq!(calls.count(Op::Close), 1);
}

#[test]
fn malformed_requests_are_refused_before_dispatch() {
    let rig = Rig::new(|_| {});
    assert!(matches!(
        rig.service.create_job("", "t", &options(), GENEROUS),
        Err(ProviderError::InvalidJobRequest { .. })
    ));
    assert!(matches!(
        rig.service
            .create_job(PRINTER, "a\0b", &options(), GENEROUS),
        Err(ProviderError::InvalidJobRequest { .. })
    ));
    let handle = rig.created();
    assert!(matches!(
        rig.service.submit_pdf_bytes(handle, b"", GENEROUS),
        Err(ProviderError::InvalidJobRequest { .. })
    ));
    assert!(matches!(
        rig.service.submit_pdf_bytes(handle, b"\x89PNG", GENEROUS),
        Err(ProviderError::InvalidJobRequest { .. })
    ));
    // Still Created: a refused payload did not use up the one document.
    assert_eq!(
        rig.service.stage(handle).expect("known"),
        ProviderJobStage::Created { job: job42() }
    );
    assert_eq!(rig.calls().count(Op::Start), 0);
}

// --- No retry at any phase --------------------------------------------------

#[test]
fn a_failure_at_each_phase_is_attempted_exactly_once() {
    type Configure = Box<dyn Fn(&mut FakeBackend)>;
    let scenarios: Vec<(&str, Configure, Op)> = vec![
        (
            "create refused",
            Box::new(|b| b.create = Err(rejected(JobRejection::NotAccepting))),
            Op::Create,
        ),
        (
            "create unanswered",
            Box::new(|b| b.create = Err(no_answer())),
            Op::Create,
        ),
        (
            "start refused",
            Box::new(|b| b.start = Err(rejected(JobRejection::NotPossible))),
            Op::Start,
        ),
        (
            "write fails",
            Box::new(|b| b.fail_write_after = Some((1_000, no_answer()))),
            Op::Write,
        ),
        (
            "finish refused",
            Box::new(|b| b.finish = Err(rejected(JobRejection::DocumentFormatNotSupported))),
            Op::Finish,
        ),
        (
            "finish unanswered",
            Box::new(|b| b.finish = Err(no_answer())),
            Op::Finish,
        ),
        (
            "close refused",
            Box::new(|b| b.close = Err(rejected(JobRejection::NotAuthorized))),
            Op::Close,
        ),
        (
            "close unanswered",
            Box::new(|b| b.close = Err(no_answer())),
            Op::Close,
        ),
    ];

    for (name, configure, failing) in scenarios {
        let rig = Rig::new(|backend| configure(backend));
        let outcome = rig
            .service
            .create_job(PRINTER, "postcard", &options(), GENEROUS)
            .and_then(|created| {
                rig.service
                    .submit_pdf_bytes(created.handle, &pdf(), GENEROUS)
                    .map(|()| created.handle)
            })
            .and_then(|handle| rig.service.close_job(handle, GENEROUS));
        assert!(outcome.is_err(), "{name}: must fail");

        let calls = rig.calls();
        assert_eq!(calls.count(failing), 1, "{name}: {failing:?} once");
        assert!(calls.count(Op::Create) <= 1, "{name}: at most one create");
        assert!(calls.count(Op::Start) <= 1, "{name}: at most one document");
        assert!(calls.count(Op::Close) <= 1, "{name}: at most one close");
        assert_eq!(calls.count(Op::Cancel), 0, "{name}: no automatic cancel");
    }
}

#[test]
fn cancel_failures_are_attempted_exactly_once() {
    for failure in [rejected(JobRejection::NotPossible), no_answer()] {
        let rig = Rig::new(|backend| backend.cancel = Err(failure.clone()));
        let outcome = rig.service.cancel_job(&job42(), GENEROUS);
        match failure {
            super::backend::MutationFailure::Rejected { .. } => assert!(matches!(
                outcome,
                Err(ProviderError::JobCancelFailed {
                    reason: JobRejection::NotPossible,
                    ..
                })
            )),
            _ => assert!(matches!(
                outcome,
                Err(ProviderError::JobOutcomeUnknown {
                    operation: JobOperation::Cancel,
                    in_flight: false,
                    ..
                })
            )),
        }
        assert_eq!(rig.calls().count(Op::Cancel), 1);
    }
}

// --- Timeout before dispatch: nothing happened -------------------------------

#[test]
fn a_create_that_expires_in_the_queue_never_reaches_the_backend() {
    let rig = Rig::new(|_| {});
    let (release, occupant) = rig.occupy();

    let result = rig
        .service
        .create_job(PRINTER, "postcard", &options(), SHORT);

    // An ordinary timeout: the mutation certainly did not happen.
    assert!(matches!(result, Err(ProviderError::Timeout { .. })));
    release.send(()).expect("occupant waiting");
    occupant.join().expect("occupant joins");
    assert_eq!(rig.calls().count(Op::Create), 0);
}

#[test]
fn a_submit_that_expires_in_the_queue_leaves_the_job_submittable() {
    let rig = Rig::new(|_| {});
    let handle = rig.created();
    let (release, occupant) = rig.occupy();

    let result = rig.service.submit_pdf_bytes(handle, &pdf(), SHORT);

    assert!(matches!(result, Err(ProviderError::Timeout { .. })));
    assert_eq!(
        rig.service.stage(handle).expect("known"),
        ProviderJobStage::Created { job: job42() },
        "nothing was sent, so the one document is still available"
    );
    release.send(()).expect("occupant waiting");
    occupant.join().expect("occupant joins");
    assert_eq!(rig.calls().count(Op::Start), 0);
}

// --- Timeout after dispatch: the outcome is kept ------------------------------

#[test]
fn late_create_success_keeps_the_job_identity() {
    let mut gate = None;
    let rig = Rig::new(|backend| gate = Some(backend.block(Op::Create)));
    let gate = gate.expect("gate");

    let err = rig
        .service
        .create_job(PRINTER, "postcard", &options(), SHORT)
        .expect_err("caller gives up first");
    let ProviderError::JobOutcomeUnknown {
        operation: JobOperation::Create,
        handle: Some(handle),
        job: None,
        in_flight: true,
        ..
    } = err
    else {
        panic!("a dispatched create must be unknown, not failed: {err:?}");
    };
    assert_eq!(gate.wait_entered(), Op::Create);
    assert_eq!(
        rig.service.stage(handle).expect("known"),
        ProviderJobStage::Creating
    );

    // The scheduler's answer arrives after the caller left.
    gate.release();
    rig.wait_for_stage(handle, |stage| !stage.is_in_flight());
    assert_eq!(
        rig.service.stage(handle).expect("known"),
        ProviderJobStage::Created { job: job42() },
        "the late job id must be recorded, not discarded"
    );

    // The recovered identity is usable, and nothing was created twice.
    rig.service
        .submit_pdf_bytes(handle, &pdf(), GENEROUS)
        .expect("submit to the recovered job");
    assert_eq!(rig.calls().count(Op::Create), 1);
}

#[test]
fn a_create_that_times_out_cannot_be_repeated_on_its_handle() {
    let mut gate = None;
    let rig = Rig::new(|backend| gate = Some(backend.block(Op::Create)));
    let gate = gate.expect("gate");

    let Err(ProviderError::JobOutcomeUnknown {
        handle: Some(handle),
        ..
    }) = rig
        .service
        .create_job(PRINTER, "postcard", &options(), SHORT)
    else {
        panic!("expected unknown outcome");
    };
    // No operation on the handle can start while the create is in flight.
    assert!(matches!(
        rig.service.submit_pdf_bytes(handle, &pdf(), SHORT),
        Err(ProviderError::JobStateConflict { .. })
    ));
    assert!(rig.service.release(handle).is_err());

    gate.release();
    rig.wait_for_stage(handle, |stage| !stage.is_in_flight());
    assert_eq!(rig.calls().count(Op::Create), 1);
}

#[test]
fn close_that_times_out_is_reconciled_when_it_returns() {
    let mut gate = None;
    let rig = Rig::new(|backend| gate = Some(backend.block(Op::Close)));
    let gate = gate.expect("gate");
    let handle = rig.submitted();

    let err = rig
        .service
        .close_job(handle, SHORT)
        .expect_err("caller gives up first");
    assert!(matches!(
        err,
        ProviderError::JobOutcomeUnknown {
            operation: JobOperation::Close,
            in_flight: true,
            job: Some(_),
            ..
        }
    ));
    gate.wait_entered();

    gate.release();
    rig.wait_for_stage(handle, |stage| !stage.is_in_flight());
    assert_eq!(
        rig.service.stage(handle).expect("known"),
        ProviderJobStage::Closed { job: job42() }
    );
    let calls = rig.calls();
    assert_eq!(calls.count(Op::Create), 1, "no recreated job");
    assert_eq!(calls.count(Op::Start), 1, "no resubmission");
    assert_eq!(calls.count(Op::Close), 1, "no second close");
}

#[test]
fn close_whose_answer_is_lost_stays_explicitly_unknown() {
    let mut gate = None;
    let rig = Rig::new(|backend| {
        gate = Some(backend.block(Op::Close));
        backend.close = Err(no_answer());
    });
    let gate = gate.expect("gate");
    let handle = rig.submitted();

    assert!(rig.service.close_job(handle, SHORT).is_err());
    gate.wait_entered();
    gate.release();
    rig.wait_for_stage(handle, |stage| !stage.is_in_flight());

    assert_eq!(
        rig.service.stage(handle).expect("known"),
        ProviderJobStage::CloseOutcomeUnknown { job: job42() }
    );
    // Neither close nor submit can be re-issued on this handle.
    assert!(rig.service.close_job(handle, GENEROUS).is_err());
    assert!(
        rig.service
            .submit_pdf_bytes(handle, &pdf(), GENEROUS)
            .is_err()
    );
    let calls = rig.calls();
    assert_eq!(calls.count(Op::Close), 1);
    assert_eq!(calls.count(Op::Start), 1);
    assert_eq!(calls.count(Op::Create), 1);
}

#[test]
fn cancel_that_times_out_is_recorded_and_not_repeated() {
    let mut gate = None;
    let rig = Rig::new(|backend| gate = Some(backend.block(Op::Cancel)));
    let gate = gate.expect("gate");

    let err = rig
        .service
        .cancel_job(&job42(), SHORT)
        .expect_err("caller gives up first");
    assert!(matches!(
        err,
        ProviderError::JobOutcomeUnknown {
            operation: JobOperation::Cancel,
            in_flight: true,
            ..
        }
    ));
    gate.wait_entered();
    assert_eq!(
        rig.service.cancel_progress(&job42()).expect("readable"),
        Some(CancelProgress::InFlight)
    );
    // A second cancel is refused locally while the first is unanswered.
    assert!(matches!(
        rig.service.cancel_job(&job42(), SHORT),
        Err(ProviderError::JobOperationInProgress { .. })
    ));

    gate.release();
    let start = Instant::now();
    while rig.service.cancel_progress(&job42()).expect("readable") == Some(CancelProgress::InFlight)
    {
        assert!(start.elapsed() < GENEROUS, "cancel never recorded");
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        rig.service.cancel_progress(&job42()).expect("readable"),
        Some(CancelProgress::Finished(Ok(CancelOutcome::CancelRequested)))
    );
    assert_eq!(rig.calls().count(Op::Cancel), 1);
}

#[test]
fn a_dead_provider_thread_is_an_unknown_outcome_not_a_failure() {
    let rig = Rig::new(|backend| backend.panic_on = Some(Op::Create));
    let err = rig
        .service
        .create_job(PRINTER, "postcard", &options(), GENEROUS)
        .expect_err("thread died");
    let ProviderError::JobOutcomeUnknown {
        handle: Some(handle),
        in_flight: false,
        ..
    } = err
    else {
        panic!("expected unknown outcome, got {err:?}");
    };
    assert_eq!(
        rig.service.stage(handle).expect("known"),
        ProviderJobStage::CreateOutcomeUnknown
    );
}

// --- Partial submission -----------------------------------------------------

#[test]
fn partial_write_failure_reports_the_job_and_sends_nothing_more() {
    const LIMIT: u64 = 128 * 1024;
    let rig = Rig::new(|backend| backend.fail_write_after = Some((LIMIT, no_answer())));
    let handle = rig.created();
    let document = pdf();

    let err = rig
        .service
        .submit_pdf_bytes(handle, &document, GENEROUS)
        .expect_err("write fails");
    let ProviderError::JobSubmitFailed {
        job,
        phase: SubmitPhase::Write,
        bytes_sent,
        reason: None,
        ..
    } = err
    else {
        panic!("expected a write-phase failure, got {err:?}");
    };
    assert_eq!(job, job42(), "the job's identity is returned");
    assert_eq!(bytes_sent, LIMIT);

    let calls = rig.calls();
    // The bytes before the failure went once; nothing after it went at all.
    assert_eq!(calls.written, document[..LIMIT as usize].to_vec());
    assert_eq!(
        calls.count(Op::Write),
        3,
        "two chunks accepted, the third failed"
    );
    assert_eq!(calls.count(Op::Start), 1, "no second document");
    assert_eq!(calls.count(Op::Finish), 0);
    assert_eq!(
        calls.count(Op::Cancel),
        0,
        "not cancelled behind the caller"
    );

    assert_eq!(
        rig.service.stage(handle).expect("known"),
        ProviderJobStage::SubmitFailed {
            job: job42(),
            phase: SubmitPhase::Write,
            bytes_sent: LIMIT,
        }
    );
    // The one document is used up; the job must be cancelled explicitly.
    assert!(matches!(
        rig.service.submit_pdf_bytes(handle, &document, GENEROUS),
        Err(ProviderError::JobStateConflict { .. })
    ));
    assert_eq!(rig.calls().count(Op::Start), 1);
}

#[test]
fn each_submit_phase_failure_is_distinguished() {
    let start = Rig::new(|backend| backend.start = Err(rejected(JobRejection::NotPossible)));
    let handle = start.created();
    assert!(matches!(
        start.service.submit_pdf_bytes(handle, &pdf(), GENEROUS),
        Err(ProviderError::JobSubmitFailed {
            phase: SubmitPhase::Start,
            bytes_sent: 0,
            reason: Some(JobRejection::NotPossible),
            ..
        })
    ));
    assert_eq!(start.calls().count(Op::Write), 0);

    let finish = Rig::new(|backend| {
        backend.finish = Err(rejected(JobRejection::DocumentFormatNotSupported))
    });
    let handle = finish.created();
    let total = pdf().len() as u64;
    assert!(matches!(
        finish.service.submit_pdf_bytes(handle, &pdf(), GENEROUS),
        Err(ProviderError::JobSubmitFailed {
            phase: SubmitPhase::Finish,
            bytes_sent,
            reason: Some(JobRejection::DocumentFormatNotSupported),
            ..
        }) if bytes_sent == total
    ));

    // All data sent, no verdict: may have been accepted, so unknown.
    let lost = Rig::new(|backend| backend.finish = Err(no_answer()));
    let handle = lost.created();
    assert!(matches!(
        lost.service.submit_pdf_bytes(handle, &pdf(), GENEROUS),
        Err(ProviderError::JobOutcomeUnknown {
            operation: JobOperation::Submit,
            in_flight: false,
            job: Some(_),
            ..
        })
    ));
    assert_eq!(
        lost.service.stage(handle).expect("known"),
        ProviderJobStage::SubmitOutcomeUnknown { job: job42() }
    );
}

#[test]
fn a_submit_that_provably_sent_nothing_can_be_made_again() {
    let rig = Rig::new(|backend| backend.start = Err(not_sent()));
    let handle = rig.created();
    assert!(matches!(
        rig.service.submit_pdf_bytes(handle, &pdf(), GENEROUS),
        Err(ProviderError::ConnectionFailed { .. })
    ));
    assert_eq!(
        rig.service.stage(handle).expect("known"),
        ProviderJobStage::Created { job: job42() }
    );
}

#[test]
fn a_create_that_sent_nothing_leaves_no_handle_behind() {
    let rig = Rig::new(|backend| {
        backend.create = Err(super::backend::MutationFailure::NotSent(
            ProviderError::PrinterNotFound {
                printer: PRINTER.into(),
            },
        ))
    });
    assert!(matches!(
        rig.service
            .create_job(PRINTER, "postcard", &options(), GENEROUS),
        Err(ProviderError::PrinterNotFound { .. })
    ));
    // The handle was never returned, so it is not kept either.
    assert_eq!(rig.service.tracked_handles(), 0);
}

// --- Status and cancel ------------------------------------------------------

#[test]
fn status_errors_stay_distinct() {
    let not_found =
        Rig::new(|backend| backend.status = Err(ProviderError::JobNotFound { job: job42() }));
    assert!(matches!(
        not_found.service.job_status(&job42(), GENEROUS),
        Err(ProviderError::JobNotFound { .. })
    ));

    let failed = Rig::new(|backend| {
        backend.status = Err(ProviderError::JobStatusQueryFailed {
            job: job42(),
            detail: "no answer".into(),
        })
    });
    assert!(matches!(
        failed.service.job_status(&job42(), GENEROUS),
        Err(ProviderError::JobStatusQueryFailed { .. })
    ));

    let unknown = Rig::new(|backend| backend.status = Ok(ProviderJobStatus::Unknown));
    assert_eq!(
        unknown.service.job_status(&job42(), GENEROUS),
        Ok(ProviderJobStatus::Unknown),
        "an unrecognised state is reported as Unknown, not as an error or Completed"
    );
}

#[test]
fn cancel_confirms_the_job_before_sending_anything() {
    for (status, expected_calls) in [
        (Err(ProviderError::JobNotFound { job: job42() }), 0),
        (
            Err(ProviderError::JobDestinationMismatch {
                job: job42(),
                reported_printer: "Other_Printer".into(),
            }),
            0,
        ),
        (
            Err(ProviderError::JobStatusQueryFailed {
                job: job42(),
                detail: "no answer".into(),
            }),
            0,
        ),
        (Ok(ProviderJobStatus::Completed), 0),
        (Ok(ProviderJobStatus::Canceled), 0),
        (Ok(ProviderJobStatus::Held), 1),
    ] {
        let rig = Rig::new(|backend| backend.status = status.clone());
        let outcome = rig.service.cancel_job(&job42(), GENEROUS);
        assert_eq!(
            rig.calls().count(Op::Cancel),
            expected_calls,
            "status {status:?} -> {outcome:?}"
        );
        if let Ok(terminal) = status
            && terminal.is_terminal()
        {
            assert_eq!(outcome, Ok(CancelOutcome::AlreadyTerminal(terminal)));
        }
    }
}

#[test]
fn polling_status_never_cancels() {
    let rig = Rig::new(|backend| backend.status = Ok(ProviderJobStatus::Stopped));
    for _ in 0..5 {
        let _ = rig.service.job_status(&job42(), GENEROUS);
    }
    assert_eq!(rig.calls().count(Op::Cancel), 0);
    assert_eq!(rig.calls().count(Op::Status), 5);
}

// --- Serialisation with the read-only API -----------------------------------

#[test]
fn job_operations_share_the_single_provider_thread() {
    // While a job operation holds the provider thread, a read cannot start:
    // it queues and, given a short deadline, expires without running. One
    // thread serialises job calls and reads alike.
    let mut gate = None;
    let rig = Rig::new(|backend| gate = Some(backend.block(Op::Create)));
    let gate = gate.expect("gate");

    let creator = {
        let service = &rig.service;
        thread::scope(|scope| {
            let create =
                scope.spawn(|| service.create_job(PRINTER, "postcard", &options(), GENEROUS));
            gate.wait_entered();

            let read_ran = Arc::new(Mutex::new(false));
            let flag = Arc::clone(&read_ran);
            let read = rig.worker.execute("read", SHORT, move || {
                *flag.lock().expect("flag") = true;
                Ok(())
            });
            assert!(matches!(read, Err(ProviderError::Timeout { .. })));
            assert!(
                !*read_ran.lock().expect("flag"),
                "read ran beside a job call"
            );

            gate.release();
            create.join().expect("create joins")
        })
    };
    assert!(creator.is_ok());
}

// --- Handles belong to the provider that issued them -------------------------
//
// Every provider numbers its handles from the same starting point. These tests
// build two providers whose handles would collide if the handle carried only
// that number, and check that neither can reach the other's job.

/// Two providers, each with one created job at the same local sequence.
fn two_providers_with_created_jobs() -> (Rig, ProviderJobHandle, Rig, ProviderJobHandle) {
    let a = Rig::new(|_| {});
    let b = Rig::new(|_| {});
    let a_handle = a.created();
    let b_handle = b.created();
    (a, a_handle, b, b_handle)
}

#[test]
fn handles_from_different_providers_never_compare_equal() {
    let (_a, a_handle, _b, b_handle) = two_providers_with_created_jobs();
    assert_ne!(
        a_handle, b_handle,
        "first handles of two providers must not be the same handle"
    );
}

#[test]
fn a_foreign_handle_cannot_submit_to_this_providers_job() {
    let (a, a_handle, b, b_handle) = two_providers_with_created_jobs();

    let result = b.service.submit_pdf_bytes(a_handle, &pdf(), GENEROUS);

    assert!(
        matches!(result, Err(ProviderError::UnknownJobHandle { .. })),
        "B must refuse A's handle, got {result:?}"
    );
    assert_eq!(b.calls().count(Op::Start), 0, "nothing sent through B");
    assert_eq!(b.calls().count(Op::Write), 0);
    assert_eq!(
        b.service.stage(b_handle).expect("B's own job"),
        ProviderJobStage::Created { job: job42() },
        "B's job is untouched"
    );
    assert_eq!(
        a.service.stage(a_handle).expect("A's own job"),
        ProviderJobStage::Created { job: job42() },
        "A's job is untouched"
    );
    assert_eq!(a.calls().count(Op::Start), 0);
}

#[test]
fn a_foreign_handle_cannot_close_this_providers_job() {
    let a = Rig::new(|_| {});
    let b = Rig::new(|_| {});
    let a_handle = a.submitted();
    let b_handle = b.submitted();

    let result = b.service.close_job(a_handle, GENEROUS);

    assert!(
        matches!(result, Err(ProviderError::UnknownJobHandle { .. })),
        "B must refuse A's handle, got {result:?}"
    );
    assert_eq!(b.calls().count(Op::Close), 0, "nothing sent through B");
    assert_eq!(
        b.service.stage(b_handle).expect("B's own job"),
        ProviderJobStage::Submitted { job: job42() }
    );
    assert_eq!(a.calls().count(Op::Close), 0);
}

#[test]
fn a_foreign_handle_cannot_release_this_providers_job() {
    let (_a, a_handle, b, b_handle) = two_providers_with_created_jobs();

    let result = b.service.release(a_handle);

    assert!(
        matches!(result, Err(ProviderError::UnknownJobHandle { .. })),
        "B must refuse A's handle, got {result:?}"
    );
    assert_eq!(
        b.service.stage(b_handle),
        Ok(ProviderJobStage::Created { job: job42() }),
        "B's own job must still be tracked"
    );
}

#[test]
fn a_foreign_handle_cannot_read_this_providers_job() {
    let (_a, a_handle, b, _b_handle) = two_providers_with_created_jobs();
    assert!(matches!(
        b.service.stage(a_handle),
        Err(ProviderError::UnknownJobHandle { .. })
    ));
}

#[test]
fn providers_created_concurrently_issue_distinct_handles() {
    // No assumption about which provider is created or allocates first.
    const PROVIDERS: usize = 8;
    const HANDLES_EACH: usize = 4;
    let barrier = Arc::new(std::sync::Barrier::new(PROVIDERS));
    let workers: Vec<_> = (0..PROVIDERS)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                let rig = Rig::new(|_| {});
                let handles: Vec<_> = (0..HANDLES_EACH).map(|_| rig.created()).collect();
                (rig, handles)
            })
        })
        .collect();
    let results: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().expect("worker joins"))
        .collect();

    let mut all: Vec<ProviderJobHandle> = results
        .iter()
        .flat_map(|(_, handles)| handles.iter().copied())
        .collect();
    let total = all.len();
    all.sort();
    all.dedup();
    assert_eq!(all.len(), total, "every issued handle is distinct");

    // And each provider still refuses every other provider's handles.
    for (index, (rig, _)) in results.iter().enumerate() {
        for (other, (_, handles)) in results.iter().enumerate() {
            if index == other {
                continue;
            }
            for handle in handles {
                assert!(matches!(
                    rig.service.stage(*handle),
                    Err(ProviderError::UnknownJobHandle { .. })
                ));
            }
        }
    }
}

#[test]
fn a_job_id_outlives_the_provider_that_created_it() {
    // Handles are bound to their provider; job ids are scheduler identity and
    // must stay usable by a new provider, e.g. after an app restart.
    let job = {
        let original = Rig::new(|_| {});
        let handle = original.submitted();
        original.service.close_job(handle, GENEROUS).expect("close")
    };
    let rebuilt = ProviderJobId::new(job.printer(), job.scheduler_job_id()).expect("persisted id");

    let successor = Rig::new(|backend| backend.status = Ok(ProviderJobStatus::Pending));
    assert_eq!(
        successor.service.job_status(&rebuilt, GENEROUS),
        Ok(ProviderJobStatus::Pending)
    );
    assert_eq!(
        successor.service.cancel_job(&rebuilt, GENEROUS),
        Ok(CancelOutcome::CancelRequested)
    );
    assert_eq!(
        successor
            .service
            .cancel_progress(&rebuilt)
            .expect("readable"),
        Some(CancelProgress::Finished(Ok(CancelOutcome::CancelRequested)))
    );
    assert_eq!(successor.calls().count(Op::Cancel), 1);
}
