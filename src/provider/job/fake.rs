//! A scripted backend for lifecycle tests. Never touches CUPS.
//!
//! Each operation's answer is set in advance, every call is counted, and any
//! one operation can be made to block until the test releases it — which is
//! how "the caller timed out, then the operation finished" is reproduced
//! deterministically rather than by racing a clock.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::provider::error::{ProviderError, ProviderResult};

use super::backend::{CreateAccepted, JobBackend, MutationFailure};
use super::options::JobAttribute;
use super::types::{ProviderJobId, ProviderJobStatus};

/// Longest a blocked operation waits for release before failing the test.
const GATE_LIMIT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Op {
    Create,
    Start,
    Write,
    Finish,
    Close,
    Cancel,
    Status,
}

/// Everything the backend was asked to do.
#[derive(Clone, Debug, Default)]
pub(super) struct Calls {
    pub log: Vec<Op>,
    pub printer: Option<String>,
    pub title: Option<String>,
    pub attributes: Vec<JobAttribute>,
    /// Bytes the backend accepted, in order.
    pub written: Vec<u8>,
}

impl Calls {
    pub fn count(&self, op: Op) -> usize {
        self.log.iter().filter(|logged| **logged == op).count()
    }
}

pub(super) struct FakeBackend {
    calls: Arc<Mutex<Calls>>,
    pub create: Result<CreateAccepted, MutationFailure>,
    pub start: Result<(), MutationFailure>,
    /// Fail the write that would carry the document past this many bytes.
    pub fail_write_after: Option<(u64, MutationFailure)>,
    pub finish: Result<(), MutationFailure>,
    pub close: Result<(), MutationFailure>,
    pub cancel: Result<(), MutationFailure>,
    pub status: ProviderResult<ProviderJobStatus>,
    pub panic_on: Option<Op>,
    gate: Option<(Op, Receiver<()>)>,
    entered: Option<Sender<Op>>,
}

/// The test's side of a blocked operation.
pub(super) struct Gate {
    release: Sender<()>,
    entered: Receiver<Op>,
}

impl Gate {
    /// Wait until the blocked operation has started on the provider thread.
    pub fn wait_entered(&self) -> Op {
        self.entered
            .recv_timeout(GATE_LIMIT)
            .expect("blocked operation never started")
    }

    pub fn release(&self) {
        self.release.send(()).expect("blocked operation is waiting");
    }
}

impl FakeBackend {
    /// A backend on which every operation succeeds, creating job 42.
    pub fn succeeding() -> (Self, Arc<Mutex<Calls>>) {
        let calls = Arc::new(Mutex::new(Calls::default()));
        let backend = Self {
            calls: Arc::clone(&calls),
            create: Ok(CreateAccepted {
                job_id: 42,
                ignored_attributes: Vec::new(),
            }),
            start: Ok(()),
            fail_write_after: None,
            finish: Ok(()),
            close: Ok(()),
            cancel: Ok(()),
            status: Ok(ProviderJobStatus::Processing),
            panic_on: None,
            gate: None,
            entered: None,
        };
        (backend, calls)
    }

    /// Make `op` block until the returned gate releases it.
    pub fn block(&mut self, op: Op) -> Gate {
        let (release, released) = mpsc::channel();
        let (entered_tx, entered) = mpsc::channel();
        self.gate = Some((op, released));
        self.entered = Some(entered_tx);
        Gate { release, entered }
    }

    fn record(&mut self, op: Op) {
        self.calls.lock().expect("calls").log.push(op);
        if self.panic_on == Some(op) {
            panic!("scripted panic in {op:?}");
        }
        if let Some((gated, released)) = &self.gate
            && *gated == op
        {
            if let Some(entered) = &self.entered {
                let _ = entered.send(op);
            }
            released
                .recv_timeout(GATE_LIMIT)
                .expect("test never released the blocked operation");
        }
    }
}

impl JobBackend for FakeBackend {
    fn create_job(
        &mut self,
        printer: &str,
        title: &str,
        attributes: &[JobAttribute],
    ) -> Result<CreateAccepted, MutationFailure> {
        {
            let mut calls = self.calls.lock().expect("calls");
            calls.printer = Some(printer.to_string());
            calls.title = Some(title.to_string());
            calls.attributes = attributes.to_vec();
        }
        self.record(Op::Create);
        self.create.clone()
    }

    fn start_document(&mut self, _job: &ProviderJobId) -> Result<(), MutationFailure> {
        self.record(Op::Start);
        self.start.clone()
    }

    fn write_document(&mut self, chunk: &[u8]) -> Result<(), MutationFailure> {
        self.record(Op::Write);
        let mut calls = self.calls.lock().expect("calls");
        if let Some((limit, failure)) = &self.fail_write_after
            && calls.written.len() as u64 + chunk.len() as u64 > *limit
        {
            return Err(failure.clone());
        }
        calls.written.extend_from_slice(chunk);
        Ok(())
    }

    fn finish_document(&mut self, _job: &ProviderJobId) -> Result<(), MutationFailure> {
        self.record(Op::Finish);
        self.finish.clone()
    }

    fn close_job(&mut self, _job: &ProviderJobId) -> Result<(), MutationFailure> {
        self.record(Op::Close);
        self.close.clone()
    }

    fn cancel_job(&mut self, _job: &ProviderJobId) -> Result<(), MutationFailure> {
        self.record(Op::Cancel);
        self.cancel.clone()
    }

    fn job_status(&mut self, _job: &ProviderJobId) -> ProviderResult<ProviderJobStatus> {
        self.record(Op::Status);
        self.status.clone()
    }
}

/// Shorthand for scripted failures.
pub(super) fn rejected(reason: super::types::JobRejection) -> MutationFailure {
    MutationFailure::Rejected {
        reason,
        detail: "scripted refusal".into(),
    }
}

pub(super) fn no_answer() -> MutationFailure {
    MutationFailure::NoAnswer {
        detail: "scripted lost answer".into(),
    }
}

pub(super) fn not_sent() -> MutationFailure {
    MutationFailure::NotSent(ProviderError::ConnectionFailed {
        detail: "scripted: scheduler unreachable before sending".into(),
    })
}
