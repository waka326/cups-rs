//! The Nenga provider contract.
//!
//! A read-only surface over CUPS that keeps IPP types intact, distinguishes
//! "unknown" from "unsupported", expresses media in exact integer units, and
//! runs every CUPS call on one dedicated thread.
//!
//! The older `Vec<String>` accessors elsewhere in this crate remain for
//! compatibility but should not be used for capability decisions: they guess
//! at value types and cannot represent ranges or out-of-band markers.
//!
//! Nothing in this module creates, submits or cancels a print job.

pub mod api;
pub mod attribute;
pub mod error;
pub mod media;
pub mod value;
pub mod worker;

pub use api::{CupsProvider, PrinterSummary};
pub use attribute::AttributeState;
pub use error::{ProviderError, ProviderResult};
pub use media::{
    Margins, MediaDescriptor, MediaQuery, POSTCARD_LENGTH_UM, POSTCARD_TOLERANCE_UM,
    POSTCARD_WIDTH_UM, ReadyMedia,
};
pub use value::{IppValue, ResolutionUnits};
pub use worker::ProviderHandle;
