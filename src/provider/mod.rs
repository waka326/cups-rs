//! The Nenga provider contract.
//!
//! A surface over CUPS that keeps IPP types intact, distinguishes
//! "unknown" from "unsupported", expresses media in exact integer units, and
//! runs every CUPS call on one dedicated thread.
//!
//! The older `Vec<String>` accessors elsewhere in this crate remain for
//! compatibility but should not be used for capability decisions: they guess
//! at value types and cannot represent ranges or out-of-band markers.
//!
//! The job lifecycle ([`job`]) creates, submits, closes and cancels print
//! jobs under a stricter contract than the reads: one PDF per job, no retry,
//! and an operation whose outcome is unknown is never reported as a failure.

pub mod api;
pub mod attribute;
mod compat_check;
pub mod error;
pub mod job;
pub mod media;
pub mod value;
pub mod worker;

pub use api::{CupsProvider, PrinterSummary};
pub use attribute::AttributeState;
pub use error::{ProviderError, ProviderResult};
pub use job::{
    CancelOutcome, CancelProgress, ColorMode, CreatedJob, JobMedia, JobMediaSize, JobOperation,
    JobRejection, PrintQuality, ProviderJobHandle, ProviderJobId, ProviderJobOptions,
    ProviderJobStage, ProviderJobStatus, Sides, SubmitPhase,
};
pub use media::{
    Margins, MediaDescriptor, MediaQuery, POSTCARD_LENGTH_UM, POSTCARD_TOLERANCE_UM,
    POSTCARD_WIDTH_UM, ReadyMedia,
};
pub use value::{IppValue, ResolutionUnits};
pub use worker::ProviderHandle;
