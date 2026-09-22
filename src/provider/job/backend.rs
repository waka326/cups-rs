//! The seam between the job state machine and libcups.
//!
//! The lifecycle rules — one document per job, no retry, what a failure at
//! each step means — live in the service, which only talks to this trait. The
//! real implementation calls libcups; tests substitute a scripted fake, so
//! every rule is exercised without a print job ever existing.
//!
//! Each method is one scheduler request (or, for a document, one step of one
//! request). None of them retries, and none of them does more than it says.

use crate::provider::error::ProviderResult;

use super::options::JobAttribute;
use super::types::{JobRejection, ProviderJobId, ProviderJobStatus};

/// How a mutating request failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MutationFailure {
    /// Nothing reached the scheduler: resolving the printer or building the
    /// request failed first. The scheduler's state is unchanged.
    NotSent(crate::provider::error::ProviderError),
    /// The scheduler answered and refused.
    Rejected {
        reason: JobRejection,
        detail: String,
    },
    /// The request may have reached the scheduler, but no answer was read.
    NoAnswer { detail: String },
}

/// What the scheduler said to a successful Create-Job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CreateAccepted {
    pub job_id: u32,
    pub ignored_attributes: Vec<String>,
}

/// Runs on the provider thread only.
///
/// `Send` so the backend can be moved into the thread's closures; it is never
/// used from two threads at once because every call is serialised by the
/// provider thread.
pub(crate) trait JobBackend: Send + 'static {
    /// Send one Create-Job to `printer`, exactly as named.
    fn create_job(
        &mut self,
        printer: &str,
        title: &str,
        attributes: &[JobAttribute],
    ) -> Result<CreateAccepted, MutationFailure>;

    /// Begin the job's single PDF document.
    fn start_document(&mut self, job: &ProviderJobId) -> Result<(), MutationFailure>;

    /// Send the next part of the document started by
    /// [`Self::start_document`].
    fn write_document(&mut self, chunk: &[u8]) -> Result<(), MutationFailure>;

    /// End the document and read the scheduler's verdict on it.
    fn finish_document(&mut self, job: &ProviderJobId) -> Result<(), MutationFailure>;

    /// Send one Close-Job.
    fn close_job(&mut self, job: &ProviderJobId) -> Result<(), MutationFailure>;

    /// Send one Cancel-Job.
    fn cancel_job(&mut self, job: &ProviderJobId) -> Result<(), MutationFailure>;

    /// Read the job's state, confirming it belongs to `job.printer()`.
    fn job_status(&mut self, job: &ProviderJobId) -> ProviderResult<ProviderJobStatus>;
}
