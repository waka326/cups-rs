//! The job half of [`CupsProvider`]'s public surface.

use std::time::Duration;

use crate::provider::api::CupsProvider;
use crate::provider::error::ProviderResult;

use super::options::ProviderJobOptions;
use super::types::{
    CancelOutcome, CancelProgress, CreatedJob, ProviderJobHandle, ProviderJobId, ProviderJobStage,
    ProviderJobStatus,
};

impl CupsProvider {
    /// Create an empty, held job on exactly `printer`, never another.
    ///
    /// Every option is encoded before anything is sent; one that cannot be
    /// is an error and no job is created. If the answer does not arrive in
    /// time the error is `JobOutcomeUnknown` carrying the handle, and the job
    /// id is recorded against it when the scheduler's answer comes back —
    /// read it with [`Self::job_stage`] rather than creating again.
    pub fn create_job(
        &self,
        printer: &str,
        title: &str,
        options: &ProviderJobOptions,
        timeout: Duration,
    ) -> ProviderResult<CreatedJob> {
        self.jobs.create_job(printer, title, options, timeout)
    }

    /// Send the job's single PDF document. Never retried; on failure the job
    /// is left open for an explicit [`Self::cancel_job`].
    pub fn submit_pdf_bytes(
        &self,
        handle: ProviderJobHandle,
        document: &[u8],
        timeout: Duration,
    ) -> ProviderResult<()> {
        self.jobs.submit_pdf_bytes(handle, document, timeout)
    }

    /// Finalise the job. Success means CUPS accepted it, not that it printed.
    pub fn close_job(
        &self,
        handle: ProviderJobHandle,
        timeout: Duration,
    ) -> ProviderResult<ProviderJobId> {
        self.jobs.close_job(handle, timeout)
    }

    /// The scheduler's current view of the job.
    pub fn get_job_status(
        &self,
        job: &ProviderJobId,
        timeout: Duration,
    ) -> ProviderResult<ProviderJobStatus> {
        self.jobs.job_status(job, timeout)
    }

    /// Ask the scheduler to cancel the job. Not an undo: it may have printed.
    pub fn cancel_job(
        &self,
        job: &ProviderJobId,
        timeout: Duration,
    ) -> ProviderResult<CancelOutcome> {
        self.jobs.cancel_job(job, timeout)
    }

    /// Where a handle stands, including results that arrived after their
    /// caller timed out.
    pub fn job_stage(&self, handle: ProviderJobHandle) -> ProviderResult<ProviderJobStage> {
        self.jobs.stage(handle)
    }

    /// The most recent cancel request for `job` made through this provider.
    pub fn cancel_progress(&self, job: &ProviderJobId) -> ProviderResult<Option<CancelProgress>> {
        self.jobs.cancel_progress(job)
    }

    /// Stop tracking a handle. Refused while an operation on it is in flight.
    pub fn release_job_handle(&self, handle: ProviderJobHandle) -> ProviderResult<()> {
        self.jobs.release(handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn job_surface_is_plain_shareable_data() {
        // Adding the job lifecycle must not make the provider thread-bound:
        // nothing it holds for jobs is a CUPS pointer.
        assert_send_sync::<CupsProvider>();
        assert_send_sync::<ProviderJobHandle>();
        assert_send_sync::<ProviderJobId>();
        assert_send_sync::<ProviderJobStage>();
        assert_send_sync::<ProviderJobOptions>();
        assert_send_sync::<CreatedJob>();
        assert_send_sync::<CancelProgress>();
    }
}
