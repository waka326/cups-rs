//! Provider-owned job identities and states.
//!
//! None of these wrap a CUPS object. They are plain data, safe to hold on any
//! thread, and carry no raw status numbers a caller could branch on.

use std::fmt;
use std::num::NonZeroU32;

use crate::provider::error::{ProviderError, ProviderResult};

/// This provider's name for one requested job, allocated before anything is
/// sent to CUPS.
///
/// It exists so a job can be followed even when the scheduler's answer never
/// reached the caller: a create that times out after dispatch still has a
/// handle, and the job id it eventually produces is recorded against it.
///
/// A handle is meaningful only to the provider that issued it. It is not a
/// pointer, and it does not survive the provider; persist the
/// [`ProviderJobId`] instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProviderJobHandle(u64);

impl ProviderJobHandle {
    pub(crate) fn from_raw(value: u64) -> Self {
        Self(value)
    }
}

impl fmt::Display for ProviderJobHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "job-handle-{}", self.0)
    }
}

/// A CUPS job, identified by the scheduler's job id together with the
/// destination it was created on.
///
/// CUPS job ids are unique across the scheduler, and cupsd's Cancel-Job looks
/// a job up by id alone without checking the printer named in the request.
/// The destination is kept anyway so the provider can confirm, before acting,
/// that the job it is about to touch is still the one on this printer — and so
/// a caller never has to recover the destination from CUPS data.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ProviderJobId {
    printer: String,
    job_id: NonZeroU32,
}

impl ProviderJobId {
    /// Rebuild an id a caller persisted earlier.
    ///
    /// Rejects the values CUPS gives special meaning to: job id 0 means "the
    /// current job on this printer" to Cancel-Job, which would cancel whatever
    /// happens to be printing.
    pub fn new(printer: impl Into<String>, job_id: u32) -> ProviderResult<Self> {
        let printer = printer.into();
        if printer.is_empty() || printer.contains('\0') {
            return Err(ProviderError::InvalidJobRequest {
                detail: "printer name must be non-empty and contain no NUL".into(),
            });
        }
        let job_id = NonZeroU32::new(job_id)
            .filter(|id| i32::try_from(id.get()).is_ok())
            .ok_or_else(|| ProviderError::InvalidJobRequest {
                detail: format!("job id {job_id} is not a valid CUPS job id"),
            })?;
        Ok(Self { printer, job_id })
    }

    pub fn printer(&self) -> &str {
        &self.printer
    }

    /// The scheduler's job id. Always positive.
    pub fn scheduler_job_id(&self) -> u32 {
        self.job_id.get()
    }

    /// The id as libcups takes it. Construction guarantees it fits.
    pub(crate) fn as_cups_id(&self) -> i32 {
        i32::try_from(self.job_id.get()).unwrap_or(i32::MAX)
    }
}

impl fmt::Display for ProviderJobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.printer, self.job_id)
    }
}

/// A job's state as the scheduler reports it.
///
/// This is a scheduler observation. `Completed` means CUPS finished
/// processing the job; it is not evidence that a sheet came out of the
/// printer correctly. Confirming physical output is a human decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderJobStatus {
    Pending,
    Held,
    Processing,
    Stopped,
    Canceled,
    Aborted,
    Completed,
    /// The scheduler answered with a state this provider does not recognise,
    /// or with none. Never folded into a known state.
    Unknown,
}

impl ProviderJobStatus {
    /// Map an IPP `job-state` value (RFC 8011 §5.3.7).
    ///
    /// The numbers are the IPP enum, fixed by the standard and identical in
    /// CUPS 2 and 3; they stay inside the provider.
    pub(crate) fn from_ipp_job_state(value: i32) -> Self {
        match value {
            3 => Self::Pending,
            4 => Self::Held,
            5 => Self::Processing,
            6 => Self::Stopped,
            7 => Self::Canceled,
            8 => Self::Aborted,
            9 => Self::Completed,
            _ => Self::Unknown,
        }
    }

    /// Whether the scheduler will do nothing further with the job.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Canceled | Self::Aborted | Self::Completed)
    }
}

/// Which lifecycle operation an error or state refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JobOperation {
    Create,
    Submit,
    Close,
    Cancel,
}

impl fmt::Display for JobOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Create => "create",
            Self::Submit => "submit",
            Self::Close => "close",
            Self::Cancel => "cancel",
        })
    }
}

/// How far a document submission got before it stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitPhase {
    /// The Send-Document request could not be started. No document data was
    /// sent.
    Start,
    /// Sending document data failed part-way.
    Write,
    /// All data was sent, but the scheduler's verdict on the document was a
    /// refusal or never arrived.
    Finish,
}

/// Why the scheduler refused a request.
///
/// Derived from the IPP status class, without exposing the number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobRejection {
    /// The printer or job does not exist on the scheduler.
    NotFound,
    /// The printer is not accepting jobs.
    NotAccepting,
    /// The requesting user may not do this.
    NotAuthorized,
    /// The request is valid but not possible in the job's current state.
    NotPossible,
    /// The request, an attribute or a value was not acceptable.
    BadRequest,
    /// The document format is not supported.
    DocumentFormatNotSupported,
    /// Any other refusal.
    Other,
}

/// Where a job stands in this provider's lifecycle.
///
/// The `*ing` states mean an operation was dispatched and its result has not
/// been recorded yet — including when its caller has already given up. They
/// change to a final state when the operation returns, whenever that is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderJobStage {
    /// Create-Job dispatched; no answer recorded yet.
    Creating,
    /// The create never left the provider (the printer could not be resolved,
    /// or the request could not be built). No job exists.
    CreateNotSent,
    /// The scheduler refused to create the job. No job exists.
    CreateFailed { reason: JobRejection },
    /// The create request may have reached the scheduler, but no answer was
    /// read. A job may exist and its id is not known.
    CreateOutcomeUnknown,
    /// The job exists and holds no document yet.
    Created { job: ProviderJobId },
    /// The document is being sent.
    Submitting { job: ProviderJobId },
    /// The document was not accepted. The job still exists, is not closed,
    /// and will not print unless it is closed — or until the scheduler's
    /// multiple-operation timeout releases it. It must be cancelled
    /// explicitly; the provider does not do that on its own.
    SubmitFailed {
        job: ProviderJobId,
        phase: SubmitPhase,
        bytes_sent: u64,
    },
    /// All document data was sent but whether the scheduler accepted it is
    /// unknown.
    SubmitOutcomeUnknown { job: ProviderJobId },
    /// The one document was accepted. The job is not yet closed.
    Submitted { job: ProviderJobId },
    /// Close-Job dispatched; no answer recorded yet.
    Closing { job: ProviderJobId },
    /// The scheduler refused to close the job. It remains open.
    CloseFailed {
        job: ProviderJobId,
        reason: JobRejection,
    },
    /// Close-Job may or may not have taken effect.
    CloseOutcomeUnknown { job: ProviderJobId },
    /// The scheduler accepted the finished job. It may now print. This is not
    /// confirmation that it printed.
    Closed { job: ProviderJobId },
}

impl ProviderJobStage {
    /// The CUPS job, once one is known to exist.
    pub fn job(&self) -> Option<&ProviderJobId> {
        match self {
            Self::Creating
            | Self::CreateNotSent
            | Self::CreateFailed { .. }
            | Self::CreateOutcomeUnknown => None,
            Self::Created { job }
            | Self::Submitting { job }
            | Self::SubmitFailed { job, .. }
            | Self::SubmitOutcomeUnknown { job }
            | Self::Submitted { job }
            | Self::Closing { job }
            | Self::CloseFailed { job, .. }
            | Self::CloseOutcomeUnknown { job }
            | Self::Closed { job } => Some(job),
        }
    }

    /// Whether an operation is dispatched and still unanswered.
    pub fn is_in_flight(&self) -> bool {
        matches!(
            self,
            Self::Creating | Self::Submitting { .. } | Self::Closing { .. }
        )
    }
}

/// A job the scheduler created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatedJob {
    pub handle: ProviderJobHandle,
    pub job: ProviderJobId,
    /// Attributes the scheduler reported as ignored or substituted.
    ///
    /// The job exists regardless — it cannot be un-created by reporting an
    /// error — so this is surfaced rather than hidden. A caller that required
    /// every option honoured can cancel the job.
    pub ignored_attributes: Vec<String>,
}

/// What a cancel request achieved.
///
/// Cancelling is not undoing: a job may already have printed in whole or in
/// part before the scheduler acted on the request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelOutcome {
    /// The scheduler accepted the cancel request.
    CancelRequested,
    /// The job had already finished; nothing was sent.
    AlreadyTerminal(ProviderJobStatus),
}

/// The state of the most recent cancel request for a job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CancelProgress {
    /// Dispatched; no answer recorded yet.
    InFlight,
    /// Answered.
    Finished(Result<CancelOutcome, ProviderError>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_id_rejects_values_cups_treats_specially() {
        // Job id 0 would make Cancel-Job act on the printer's current job.
        assert!(ProviderJobId::new("Printer", 0).is_err());
        assert!(ProviderJobId::new("Printer", u32::MAX).is_err());
        assert!(ProviderJobId::new("", 5).is_err());
        assert!(ProviderJobId::new("Pri\0nter", 5).is_err());

        let id = ProviderJobId::new("Canon_TS5400_series", 472).expect("valid");
        assert_eq!(id.printer(), "Canon_TS5400_series");
        assert_eq!(id.scheduler_job_id(), 472);
        assert_eq!(id.as_cups_id(), 472);
    }

    #[test]
    fn maps_every_ipp_job_state() {
        let expected = [
            (3, ProviderJobStatus::Pending),
            (4, ProviderJobStatus::Held),
            (5, ProviderJobStatus::Processing),
            (6, ProviderJobStatus::Stopped),
            (7, ProviderJobStatus::Canceled),
            (8, ProviderJobStatus::Aborted),
            (9, ProviderJobStatus::Completed),
        ];
        for (value, status) in expected {
            assert_eq!(ProviderJobStatus::from_ipp_job_state(value), status);
        }
        for value in [0, 1, 2, 10, -1, i32::MAX] {
            assert_eq!(
                ProviderJobStatus::from_ipp_job_state(value),
                ProviderJobStatus::Unknown
            );
        }
    }

    #[test]
    fn job_state_numbers_match_the_bindings() {
        // The mapping hard-codes the IPP enum; prove it agrees with libcups.
        use crate::bindings::*;
        let pairs = [
            (ipp_jstate_e_IPP_JSTATE_PENDING, ProviderJobStatus::Pending),
            (ipp_jstate_e_IPP_JSTATE_HELD, ProviderJobStatus::Held),
            (
                ipp_jstate_e_IPP_JSTATE_PROCESSING,
                ProviderJobStatus::Processing,
            ),
            (ipp_jstate_e_IPP_JSTATE_STOPPED, ProviderJobStatus::Stopped),
            (
                ipp_jstate_e_IPP_JSTATE_CANCELED,
                ProviderJobStatus::Canceled,
            ),
            (ipp_jstate_e_IPP_JSTATE_ABORTED, ProviderJobStatus::Aborted),
            (
                ipp_jstate_e_IPP_JSTATE_COMPLETED,
                ProviderJobStatus::Completed,
            ),
        ];
        for (raw, status) in pairs {
            assert_eq!(ProviderJobStatus::from_ipp_job_state(raw as i32), status);
        }
    }

    #[test]
    fn only_finished_states_are_terminal() {
        assert!(ProviderJobStatus::Completed.is_terminal());
        assert!(ProviderJobStatus::Canceled.is_terminal());
        assert!(ProviderJobStatus::Aborted.is_terminal());
        for status in [
            ProviderJobStatus::Pending,
            ProviderJobStatus::Held,
            ProviderJobStatus::Processing,
            ProviderJobStatus::Stopped,
            ProviderJobStatus::Unknown,
        ] {
            assert!(!status.is_terminal(), "{status:?}");
        }
    }
}
