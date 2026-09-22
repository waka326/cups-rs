//! The job lifecycle: create, submit one PDF, close, status, cancel.
//!
//! Three rules shape everything here.
//!
//! 1. **Nothing is repeated.** Each request results in at most one call of
//!    the matching backend operation, and a failure of any kind is returned,
//!    never retried.
//! 2. **Dispatched is not failed.** Work that timed out before reaching the
//!    provider thread certainly did not happen and comes back as an ordinary
//!    `Timeout`. Work that reached the thread may have happened; it comes back
//!    as `JobOutcomeUnknown`, and its eventual result is recorded by the
//!    operation itself — against the handle, or for a cancel against the job.
//! 3. **No cleanup behind the caller's back.** A job left half-submitted is
//!    reported with its identity and is not cancelled automatically; see
//!    [`JobService::submit_pdf_bytes`].

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::provider::error::{ProviderError, ProviderResult};
use crate::provider::worker::{Delivery, ProviderHandle, Undelivered};

use super::backend::{CreateAccepted, JobBackend, MutationFailure};
use super::handle::ProviderJobHandle;
use super::options::ProviderJobOptions;
use super::registry::Registry;
use super::types::{
    CancelOutcome, CancelProgress, CreatedJob, JobOperation, ProviderJobId, ProviderJobStage,
    ProviderJobStatus, SubmitPhase,
};
use super::validate::{validate_pdf, validate_printer, validate_title};

/// Size of each write. Only affects how finely progress is reported.
const WRITE_CHUNK_BYTES: usize = 64 * 1024;

pub(crate) struct JobService<B: JobBackend> {
    worker: ProviderHandle,
    backend: Arc<Mutex<B>>,
    registry: Arc<Mutex<Registry>>,
}

/// What a handle-based operation needs to settle after dispatch.
struct Tracked {
    handle: ProviderJobHandle,
    operation: JobOperation,
    job: Option<ProviderJobId>,
    /// The stage the operation put the handle in.
    in_flight: ProviderJobStage,
    /// The stage to go back to if the operation was never dispatched.
    before: Option<ProviderJobStage>,
    /// The stage to record if the provider thread died mid-operation.
    lost: ProviderJobStage,
}

impl<B: JobBackend> JobService<B> {
    /// Share `worker` with the read-only API: one thread serialises every
    /// libcups call, whichever API issued it.
    ///
    /// Each service gets a registry identity no other service in the process
    /// has, so its handles cannot be used on — or collide with — another's.
    pub fn new(worker: ProviderHandle, backend: B) -> ProviderResult<Self> {
        Ok(Self {
            worker,
            backend: Arc::new(Mutex::new(backend)),
            registry: Arc::new(Mutex::new(Registry::new()?)),
        })
    }

    /// Create an empty job on exactly `printer`.
    ///
    /// Never falls back to another destination. All options are encoded
    /// before anything is sent; one that cannot be is an error, and no job is
    /// created.
    pub fn create_job(
        &self,
        printer: &str,
        title: &str,
        options: &ProviderJobOptions,
        timeout: Duration,
    ) -> ProviderResult<CreatedJob> {
        validate_printer(printer)?;
        validate_title(title)?;
        let attributes = options.to_attributes()?;

        let handle = lock(&self.registry)?.begin_create()?;
        let backend = Arc::clone(&self.backend);
        let registry = Arc::clone(&self.registry);
        let printer = printer.to_string();
        let title = title.to_string();

        let delivery = self.worker.execute_tracked("create_job", timeout, move || {
            let result = lock(&backend)
                .map_err(MutationFailure::NotSent)
                .and_then(|mut backend| backend.create_job(&printer, &title, &attributes));
            let (stage, outcome) = settle_create(handle, &printer, result);
            lock(&registry)?.settle(handle, stage)?;
            outcome
        });

        let result = self.conclude(
            Tracked {
                handle,
                operation: JobOperation::Create,
                job: None,
                in_flight: ProviderJobStage::Creating,
                before: None,
                lost: ProviderJobStage::CreateOutcomeUnknown,
            },
            delivery,
        );
        // A caller only learns the handle from success or an unknown outcome.
        // After any other error nothing could ever follow it up.
        if let Err(err) = &result
            && !err.is_outcome_unknown()
        {
            lock(&self.registry)?.discard(handle)?;
        }
        result
    }

    /// Send the job's one and only document.
    ///
    /// The format is always `application/pdf`. The document is sent
    /// unaltered, in one attempt, and is not marked as the last document: the
    /// job stays held until [`Self::close_job`], so a failure here never
    /// starts a partial job printing.
    ///
    /// On failure the job is **not** cancelled automatically. It is left open
    /// — held, not printing — and reported with its identity and the phase at
    /// which submission stopped, for the caller to cancel explicitly. libcups'
    /// own `cupsPrintFiles2` cancels instead, and discards the cancel's
    /// result; doing so here would add a mutation nobody asked for whose
    /// failure could only be hidden or reported as a second ambiguity. CUPS
    /// releases a job left open after its `MultipleOperationTimeout` (900 s by
    /// default), so an abandoned job does not stay held forever — which is why
    /// the caller must act on the error.
    pub fn submit_pdf_bytes(
        &self,
        handle: ProviderJobHandle,
        document: &[u8],
        timeout: Duration,
    ) -> ProviderResult<()> {
        validate_pdf(document)?;
        let job = lock(&self.registry)?.begin_submit(handle)?;

        let backend = Arc::clone(&self.backend);
        let registry = Arc::clone(&self.registry);
        let document = document.to_vec();
        let task_job = job.clone();

        let delivery = self
            .worker
            .execute_tracked("submit_pdf_bytes", timeout, move || {
                let result = lock(&backend)
                    .map_err(|err| SubmitStop::before_start(MutationFailure::NotSent(err)))
                    .and_then(|mut backend| send_document(&mut *backend, &task_job, &document));
                let (stage, outcome) = settle_submit(handle, &task_job, result);
                lock(&registry)?.settle(handle, stage)?;
                outcome
            });

        self.conclude(
            Tracked {
                handle,
                operation: JobOperation::Submit,
                job: Some(job.clone()),
                in_flight: ProviderJobStage::Submitting { job: job.clone() },
                before: Some(ProviderJobStage::Created { job: job.clone() }),
                lost: ProviderJobStage::SubmitOutcomeUnknown { job },
            },
            delivery,
        )
    }

    /// Finalise the job so the scheduler may print it.
    ///
    /// Success means CUPS accepted the finished job. It says nothing about
    /// paper: the job may still fail, be held by the printer, or print badly.
    pub fn close_job(
        &self,
        handle: ProviderJobHandle,
        timeout: Duration,
    ) -> ProviderResult<ProviderJobId> {
        let job = lock(&self.registry)?.begin_close(handle)?;

        let backend = Arc::clone(&self.backend);
        let registry = Arc::clone(&self.registry);
        let task_job = job.clone();

        let delivery = self.worker.execute_tracked("close_job", timeout, move || {
            let result = lock(&backend)
                .map_err(MutationFailure::NotSent)
                .and_then(|mut backend| backend.close_job(&task_job));
            let (stage, outcome) = settle_close(handle, &task_job, result);
            lock(&registry)?.settle(handle, stage)?;
            outcome
        });

        self.conclude(
            Tracked {
                handle,
                operation: JobOperation::Close,
                job: Some(job.clone()),
                in_flight: ProviderJobStage::Closing { job: job.clone() },
                before: Some(ProviderJobStage::Submitted { job: job.clone() }),
                lost: ProviderJobStage::CloseOutcomeUnknown { job: job.clone() },
            },
            delivery,
        )
        .map(|()| job)
    }

    /// The scheduler's view of a job. Read-only; safe to repeat.
    pub fn job_status(
        &self,
        job: &ProviderJobId,
        timeout: Duration,
    ) -> ProviderResult<ProviderJobStatus> {
        let backend = Arc::clone(&self.backend);
        let job = job.clone();
        self.worker.execute("get_job_status", timeout, move || {
            lock(&backend)?.job_status(&job)
        })
    }

    /// Ask the scheduler to cancel `job`. An explicit caller action only.
    ///
    /// The job is looked up first, in the same provider-thread turn: if it is
    /// not on `job.printer()`, nothing is sent (cupsd would cancel by id
    /// alone); if it has already finished, nothing is sent and the finished
    /// state is returned. Cancelling is not undoing — the job may already have
    /// printed.
    pub fn cancel_job(
        &self,
        job: &ProviderJobId,
        timeout: Duration,
    ) -> ProviderResult<CancelOutcome> {
        let previous = lock(&self.registry)?.begin_cancel(job)?;

        let backend = Arc::clone(&self.backend);
        let registry = Arc::clone(&self.registry);
        let task_job = job.clone();

        let delivery = self.worker.execute_tracked("cancel_job", timeout, move || {
            let outcome = lock(&backend).and_then(|mut backend| cancel(&mut *backend, &task_job));
            lock(&registry)?.finish_cancel(&task_job, CancelProgress::Finished(outcome.clone()));
            outcome
        });

        match delivery {
            Delivery::Completed(outcome) => outcome,
            Delivery::NotDispatched(err) => {
                lock(&self.registry)?.restore_cancel(job, previous);
                Err(err)
            }
            Delivery::Undelivered(why) => {
                if why == Undelivered::WorkerLost {
                    let lost = outcome_unknown(JobOperation::Cancel, None, Some(job.clone()), why);
                    lock(&self.registry)?.finish_cancel(job, CancelProgress::Finished(Err(lost)));
                }
                Err(outcome_unknown(
                    JobOperation::Cancel,
                    None,
                    Some(job.clone()),
                    why,
                ))
            }
        }
    }

    pub fn stage(&self, handle: ProviderJobHandle) -> ProviderResult<ProviderJobStage> {
        lock(&self.registry)?.stage(handle)
    }

    pub fn cancel_progress(&self, job: &ProviderJobId) -> ProviderResult<Option<CancelProgress>> {
        Ok(lock(&self.registry)?.cancel_progress(job))
    }

    pub fn release(&self, handle: ProviderJobHandle) -> ProviderResult<()> {
        lock(&self.registry)?.release(handle)
    }

    #[cfg(test)]
    pub fn tracked_handles(&self) -> usize {
        lock(&self.registry).map_or(0, |registry| registry.tracked_handles())
    }

    /// Turn a delivery into the caller's answer, fixing up the stage when
    /// the operation itself could not.
    fn conclude<T>(&self, tracked: Tracked, delivery: Delivery<T>) -> ProviderResult<T> {
        match delivery {
            // The operation recorded its own stage.
            Delivery::Completed(outcome) => outcome,
            // Never ran: the handle is as it was before the call.
            Delivery::NotDispatched(err) => {
                let mut registry = lock(&self.registry)?;
                match tracked.before {
                    Some(before) => registry.restore(tracked.handle, &tracked.in_flight, before)?,
                    None => registry.discard(tracked.handle)?,
                }
                Err(err)
            }
            Delivery::Undelivered(why) => {
                // A timed-out operation is still running and will record its
                // own outcome. One whose thread died never will.
                if why == Undelivered::WorkerLost {
                    lock(&self.registry)?.restore(
                        tracked.handle,
                        &tracked.in_flight,
                        tracked.lost,
                    )?;
                }
                Err(outcome_unknown(
                    tracked.operation,
                    Some(tracked.handle),
                    tracked.job,
                    why,
                ))
            }
        }
    }
}

/// Where a document submission stopped.
struct SubmitStop {
    phase: SubmitPhase,
    bytes_sent: u64,
    failure: MutationFailure,
}

impl SubmitStop {
    fn before_start(failure: MutationFailure) -> Self {
        Self {
            phase: SubmitPhase::Start,
            bytes_sent: 0,
            failure,
        }
    }
}

/// Start, write and finish one document — each step at most once, stopping
/// at the first failure.
fn send_document<B: JobBackend + ?Sized>(
    backend: &mut B,
    job: &ProviderJobId,
    document: &[u8],
) -> Result<(), SubmitStop> {
    backend
        .start_document(job)
        .map_err(SubmitStop::before_start)?;

    let mut bytes_sent: u64 = 0;
    for chunk in document.chunks(WRITE_CHUNK_BYTES) {
        backend
            .write_document(chunk)
            .map_err(|failure| SubmitStop {
                phase: SubmitPhase::Write,
                bytes_sent,
                failure,
            })?;
        bytes_sent += chunk.len() as u64;
    }

    backend.finish_document(job).map_err(|failure| SubmitStop {
        phase: SubmitPhase::Finish,
        bytes_sent,
        failure,
    })
}

/// Cancel after confirming the job is on this printer and not finished.
fn cancel<B: JobBackend + ?Sized>(
    backend: &mut B,
    job: &ProviderJobId,
) -> ProviderResult<CancelOutcome> {
    let status = backend.job_status(job)?;
    if status.is_terminal() {
        return Ok(CancelOutcome::AlreadyTerminal(status));
    }
    match backend.cancel_job(job) {
        Ok(()) => Ok(CancelOutcome::CancelRequested),
        Err(MutationFailure::NotSent(err)) => Err(err),
        Err(MutationFailure::Rejected { reason, detail }) => Err(ProviderError::JobCancelFailed {
            job: job.clone(),
            reason,
            detail,
        }),
        Err(MutationFailure::NoAnswer { detail }) => Err(ProviderError::JobOutcomeUnknown {
            operation: JobOperation::Cancel,
            handle: None,
            job: Some(job.clone()),
            in_flight: false,
            detail,
        }),
    }
}

fn settle_create(
    handle: ProviderJobHandle,
    printer: &str,
    result: Result<CreateAccepted, MutationFailure>,
) -> (ProviderJobStage, ProviderResult<CreatedJob>) {
    let unknown = |detail: String| {
        (
            ProviderJobStage::CreateOutcomeUnknown,
            Err(ProviderError::JobOutcomeUnknown {
                operation: JobOperation::Create,
                handle: Some(handle),
                job: None,
                in_flight: false,
                detail,
            }),
        )
    };
    match result {
        Ok(accepted) => match ProviderJobId::new(printer, accepted.job_id) {
            Ok(job) => (
                ProviderJobStage::Created { job: job.clone() },
                Ok(CreatedJob {
                    handle,
                    job,
                    ignored_attributes: accepted.ignored_attributes,
                }),
            ),
            // The scheduler said yes with an id no job can have. A job may
            // exist; its identity is not usable.
            Err(_) => unknown(format!("scheduler reported job id {}", accepted.job_id)),
        },
        Err(MutationFailure::NotSent(err)) => (ProviderJobStage::CreateNotSent, Err(err)),
        Err(MutationFailure::Rejected { reason, detail }) => (
            ProviderJobStage::CreateFailed { reason },
            Err(ProviderError::JobCreateFailed {
                printer: printer.to_string(),
                reason,
                detail,
            }),
        ),
        Err(MutationFailure::NoAnswer { detail }) => unknown(detail),
    }
}

fn settle_submit(
    handle: ProviderJobHandle,
    job: &ProviderJobId,
    result: Result<(), SubmitStop>,
) -> (ProviderJobStage, ProviderResult<()>) {
    let Err(stop) = result else {
        return (ProviderJobStage::Submitted { job: job.clone() }, Ok(()));
    };
    match (stop.phase, stop.failure) {
        // Nothing left the provider: the job is exactly as it was, and may be
        // submitted to again.
        (SubmitPhase::Start, MutationFailure::NotSent(err)) => {
            (ProviderJobStage::Created { job: job.clone() }, Err(err))
        }
        // A complete document went out and the verdict never came back. It
        // may have been accepted.
        (SubmitPhase::Finish, MutationFailure::NoAnswer { detail }) => (
            ProviderJobStage::SubmitOutcomeUnknown { job: job.clone() },
            Err(ProviderError::JobOutcomeUnknown {
                operation: JobOperation::Submit,
                handle: Some(handle),
                job: Some(job.clone()),
                in_flight: false,
                detail,
            }),
        ),
        // Everything else leaves a job with no document. cupsd runs the
        // Send-Document handler only once the whole request body has
        // arrived, and discards a partial body; a refused document is not
        // attached either. The job exists and is still open.
        (phase, failure) => {
            let (reason, detail) = match failure {
                MutationFailure::Rejected { reason, detail } => (Some(reason), detail),
                MutationFailure::NoAnswer { detail } => (None, detail),
                MutationFailure::NotSent(err) => (None, err.to_string()),
            };
            (
                ProviderJobStage::SubmitFailed {
                    job: job.clone(),
                    phase,
                    bytes_sent: stop.bytes_sent,
                },
                Err(ProviderError::JobSubmitFailed {
                    job: job.clone(),
                    phase,
                    bytes_sent: stop.bytes_sent,
                    reason,
                    detail,
                }),
            )
        }
    }
}

fn settle_close(
    handle: ProviderJobHandle,
    job: &ProviderJobId,
    result: Result<(), MutationFailure>,
) -> (ProviderJobStage, ProviderResult<()>) {
    match result {
        Ok(()) => (ProviderJobStage::Closed { job: job.clone() }, Ok(())),
        // Nothing sent: still submitted, and may be closed again.
        Err(MutationFailure::NotSent(err)) => {
            (ProviderJobStage::Submitted { job: job.clone() }, Err(err))
        }
        Err(MutationFailure::Rejected { reason, detail }) => (
            ProviderJobStage::CloseFailed {
                job: job.clone(),
                reason,
            },
            Err(ProviderError::JobCloseFailed {
                job: job.clone(),
                reason,
                detail,
            }),
        ),
        Err(MutationFailure::NoAnswer { detail }) => (
            ProviderJobStage::CloseOutcomeUnknown { job: job.clone() },
            Err(ProviderError::JobOutcomeUnknown {
                operation: JobOperation::Close,
                handle: Some(handle),
                job: Some(job.clone()),
                in_flight: false,
                detail,
            }),
        ),
    }
}

fn outcome_unknown(
    operation: JobOperation,
    handle: Option<ProviderJobHandle>,
    job: Option<ProviderJobId>,
    why: Undelivered,
) -> ProviderError {
    let (in_flight, detail) = match why {
        Undelivered::TimedOut => (
            true,
            "deadline passed after dispatch; the result is recorded when it returns".to_string(),
        ),
        Undelivered::WorkerLost => (
            false,
            "provider thread stopped without answering".to_string(),
        ),
    };
    ProviderError::JobOutcomeUnknown {
        operation,
        handle,
        job,
        in_flight,
        detail,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> ProviderResult<MutexGuard<'_, T>> {
    mutex
        .lock()
        .map_err(|_| ProviderError::InternalContractViolation {
            detail: "job state lock poisoned".into(),
        })
}
