//! Checks done before anything is sent.
//!
//! Anything refused here is refused with nothing mutated, so the caller may
//! correct the input and try again without risking a second job.

use crate::provider::error::{ProviderError, ProviderResult};

/// Largest document accepted, matching the crate's existing guard.
const MAX_DOCUMENT_BYTES: usize = 100 * 1024 * 1024;

/// How far into the data the `%PDF-` marker may appear. Readers tolerate a
/// short preamble before it, so requiring it at byte 0 would refuse real PDFs.
const PDF_HEADER_WINDOW: usize = 1024;

/// IPP `name` values are at most 255 octets (RFC 8011 §5.1.3).
const MAX_TITLE_BYTES: usize = 255;

pub(crate) fn validate_printer(printer: &str) -> ProviderResult<()> {
    if printer.is_empty() || printer.contains('\0') {
        return Err(invalid(
            "printer name must be non-empty and contain no NUL".into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_title(title: &str) -> ProviderResult<()> {
    if title.contains('\0') || title.len() > MAX_TITLE_BYTES {
        return Err(invalid(format!(
            "job title must contain no NUL and be at most {MAX_TITLE_BYTES} bytes"
        )));
    }
    Ok(())
}

/// Structural checks only.
///
/// The `%PDF-` marker catches the wrong buffer being passed — a PNG, an
/// empty file, text. It is not validation: a file can carry the marker and
/// still be broken, and nothing here parses or alters the document.
pub(crate) fn validate_pdf(document: &[u8]) -> ProviderResult<()> {
    if document.is_empty() {
        return Err(invalid("document is empty".into()));
    }
    if document.len() > MAX_DOCUMENT_BYTES {
        return Err(invalid(format!(
            "document is {} bytes, over the {MAX_DOCUMENT_BYTES}-byte limit",
            document.len()
        )));
    }
    let window = &document[..document.len().min(PDF_HEADER_WINDOW)];
    if !window.windows(5).any(|bytes| bytes == b"%PDF-") {
        return Err(invalid("document has no %PDF- header".into()));
    }
    Ok(())
}

fn invalid(detail: String) -> ProviderError {
    ProviderError::InvalidJobRequest { detail }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_pdf_header_after_a_short_preamble() {
        assert!(validate_pdf(b"%PDF-1.7\n").is_ok());
        let mut preamble = vec![b' '; 100];
        preamble.extend_from_slice(b"%PDF-1.4");
        assert!(validate_pdf(&preamble).is_ok());
    }

    #[test]
    fn refuses_what_is_plainly_not_a_pdf() {
        assert!(validate_pdf(b"").is_err());
        assert!(validate_pdf(b"\x89PNG\r\n\x1a\n").is_err());
        let mut late = vec![b' '; PDF_HEADER_WINDOW];
        late.extend_from_slice(b"%PDF-1.4");
        assert!(validate_pdf(&late).is_err());
    }

    #[test]
    fn titles_and_printers_must_be_c_safe() {
        assert!(validate_printer("").is_err());
        assert!(validate_printer("A\0B").is_err());
        assert!(validate_printer("Canon_TS5400_series").is_ok());
        assert!(validate_title("年賀状 宛名面").is_ok());
        assert!(validate_title("a\0b").is_err());
        assert!(validate_title(&"x".repeat(MAX_TITLE_BYTES + 1)).is_err());
    }
}
