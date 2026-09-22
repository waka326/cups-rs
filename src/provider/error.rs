//! Provider error taxonomy.
//!
//! Callers branch on these variants; they never see a raw CUPS status code.
//! Diagnostic detail is carried along for logs, but it is not part of the
//! decision surface — a caller that needed to parse it would be coupled to
//! libcups again.

use std::fmt;

use super::job::{
    JobOperation, JobRejection, ProviderJobHandle, ProviderJobId, ProviderJobStage, SubmitPhase,
};

/// Why a provider operation did not produce an answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderError {
    /// The build or runtime environment cannot support CUPS at all.
    Environment { detail: String },
    /// No destination with that name exists.
    PrinterNotFound { printer: String },
    /// The destination exists but its state prevents printing.
    PrinterNotReady { printer: String, detail: String },
    /// The destination is not accepting jobs.
    PrinterNotAccepting { printer: String },
    /// The capability could not be determined.
    ///
    /// Not the same as [`Self::CapabilityUnsupported`]: this means we do not
    /// know, not that the printer said no.
    CapabilityUnknown { printer: String, attribute: String },
    /// The printer explicitly reported the capability as unsupported.
    CapabilityUnsupported { printer: String, attribute: String },
    /// No media matching the request was found.
    MediaUnknown { printer: String, detail: String },
    /// The CUPS scheduler could not be reached.
    ConnectionFailed { detail: String },
    /// The operation did not finish within its deadline.
    ///
    /// The underlying libcups call is still running; see the threading
    /// contract. Retrying immediately will block until it returns.
    Timeout { operation: String },
    /// The provider violated one of its own invariants. A bug.
    InternalContractViolation { detail: String },

    // --- Job lifecycle ------------------------------------------------------
    //
    // Job errors fall into four groups, and a caller must be able to tell them
    // apart without reading `detail`:
    //
    // - rejected locally, nothing sent: `InvalidJobRequest`, `JobStateConflict`,
    //   `JobOperationInProgress`, `UnknownJobHandle`, and `Timeout` from a job
    //   operation (it expired before dispatch);
    // - the scheduler answered and refused: `JobCreateFailed`,
    //   `JobSubmitFailed`, `JobCloseFailed`, `JobCancelFailed`;
    // - the outcome is not known: `JobOutcomeUnknown`. Never a safe retry;
    // - a status question could not be answered: `JobNotFound`,
    //   `JobStatusQueryFailed`, `JobDestinationMismatch`.
    /// The request was malformed and was rejected before anything was sent.
    InvalidJobRequest { detail: String },
    /// The operation is not valid in the job's current stage. Nothing was
    /// sent.
    JobStateConflict {
        handle: ProviderJobHandle,
        operation: JobOperation,
        stage: ProviderJobStage,
    },
    /// An earlier request of the same kind for this job is dispatched and
    /// unanswered. Nothing was sent.
    JobOperationInProgress {
        job: ProviderJobId,
        operation: JobOperation,
    },
    /// The handle was not issued by this provider, or was released.
    UnknownJobHandle { handle: ProviderJobHandle },
    /// The scheduler refused to create the job. No job exists.
    JobCreateFailed {
        printer: String,
        reason: JobRejection,
        detail: String,
    },
    /// The document was not accepted. The job exists and is still open.
    ///
    /// `reason` is present when the scheduler answered with a refusal, and
    /// absent when the connection failed before the document was complete —
    /// a scheduler only attaches a document once the whole request has
    /// arrived, so either way the document was not accepted.
    JobSubmitFailed {
        job: ProviderJobId,
        phase: SubmitPhase,
        bytes_sent: u64,
        reason: Option<JobRejection>,
        detail: String,
    },
    /// The scheduler refused to close the job. It is still open.
    JobCloseFailed {
        job: ProviderJobId,
        reason: JobRejection,
        detail: String,
    },
    /// The scheduler refused to cancel the job.
    JobCancelFailed {
        job: ProviderJobId,
        reason: JobRejection,
        detail: String,
    },
    /// A job operation was dispatched and whether it took effect is not
    /// known.
    ///
    /// Not a failure and not safe to retry: repeating a create may produce a
    /// second job and a second print. `in_flight` means the caller gave up
    /// while the operation was still running; its result will be recorded
    /// against `handle` (or, for a cancel, against `job`) when it returns.
    /// Otherwise the operation finished without an answer anyone could read.
    JobOutcomeUnknown {
        operation: JobOperation,
        handle: Option<ProviderJobHandle>,
        job: Option<ProviderJobId>,
        in_flight: bool,
        detail: String,
    },
    /// The scheduler says the job does not exist. Not the same as completed:
    /// finished jobs are listed until the scheduler's history is purged.
    JobNotFound { job: ProviderJobId },
    /// The job's status could not be determined. Says nothing about the job.
    JobStatusQueryFailed { job: ProviderJobId, detail: String },
    /// The scheduler has a job with this id, but on a different destination.
    /// Nothing was changed.
    JobDestinationMismatch {
        job: ProviderJobId,
        reported_printer: String,
    },
}

impl ProviderError {
    /// The printer this error concerns, when it concerns one.
    pub fn printer(&self) -> Option<&str> {
        match self {
            Self::PrinterNotFound { printer }
            | Self::PrinterNotReady { printer, .. }
            | Self::PrinterNotAccepting { printer }
            | Self::CapabilityUnknown { printer, .. }
            | Self::CapabilityUnsupported { printer, .. }
            | Self::MediaUnknown { printer, .. }
            | Self::JobCreateFailed { printer, .. } => Some(printer),
            Self::JobSubmitFailed { job, .. }
            | Self::JobCloseFailed { job, .. }
            | Self::JobCancelFailed { job, .. }
            | Self::JobOperationInProgress { job, .. }
            | Self::JobNotFound { job }
            | Self::JobStatusQueryFailed { job, .. }
            | Self::JobDestinationMismatch { job, .. } => Some(job.printer()),
            Self::JobOutcomeUnknown { job, .. } => job.as_ref().map(ProviderJobId::printer),
            _ => None,
        }
    }

    /// Whether a job operation was dispatched with an outcome nobody could
    /// observe. Such an error must never be treated as a retriable failure.
    pub fn is_outcome_unknown(&self) -> bool {
        matches!(self, Self::JobOutcomeUnknown { .. })
    }

    /// Whether the cause is something a person at the printer could resolve
    /// (load paper, clear a jam, enable the queue).
    ///
    /// This classifies the error; it does not decide what the caller does
    /// about it.
    pub fn is_operator_actionable(&self) -> bool {
        matches!(
            self,
            Self::PrinterNotReady { .. }
                | Self::PrinterNotAccepting { .. }
                | Self::CapabilityUnknown { .. }
                | Self::MediaUnknown { .. }
        )
    }
}

impl fmt::Display for ProviderError {
    // Diagnostic text for logs. Product-facing wording is the caller's to
    // write; this is deliberately plain and untranslated.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Environment { detail } => write!(f, "CUPS environment unusable: {detail}"),
            Self::PrinterNotFound { printer } => write!(f, "printer not found: {printer}"),
            Self::PrinterNotReady { printer, detail } => {
                write!(f, "printer {printer} not ready: {detail}")
            }
            Self::PrinterNotAccepting { printer } => {
                write!(f, "printer {printer} is not accepting jobs")
            }
            Self::CapabilityUnknown { printer, attribute } => {
                write!(f, "capability {attribute} unknown for {printer}")
            }
            Self::CapabilityUnsupported { printer, attribute } => {
                write!(f, "capability {attribute} unsupported by {printer}")
            }
            Self::MediaUnknown { printer, detail } => {
                write!(f, "media unknown for {printer}: {detail}")
            }
            Self::ConnectionFailed { detail } => write!(f, "CUPS connection failed: {detail}"),
            Self::Timeout { operation } => write!(f, "operation timed out: {operation}"),
            Self::InternalContractViolation { detail } => {
                write!(f, "provider contract violation: {detail}")
            }
            Self::InvalidJobRequest { detail } => write!(f, "invalid job request: {detail}"),
            Self::JobStateConflict {
                handle,
                operation,
                stage,
            } => write!(f, "cannot {operation} {handle} in stage {stage:?}"),
            Self::JobOperationInProgress { job, operation } => {
                write!(f, "a {operation} request for job {job} is still in flight")
            }
            Self::UnknownJobHandle { handle } => write!(f, "unknown job handle {handle}"),
            Self::JobCreateFailed {
                printer,
                reason,
                detail,
            } => write!(
                f,
                "job creation on {printer} refused ({reason:?}): {detail}"
            ),
            Self::JobSubmitFailed {
                job,
                phase,
                bytes_sent,
                reason,
                detail,
            } => write!(
                f,
                "document for job {job} not accepted at {phase:?} after {bytes_sent} bytes \
                 ({reason:?}): {detail}"
            ),
            Self::JobCloseFailed {
                job,
                reason,
                detail,
            } => write!(f, "close of job {job} refused ({reason:?}): {detail}"),
            Self::JobCancelFailed {
                job,
                reason,
                detail,
            } => write!(f, "cancel of job {job} refused ({reason:?}): {detail}"),
            Self::JobOutcomeUnknown {
                operation,
                handle,
                job,
                in_flight,
                detail,
            } => write!(
                f,
                "outcome of {operation} unknown (handle {handle:?}, job {job:?}, \
                 in flight {in_flight}): {detail}"
            ),
            Self::JobNotFound { job } => write!(f, "job {job} not found"),
            Self::JobStatusQueryFailed { job, detail } => {
                write!(f, "status of job {job} could not be read: {detail}")
            }
            Self::JobDestinationMismatch {
                job,
                reported_printer,
            } => write!(
                f,
                "job {job} belongs to {reported_printer}, not the expected printer"
            ),
        }
    }
}

impl std::error::Error for ProviderError {}

pub type ProviderResult<T> = Result<T, ProviderError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_is_not_unsupported() {
        // Conflating these would turn "we could not tell" into "the printer
        // refused", which is a stronger claim than the evidence supports.
        let unknown = ProviderError::CapabilityUnknown {
            printer: "Canon_TS5400_series".into(),
            attribute: "media-source".into(),
        };
        let unsupported = ProviderError::CapabilityUnsupported {
            printer: "Canon_TS5400_series".into(),
            attribute: "media-source".into(),
        };
        assert_ne!(unknown, unsupported);
        assert!(unknown.is_operator_actionable());
        assert!(!unsupported.is_operator_actionable());
    }

    #[test]
    fn carries_printer_identity() {
        let err = ProviderError::PrinterNotFound {
            printer: "EPSON_EW_456A_Series".into(),
        };
        assert_eq!(err.printer(), Some("EPSON_EW_456A_Series"));
        assert_eq!(
            ProviderError::Timeout {
                operation: "list_printers".into()
            }
            .printer(),
            None
        );
    }

    #[test]
    fn transport_failures_are_not_operator_actionable() {
        assert!(
            !ProviderError::ConnectionFailed {
                detail: "no scheduler".into()
            }
            .is_operator_actionable()
        );
        assert!(
            !ProviderError::Timeout {
                operation: "get_supported_media".into()
            }
            .is_operator_actionable()
        );
        assert!(
            !ProviderError::InternalContractViolation {
                detail: "worker gone".into()
            }
            .is_operator_actionable()
        );
    }
}
