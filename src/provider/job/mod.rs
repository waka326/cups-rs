//! The job lifecycle: one PDF per job, no retry, no lost outcomes.
//!
//! ```text
//! create_job        -> CreatedJob { handle, job }     Create-Job (held)
//! submit_pdf_bytes  -> ()                             Send-Document, last-document = false
//! close_job         -> ProviderJobId                  Close-Job: the job may now print
//! get_job_status    -> ProviderJobStatus              Get-Job-Attributes (read-only)
//! cancel_job        -> CancelOutcome                  Cancel-Job, after confirming the job
//! job_stage         -> ProviderJobStage               this provider's record of a handle
//! cancel_progress   -> Option<CancelProgress>         this provider's record of a cancel
//! ```
//!
//! Every call runs on the provider's single thread, under the same one-deadline
//! rule as the read-only API. What differs is how an unanswered call is
//! reported: a job operation that reached the thread and then ran out of time
//! is `JobOutcomeUnknown`, never a plain `Timeout`, and its eventual result is
//! recorded where [`job_stage`](crate::provider::CupsProvider::job_stage) or
//! [`cancel_progress`](crate::provider::CupsProvider::cancel_progress) can
//! find it.
//!
//! None of these operations confirms that anything was printed.

mod api;
mod backend;
mod cups_backend;
mod handle;
mod options;
mod registry;
mod service;
mod types;
mod validate;

#[cfg(test)]
mod fake;
#[cfg(test)]
mod service_tests;

pub(crate) use cups_backend::CupsJobBackend;
pub use handle::ProviderJobHandle;
pub use options::{ColorMode, JobMedia, JobMediaSize, PrintQuality, ProviderJobOptions, Sides};
pub(crate) use service::JobService;
pub use types::{
    CancelOutcome, CancelProgress, CreatedJob, JobOperation, JobRejection, ProviderJobId,
    ProviderJobStage, ProviderJobStatus, SubmitPhase,
};
