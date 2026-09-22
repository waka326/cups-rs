//! Provider-owned job options and their IPP encoding.
//!
//! Options become typed IPP attributes here and are handed to the backend as
//! such. They never pass through CUPS' `name=value` option strings, where a
//! comma splits one value into several and an unknown name is dropped
//! without complaint.
//!
//! Nothing here decides what a Nenga job should look like. Whether a postcard
//! prints in colour or borderless is the caller's policy; this module only
//! guarantees that whatever the caller asked for is sent exactly, or refused
//! before a job exists.

use std::num::NonZeroU32;

use crate::provider::error::{ProviderError, ProviderResult};
use crate::provider::media::{Margins, MediaDescriptor};

/// Upper bound for an IPP keyword or name value (RFC 8011 §5.1.4).
const MAX_KEYWORD_BYTES: usize = 255;

/// CUPS stores media dimensions in hundredths of a millimetre.
const UM_PER_HUNDREDTH_MM: u32 = 10;

/// `sides`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sides {
    OneSided,
    TwoSidedLongEdge,
    TwoSidedShortEdge,
}

impl Sides {
    fn keyword(self) -> &'static str {
        match self {
            Self::OneSided => "one-sided",
            Self::TwoSidedLongEdge => "two-sided-long-edge",
            Self::TwoSidedShortEdge => "two-sided-short-edge",
        }
    }
}

/// `print-color-mode`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColorMode {
    Auto,
    Color,
    Monochrome,
    /// Another keyword from the printer's `print-color-mode-supported`.
    Other(String),
}

impl ColorMode {
    fn keyword(&self) -> &str {
        match self {
            Self::Auto => "auto",
            Self::Color => "color",
            Self::Monochrome => "monochrome",
            Self::Other(keyword) => keyword,
        }
    }
}

/// `print-quality`. An IPP enum, sent as one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrintQuality {
    Draft,
    Normal,
    High,
}

impl PrintQuality {
    fn ipp_enum(self) -> i32 {
        match self {
            Self::Draft => 3,
            Self::Normal => 4,
            Self::High => 5,
        }
    }
}

/// The sheet to print on, as the printer described it.
///
/// Built from a [`MediaDescriptor`] the read-only API returned, so the size
/// sent is the size the printer reported rather than one reconstructed from a
/// name. Margins are sent too: CUPS picks the borderless variant of a size
/// from zero margins, not from the name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobMediaSize {
    canonical_name: String,
    width_um: u32,
    length_um: u32,
    margins: Margins,
}

impl JobMediaSize {
    pub fn from_descriptor(descriptor: &MediaDescriptor) -> ProviderResult<Self> {
        if !descriptor.has_known_dimensions() {
            return Err(invalid(format!(
                "media {} has no reported dimensions",
                descriptor.canonical_name
            )));
        }
        Ok(Self {
            canonical_name: descriptor.canonical_name.clone(),
            width_um: descriptor.width_um,
            length_um: descriptor.length_um,
            margins: descriptor.margins,
        })
    }

    pub fn canonical_name(&self) -> &str {
        &self.canonical_name
    }
}

/// Media selection: size, type and source.
///
/// `media_type` and `media_source` take the printer's own keywords, vendor
/// values included, and pass them through unchanged.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JobMedia {
    pub size: Option<JobMediaSize>,
    pub media_type: Option<String>,
    pub media_source: Option<String>,
}

/// Options for one job.
///
/// `copies` is required: Nenga must never inherit a copy count from printer
/// defaults. Every other field left `None` is not sent, and the printer's
/// default applies; that is the only way an option is ever left out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderJobOptions {
    pub copies: NonZeroU32,
    pub sides: Option<Sides>,
    pub color_mode: Option<ColorMode>,
    pub print_quality: Option<PrintQuality>,
    pub media: Option<JobMedia>,
}

impl ProviderJobOptions {
    pub fn new(copies: NonZeroU32) -> Self {
        Self {
            copies,
            sides: None,
            color_mode: None,
            print_quality: None,
            media: None,
        }
    }

    /// Encode every requested option, or refuse the whole set.
    pub(crate) fn to_attributes(&self) -> ProviderResult<Vec<JobAttribute>> {
        let mut attributes = vec![JobAttribute::new(
            "copies",
            AttributeValue::Integer(to_ipp_int(self.copies.get(), "copies")?),
        )];

        if let Some(sides) = self.sides {
            attributes.push(keyword_attribute("sides", sides.keyword())?);
        }
        if let Some(mode) = &self.color_mode {
            attributes.push(keyword_attribute("print-color-mode", mode.keyword())?);
        }
        if let Some(quality) = self.print_quality {
            attributes.push(JobAttribute::new(
                "print-quality",
                AttributeValue::Enum(quality.ipp_enum()),
            ));
        }
        if let Some(media) = &self.media {
            attributes.push(media_col(media)?);
        }
        Ok(attributes)
    }
}

/// A typed IPP job attribute, ready for the backend to encode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct JobAttribute {
    pub name: &'static str,
    pub value: AttributeValue,
}

impl JobAttribute {
    fn new(name: &'static str, value: AttributeValue) -> Self {
        Self { name, value }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AttributeValue {
    Integer(i32),
    Enum(i32),
    Keyword(String),
    Collection(Vec<JobAttribute>),
}

/// Encode media as one `media-col`.
///
/// `media-col` rather than `media` plus separate attributes because CUPS
/// only maps `media-type` and `media-source` to a driver's MediaType and
/// InputSlot when they are members of `media-col`
/// (`_ppdCacheGetMediaType` / `_ppdCacheGetInputSlot`). Sent on their own
/// they reach the job and are then ignored. IPP also treats `media` and
/// `media-col` as alternatives, so only one is sent.
fn media_col(media: &JobMedia) -> ProviderResult<JobAttribute> {
    let mut members = Vec::new();

    if let Some(size) = &media.size {
        let dimensions = vec![
            JobAttribute::new(
                "x-dimension",
                AttributeValue::Integer(hundredths_mm(size.width_um, "width")?),
            ),
            JobAttribute::new(
                "y-dimension",
                AttributeValue::Integer(hundredths_mm(size.length_um, "length")?),
            ),
        ];
        members.push(JobAttribute::new(
            "media-size",
            AttributeValue::Collection(dimensions),
        ));
        members.push(keyword_attribute("media-size-name", &size.canonical_name)?);
        for (name, um) in [
            ("media-bottom-margin", size.margins.bottom_um),
            ("media-left-margin", size.margins.left_um),
            ("media-right-margin", size.margins.right_um),
            ("media-top-margin", size.margins.top_um),
        ] {
            members.push(JobAttribute::new(
                name,
                AttributeValue::Integer(hundredths_mm(um, name)?),
            ));
        }
    }
    if let Some(source) = &media.media_source {
        members.push(keyword_attribute("media-source", source)?);
    }
    if let Some(media_type) = &media.media_type {
        members.push(keyword_attribute("media-type", media_type)?);
    }

    if members.is_empty() {
        return Err(invalid("media was requested but no media field was set"));
    }
    Ok(JobAttribute::new(
        "media-col",
        AttributeValue::Collection(members),
    ))
}

fn keyword_attribute(name: &'static str, value: &str) -> ProviderResult<JobAttribute> {
    validate_keyword(name, value)?;
    Ok(JobAttribute::new(
        name,
        AttributeValue::Keyword(value.to_string()),
    ))
}

/// Accept only what an IPP keyword can be, vendor forms included.
///
/// Stricter than CUPS: anything outside ASCII letters, digits, `.`, `_` and
/// `-` is refused rather than risked. A value the printer did not advertise
/// is still passed through — whether it is supported is the caller's call.
pub(crate) fn validate_keyword(name: &str, value: &str) -> ProviderResult<()> {
    let well_formed = !value.is_empty()
        && value.len() <= MAX_KEYWORD_BYTES
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if well_formed {
        Ok(())
    } else {
        Err(invalid(format!(
            "{name} value {value:?} is not a valid keyword"
        )))
    }
}

/// Micrometres to CUPS' hundredths of a millimetre, refusing anything that
/// would not convert exactly.
fn hundredths_mm(um: u32, what: &str) -> ProviderResult<i32> {
    if !um.is_multiple_of(UM_PER_HUNDREDTH_MM) {
        return Err(invalid(format!(
            "{what} of {um} um is not a whole hundredth of a millimetre"
        )));
    }
    to_ipp_int(um / UM_PER_HUNDREDTH_MM, what)
}

fn to_ipp_int(value: u32, what: &str) -> ProviderResult<i32> {
    i32::try_from(value).map_err(|_| invalid(format!("{what} {value} is out of range")))
}

fn invalid(detail: impl Into<String>) -> ProviderError {
    ProviderError::InvalidJobRequest {
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one() -> NonZeroU32 {
        NonZeroU32::new(1).expect("non-zero")
    }

    fn hagaki(margins: Margins) -> MediaDescriptor {
        MediaDescriptor {
            canonical_name: "jpn_hagaki_100x148mm".into(),
            width_um: 100_000,
            length_um: 148_000,
            margins,
            borderless: margins.is_full_bleed(),
        }
    }

    fn find<'a>(attributes: &'a [JobAttribute], name: &str) -> &'a AttributeValue {
        &attributes
            .iter()
            .find(|attribute| attribute.name == name)
            .unwrap_or_else(|| panic!("{name} missing"))
            .value
    }

    #[test]
    fn copies_is_always_sent_as_an_integer() {
        let attributes = ProviderJobOptions::new(one())
            .to_attributes()
            .expect("encodes");
        assert_eq!(
            attributes,
            vec![JobAttribute::new("copies", AttributeValue::Integer(1))]
        );
    }

    #[test]
    fn encodes_the_postcard_essentials_exactly() {
        let options = ProviderJobOptions {
            copies: one(),
            sides: Some(Sides::OneSided),
            color_mode: Some(ColorMode::Color),
            print_quality: Some(PrintQuality::High),
            media: Some(JobMedia {
                size: Some(
                    JobMediaSize::from_descriptor(&hagaki(Margins::default())).expect("known size"),
                ),
                media_type: Some("stationery".into()),
                media_source: Some("tray-2".into()),
            }),
        };
        let attributes = options.to_attributes().expect("encodes");

        assert_eq!(find(&attributes, "copies"), &AttributeValue::Integer(1));
        assert_eq!(
            find(&attributes, "sides"),
            &AttributeValue::Keyword("one-sided".into())
        );
        assert_eq!(
            find(&attributes, "print-color-mode"),
            &AttributeValue::Keyword("color".into())
        );
        // An enum, not the keyword "high" and not an integer.
        assert_eq!(find(&attributes, "print-quality"), &AttributeValue::Enum(5));

        let AttributeValue::Collection(media) = find(&attributes, "media-col") else {
            panic!("media-col must be a collection");
        };
        assert_eq!(
            find(media, "media-size"),
            &AttributeValue::Collection(vec![
                JobAttribute::new("x-dimension", AttributeValue::Integer(10_000)),
                JobAttribute::new("y-dimension", AttributeValue::Integer(14_800)),
            ])
        );
        assert_eq!(
            find(media, "media-size-name"),
            &AttributeValue::Keyword("jpn_hagaki_100x148mm".into())
        );
        for margin in [
            "media-bottom-margin",
            "media-left-margin",
            "media-right-margin",
            "media-top-margin",
        ] {
            assert_eq!(find(media, margin), &AttributeValue::Integer(0), "{margin}");
        }
        assert_eq!(
            find(media, "media-type"),
            &AttributeValue::Keyword("stationery".into())
        );
        assert_eq!(
            find(media, "media-source"),
            &AttributeValue::Keyword("tray-2".into())
        );
        // Never alongside media-col.
        assert!(attributes.iter().all(|attribute| attribute.name != "media"));
    }

    #[test]
    fn every_quality_keeps_its_ipp_enum() {
        for (quality, value) in [
            (PrintQuality::Draft, 3),
            (PrintQuality::Normal, 4),
            (PrintQuality::High, 5),
        ] {
            let mut options = ProviderJobOptions::new(one());
            options.print_quality = Some(quality);
            let attributes = options.to_attributes().expect("encodes");
            assert_eq!(
                find(&attributes, "print-quality"),
                &AttributeValue::Enum(value)
            );
        }
    }

    #[test]
    fn every_sides_value_has_its_keyword() {
        for (sides, keyword) in [
            (Sides::OneSided, "one-sided"),
            (Sides::TwoSidedLongEdge, "two-sided-long-edge"),
            (Sides::TwoSidedShortEdge, "two-sided-short-edge"),
        ] {
            let mut options = ProviderJobOptions::new(one());
            options.sides = Some(sides);
            let attributes = options.to_attributes().expect("encodes");
            assert_eq!(
                find(&attributes, "sides"),
                &AttributeValue::Keyword(keyword.into())
            );
        }
    }

    #[test]
    fn margins_travel_with_the_size() {
        let margins = Margins {
            top_um: 3_000,
            bottom_um: 3_000,
            left_um: 3_000,
            right_um: 3_000,
        };
        let mut options = ProviderJobOptions::new(one());
        options.media = Some(JobMedia {
            size: Some(JobMediaSize::from_descriptor(&hagaki(margins)).expect("known")),
            ..JobMedia::default()
        });
        let attributes = options.to_attributes().expect("encodes");
        let AttributeValue::Collection(media) = find(&attributes, "media-col") else {
            panic!("collection");
        };
        assert_eq!(
            find(media, "media-top-margin"),
            &AttributeValue::Integer(300)
        );
    }

    #[test]
    fn vendor_media_values_pass_through_unchanged() {
        let mut options = ProviderJobOptions::new(one());
        options.media = Some(JobMedia {
            media_type: Some("com.brother-Glossy_Photo.v2".into()),
            ..JobMedia::default()
        });
        let attributes = options.to_attributes().expect("encodes");
        let AttributeValue::Collection(media) = find(&attributes, "media-col") else {
            panic!("collection");
        };
        assert_eq!(
            media,
            &vec![JobAttribute::new(
                "media-type",
                AttributeValue::Keyword("com.brother-Glossy_Photo.v2".into())
            )]
        );
    }

    #[test]
    fn unencodable_options_are_refused_not_dropped() {
        let refuse = |options: ProviderJobOptions| {
            assert!(
                matches!(
                    options.to_attributes(),
                    Err(ProviderError::InvalidJobRequest { .. })
                ),
                "{options:?} must be refused"
            );
        };

        // A comma would turn one value into two in CUPS' option syntax.
        let mut comma = ProviderJobOptions::new(one());
        comma.media = Some(JobMedia {
            media_source: Some("tray-1,tray-2".into()),
            ..JobMedia::default()
        });
        refuse(comma);

        let mut nul = ProviderJobOptions::new(one());
        nul.color_mode = Some(ColorMode::Other("col\0or".into()));
        refuse(nul);

        let mut empty = ProviderJobOptions::new(one());
        empty.color_mode = Some(ColorMode::Other(String::new()));
        refuse(empty);

        let mut nothing = ProviderJobOptions::new(one());
        nothing.media = Some(JobMedia::default());
        refuse(nothing);

        let mut too_many = ProviderJobOptions::new(one());
        too_many.copies = NonZeroU32::new(u32::MAX).expect("non-zero");
        refuse(too_many);

        // 100.005 mm cannot be expressed in hundredths of a millimetre.
        let mut inexact = ProviderJobOptions::new(one());
        inexact.media = Some(JobMedia {
            size: Some(JobMediaSize {
                canonical_name: "custom_odd".into(),
                width_um: 100_005,
                length_um: 148_000,
                margins: Margins::default(),
            }),
            ..JobMedia::default()
        });
        refuse(inexact);
    }

    #[test]
    fn media_without_reported_dimensions_is_refused() {
        let mut descriptor = hagaki(Margins::default());
        descriptor.width_um = 0;
        assert!(JobMediaSize::from_descriptor(&descriptor).is_err());
    }
}
