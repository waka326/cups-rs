//! Where each job handle stands, kept apart from any CUPS call.
//!
//! The registry is what makes a late answer useful: an operation running on
//! the provider thread writes its outcome here itself, so the outcome is kept
//! even when the caller that asked for it has already timed out and left.
//!
//! Every transition that starts an operation checks the current stage and
//! moves to an in-flight stage in one step under the lock. Two requests for
//! the same handle therefore cannot both be dispatched, and an invalid
//! sequence is refused before anything is sent.

use std::collections::HashMap;

use crate::provider::error::{ProviderError, ProviderResult};

use super::handle::{HandleIssuer, ProviderJobHandle};
use super::types::{CancelProgress, JobOperation, ProviderJobId, ProviderJobStage};

pub(crate) struct Registry {
    handles: HandleIssuer,
    stages: HashMap<ProviderJobHandle, ProviderJobStage>,
    cancels: HashMap<ProviderJobId, CancelProgress>,
}

impl Registry {
    /// A registry with an identity no other registry in the process has.
    pub fn new() -> ProviderResult<Self> {
        Ok(Self {
            handles: HandleIssuer::new()?,
            stages: HashMap::new(),
            cancels: HashMap::new(),
        })
    }

    /// Refuse a handle this registry did not issue.
    ///
    /// Checked before any lookup, so a handle from another provider is
    /// refused even when its local number matches one of ours. To a caller a
    /// foreign handle and an unknown one are the same thing — neither names a
    /// job here — so both are `UnknownJobHandle`.
    fn check_issuer(&self, handle: ProviderJobHandle) -> ProviderResult<()> {
        if handle.issued_by() == self.handles.issuer() {
            Ok(())
        } else {
            Err(ProviderError::UnknownJobHandle { handle })
        }
    }

    /// The same check for calls the service makes with handles it issued
    /// itself. A mismatch there is a bug in the provider, and is reported as
    /// one rather than ignored.
    fn check_own(&self, handle: ProviderJobHandle, action: &str) -> ProviderResult<()> {
        self.check_issuer(handle)
            .map_err(|_| ProviderError::InternalContractViolation {
                detail: format!("{action} was given {handle}, which this registry did not issue"),
            })
    }

    /// Allocate a handle for a create that is about to be dispatched.
    pub fn begin_create(&mut self) -> ProviderResult<ProviderJobHandle> {
        let handle = self.handles.next()?;
        self.stages.insert(handle, ProviderJobStage::Creating);
        Ok(handle)
    }

    pub fn stage(&self, handle: ProviderJobHandle) -> ProviderResult<ProviderJobStage> {
        self.check_issuer(handle)?;
        self.stages
            .get(&handle)
            .cloned()
            .ok_or(ProviderError::UnknownJobHandle { handle })
    }

    /// `Created` → `Submitting`. One document per job: once a submission has
    /// been attempted, the handle never returns to `Created` — unless the
    /// attempt provably sent nothing, see [`Self::restore`].
    pub fn begin_submit(&mut self, handle: ProviderJobHandle) -> ProviderResult<ProviderJobId> {
        self.advance(handle, JobOperation::Submit, |stage| match stage {
            ProviderJobStage::Created { job } => Some((
                job.clone(),
                ProviderJobStage::Submitting { job: job.clone() },
            )),
            _ => None,
        })
    }

    /// `Submitted` → `Closing`. A job without its document cannot be closed:
    /// the scheduler would release an empty job.
    pub fn begin_close(&mut self, handle: ProviderJobHandle) -> ProviderResult<ProviderJobId> {
        self.advance(handle, JobOperation::Close, |stage| match stage {
            ProviderJobStage::Submitted { job } => {
                Some((job.clone(), ProviderJobStage::Closing { job: job.clone() }))
            }
            _ => None,
        })
    }

    fn advance(
        &mut self,
        handle: ProviderJobHandle,
        operation: JobOperation,
        transition: impl FnOnce(&ProviderJobStage) -> Option<(ProviderJobId, ProviderJobStage)>,
    ) -> ProviderResult<ProviderJobId> {
        self.check_issuer(handle)?;
        let current = self
            .stages
            .get(&handle)
            .ok_or(ProviderError::UnknownJobHandle { handle })?;
        let (job, next) = transition(current).ok_or_else(|| ProviderError::JobStateConflict {
            handle,
            operation,
            stage: current.clone(),
        })?;
        self.stages.insert(handle, next);
        Ok(job)
    }

    /// Record an operation's outcome. Called from the provider thread.
    ///
    /// Only ever called for a handle this registry issued and still tracks
    /// (an in-flight handle cannot be released), so anything else is a bug.
    pub fn settle(
        &mut self,
        handle: ProviderJobHandle,
        stage: ProviderJobStage,
    ) -> ProviderResult<()> {
        self.check_own(handle, "settle")?;
        let slot = self.stages.get_mut(&handle).ok_or_else(|| {
            ProviderError::InternalContractViolation {
                detail: format!("settle found no record for {handle}"),
            }
        })?;
        *slot = stage;
        Ok(())
    }

    /// Put an in-flight stage back to `previous`, but only while the handle
    /// is still in exactly `in_flight`.
    ///
    /// Used when an operation provably sent nothing, so the job is as it was.
    /// The guard means a stage the operation itself already recorded is never
    /// overwritten; that case is expected and is not an error.
    pub fn restore(
        &mut self,
        handle: ProviderJobHandle,
        in_flight: &ProviderJobStage,
        previous: ProviderJobStage,
    ) -> ProviderResult<()> {
        self.check_own(handle, "restore")?;
        if let Some(slot) = self.stages.get_mut(&handle)
            && slot == in_flight
        {
            *slot = previous;
        }
        Ok(())
    }

    /// Forget a handle whose caller never learned it. Discarding one that is
    /// already gone is fine; discarding another registry's is not.
    pub fn discard(&mut self, handle: ProviderJobHandle) -> ProviderResult<()> {
        self.check_own(handle, "discard")?;
        self.stages.remove(&handle);
        Ok(())
    }

    /// Forget a handle on the caller's request. Refused while an operation
    /// is in flight, since its outcome would then have nowhere to go.
    pub fn release(&mut self, handle: ProviderJobHandle) -> ProviderResult<()> {
        let stage = self.stage(handle)?;
        if stage.is_in_flight() {
            let operation = match stage {
                ProviderJobStage::Creating => JobOperation::Create,
                ProviderJobStage::Submitting { .. } => JobOperation::Submit,
                _ => JobOperation::Close,
            };
            return Err(ProviderError::JobStateConflict {
                handle,
                operation,
                stage,
            });
        }
        self.stages.remove(&handle);
        Ok(())
    }

    #[cfg(test)]
    pub fn tracked_handles(&self) -> usize {
        self.stages.len()
    }

    /// Mark a cancel for `job` as in flight, returning what it replaces.
    pub fn begin_cancel(&mut self, job: &ProviderJobId) -> ProviderResult<Option<CancelProgress>> {
        if matches!(self.cancels.get(job), Some(CancelProgress::InFlight)) {
            return Err(ProviderError::JobOperationInProgress {
                job: job.clone(),
                operation: JobOperation::Cancel,
            });
        }
        Ok(self.cancels.insert(job.clone(), CancelProgress::InFlight))
    }

    pub fn finish_cancel(&mut self, job: &ProviderJobId, progress: CancelProgress) {
        self.cancels.insert(job.clone(), progress);
    }

    /// Undo [`Self::begin_cancel`] for a cancel that was never dispatched.
    pub fn restore_cancel(&mut self, job: &ProviderJobId, previous: Option<CancelProgress>) {
        if !matches!(self.cancels.get(job), Some(CancelProgress::InFlight)) {
            return;
        }
        match previous {
            Some(progress) => {
                self.cancels.insert(job.clone(), progress);
            }
            None => {
                self.cancels.remove(job);
            }
        }
    }

    pub fn cancel_progress(&self, job: &ProviderJobId) -> Option<CancelProgress> {
        self.cancels.get(job).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> ProviderJobId {
        ProviderJobId::new("Printer", 12).expect("valid")
    }

    fn created(registry: &mut Registry) -> ProviderJobHandle {
        let handle = registry.begin_create().expect("handle");
        registry
            .settle(handle, ProviderJobStage::Created { job: job() })
            .expect("own handle");
        handle
    }

    #[test]
    fn handles_are_unique() {
        let mut registry = Registry::new().expect("registry");
        let first = registry.begin_create().expect("handle");
        let second = registry.begin_create().expect("handle");
        assert_ne!(first, second);
    }

    #[test]
    fn submit_requires_a_created_job() {
        let mut registry = Registry::new().expect("registry");
        let handle = registry.begin_create().expect("handle");
        assert!(matches!(
            registry.begin_submit(handle),
            Err(ProviderError::JobStateConflict {
                operation: JobOperation::Submit,
                stage: ProviderJobStage::Creating,
                ..
            })
        ));
    }

    #[test]
    fn a_second_submit_is_refused_while_the_first_is_in_flight() {
        let mut registry = Registry::new().expect("registry");
        let handle = created(&mut registry);
        registry.begin_submit(handle).expect("first submit");
        assert!(matches!(
            registry.begin_submit(handle),
            Err(ProviderError::JobStateConflict { .. })
        ));
    }

    #[test]
    fn restore_never_overwrites_a_recorded_outcome() {
        let mut registry = Registry::new().expect("registry");
        let handle = created(&mut registry);
        registry.begin_submit(handle).expect("submit");
        registry
            .settle(handle, ProviderJobStage::Submitted { job: job() })
            .expect("own handle");

        registry
            .restore(
                handle,
                &ProviderJobStage::Submitting { job: job() },
                ProviderJobStage::Created { job: job() },
            )
            .expect("own handle");
        assert_eq!(
            registry.stage(handle).expect("known"),
            ProviderJobStage::Submitted { job: job() }
        );
    }

    #[test]
    fn close_requires_the_document() {
        let mut registry = Registry::new().expect("registry");
        let handle = created(&mut registry);
        assert!(matches!(
            registry.begin_close(handle),
            Err(ProviderError::JobStateConflict {
                operation: JobOperation::Close,
                ..
            })
        ));
    }

    #[test]
    fn release_is_refused_while_in_flight() {
        let mut registry = Registry::new().expect("registry");
        let handle = registry.begin_create().expect("handle");
        assert!(registry.release(handle).is_err());
        registry
            .settle(handle, ProviderJobStage::Created { job: job() })
            .expect("own handle");
        registry.release(handle).expect("released");
        assert!(matches!(
            registry.stage(handle),
            Err(ProviderError::UnknownJobHandle { .. })
        ));
    }

    #[test]
    fn one_cancel_in_flight_per_job() {
        let mut registry = Registry::new().expect("registry");
        assert_eq!(registry.begin_cancel(&job()).expect("first"), None);
        assert!(matches!(
            registry.begin_cancel(&job()),
            Err(ProviderError::JobOperationInProgress { .. })
        ));
        registry.restore_cancel(&job(), None);
        assert_eq!(registry.cancel_progress(&job()), None);
    }

    #[test]
    fn every_handle_taking_method_refuses_a_foreign_handle() {
        // Same local sequence in both registries: the collision the old
        // handle could not tell apart.
        let mut ours = Registry::new().expect("registry");
        let mut theirs = Registry::new().expect("registry");
        let own = created(&mut ours);
        let foreign = created(&mut theirs);

        let refused = |result: ProviderResult<ProviderJobId>| {
            matches!(result, Err(ProviderError::UnknownJobHandle { .. }))
        };
        assert!(matches!(
            ours.stage(foreign),
            Err(ProviderError::UnknownJobHandle { .. })
        ));
        assert!(refused(ours.begin_submit(foreign)));
        assert!(refused(ours.begin_close(foreign)));
        assert!(matches!(
            ours.release(foreign),
            Err(ProviderError::UnknownJobHandle { .. })
        ));

        // Internal calls with a foreign handle are bugs, reported as such
        // rather than silently ignored.
        let bug = |result: ProviderResult<()>| {
            matches!(result, Err(ProviderError::InternalContractViolation { .. }))
        };
        assert!(bug(ours.settle(foreign, ProviderJobStage::CreateNotSent)));
        assert!(bug(ours.restore(
            foreign,
            &ProviderJobStage::Created { job: job() },
            ProviderJobStage::CreateNotSent
        )));
        assert!(bug(ours.discard(foreign)));

        // None of that touched our own job.
        assert_eq!(
            ours.stage(own).expect("ours"),
            ProviderJobStage::Created { job: job() }
        );
        assert_eq!(ours.tracked_handles(), 1);
    }
}
