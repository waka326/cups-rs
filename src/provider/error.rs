//! Provider error taxonomy.
//!
//! Callers branch on these variants; they never see a raw CUPS status code.
//! Diagnostic detail is carried along for logs, but it is not part of the
//! decision surface — a caller that needed to parse it would be coupled to
//! libcups again.

use std::fmt;

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
            | Self::MediaUnknown { printer, .. } => Some(printer),
            _ => None,
        }
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
