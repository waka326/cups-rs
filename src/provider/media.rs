//! Media sizes and media queries.
//!
//! Sizes are carried as integer micrometres. CUPS stores them in hundredths
//! of a millimetre, so the conversion is exact and never goes through `f64`:
//! a caller asking "is this exactly 100 mm?" should not have to reason about
//! floating-point equality.

use super::value::IppValue;
use crate::constants;

/// One hundredth of a millimetre, the unit CUPS stores media sizes in.
const UM_PER_CUPS_UNIT: i64 = 10;

/// Convert a CUPS media dimension (1/100 mm) to micrometres.
///
/// Exact integer arithmetic; negative inputs are clamped to zero because a
/// negative physical dimension is meaningless.
pub const fn cups_units_to_um(value: i32) -> u32 {
    if value <= 0 {
        0
    } else {
        (value as i64 * UM_PER_CUPS_UNIT) as u32
    }
}

/// Which media list CUPS should consult.
///
/// The numeric flags stay inside this module. The spike picked `2` for
/// borderless once, which is the duplex flag, and measured the wrong thing as
/// a result; callers should never handle these numbers.
///
/// The values come from the crate's version-independent constants. CUPS 2
/// declares them as `#define`s and CUPS 3 as an enum, so the generated
/// bindings do not share a name — but both headers give the same values
/// (`cups/cups.h` in each release), which is what makes one typed query
/// usable against either version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaQuery {
    /// CUPS' default matching.
    Default,
    /// Borderless / full-bleed variants.
    Borderless,
    /// Exact dimensional matches only.
    Exact,
    /// Only media currently loaded.
    Ready,
}

impl MediaQuery {
    /// The media-flag value this query maps to.
    pub(crate) fn flags(self) -> u32 {
        match self {
            Self::Default => constants::MEDIA_FLAGS_DEFAULT,
            Self::Borderless => constants::MEDIA_FLAGS_BORDERLESS,
            Self::Exact => constants::MEDIA_FLAGS_EXACT,
            Self::Ready => constants::MEDIA_FLAGS_READY,
        }
    }
}

/// Margins around a media size, in micrometres.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Margins {
    pub top_um: u32,
    pub bottom_um: u32,
    pub left_um: u32,
    pub right_um: u32,
}

impl Margins {
    /// Whether every margin is zero, i.e. the sheet can be printed edge to
    /// edge.
    pub fn is_full_bleed(&self) -> bool {
        self.top_um == 0 && self.bottom_um == 0 && self.left_um == 0 && self.right_um == 0
    }
}

/// A media size as the printer described it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaDescriptor {
    /// The PWG self-describing name, e.g. `jpn_hagaki_100x148mm`.
    pub canonical_name: String,
    pub width_um: u32,
    pub length_um: u32,
    pub margins: Margins,
    /// Whether this descriptor came back from a borderless query and has no
    /// margins. Callers should not infer this from the name.
    pub borderless: bool,
}

impl MediaDescriptor {
    /// Whether the printer gave real dimensions for this media.
    ///
    /// Ready media is reported by name; CUPS does not always resolve that name
    /// to a size. A zero here means "the size was not reported", not a sheet
    /// with no area, and size comparisons on such an entry are meaningless.
    pub fn has_known_dimensions(&self) -> bool {
        self.width_um > 0 && self.length_um > 0
    }

    /// Whether this media is `width_um` x `length_um` within `tolerance_um`.
    ///
    /// Real printers do not always report exact figures — one queue in the
    /// spike reported a 100 x 148 mm card as 100.2 x 147.8 mm — so an exact
    /// comparison would reject media that is physically correct.
    pub fn matches_size(&self, width_um: u32, length_um: u32, tolerance_um: u32) -> bool {
        // An entry whose size was never reported matches nothing; treating it
        // as 0 x 0 would let a large tolerance make it match anything.
        self.has_known_dimensions()
            && within(self.width_um, width_um, tolerance_um)
            && within(self.length_um, length_um, tolerance_um)
    }
}

fn within(actual: u32, expected: u32, tolerance_um: u32) -> bool {
    actual.abs_diff(expected) <= tolerance_um
}

/// 100 mm, the width of a Japanese postcard.
pub const POSTCARD_WIDTH_UM: u32 = 100_000;
/// 148 mm, the length of a Japanese postcard.
pub const POSTCARD_LENGTH_UM: u32 = 148_000;

/// How far a reported dimension may sit from the nominal one and still count
/// as the same sheet.
///
/// 0.5 mm covers the rounding real queues were observed to apply while
/// staying far below the gap to any neighbouring stock size.
pub const POSTCARD_TOLERANCE_UM: u32 = 500;

impl MediaDescriptor {
    /// Whether this is a 100 x 148 mm postcard, judged by its dimensions
    /// rather than by its name.
    pub fn is_postcard_100x148(&self) -> bool {
        self.matches_size(POSTCARD_WIDTH_UM, POSTCARD_LENGTH_UM, POSTCARD_TOLERANCE_UM)
    }
}

/// What a ready-media query found.
///
/// `Present(vec![])` and `NotAdvertised` are different answers: the first
/// means the printer reported an empty ready list, the second means it never
/// reported one. Neither means "no paper is loaded" — CUPS simply may not
/// know, which is what all three printers in the spike showed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadyMedia {
    Present(Vec<MediaDescriptor>),
    NotAdvertised,
}

impl ReadyMedia {
    /// Whether the printer told us anything about loaded media.
    ///
    /// When this is false the loaded media is unknown, which is not the same
    /// as it being absent.
    pub fn is_known(&self) -> bool {
        matches!(self, Self::Present(_))
    }

    pub fn descriptors(&self) -> &[MediaDescriptor] {
        match self {
            Self::Present(values) => values,
            Self::NotAdvertised => &[],
        }
    }
}

/// Media types and sources come back as plain IPP values; vendor keywords
/// such as `com.canon.mthagakia` are passed through untouched because their
/// meaning is the caller's to decide.
pub type MediaKeywords = super::attribute::AttributeState<IppValue>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_cups_units_without_float() {
        // 100.00 mm and 148.00 mm as CUPS stores them.
        assert_eq!(cups_units_to_um(10_000), 100_000);
        assert_eq!(cups_units_to_um(14_800), 148_000);
        assert_eq!(cups_units_to_um(0), 0);
        // A nonsensical negative dimension collapses to zero.
        assert_eq!(cups_units_to_um(-1), 0);
    }

    #[test]
    fn media_flags_match_cups_header() {
        // Verified against cups/cups.h in both CUPS 2 (macOS SDK 2.3.4) and
        // CUPS 3 (libcups v3.0.3): the values agree even though CUPS 2
        // declares them as #defines and CUPS 3 as an enum.
        assert_eq!(MediaQuery::Default.flags(), 0x00);
        assert_eq!(MediaQuery::Borderless.flags(), 0x01);
        assert_eq!(MediaQuery::Exact.flags(), 0x04);
        assert_eq!(MediaQuery::Ready.flags(), 0x08);
        // The spike once passed 2 for borderless, which is the duplex flag,
        // and measured the wrong variant.
        assert_ne!(MediaQuery::Borderless.flags(), 0x02);
    }

    #[test]
    fn media_flags_are_distinct_bits() {
        // A typo that collapsed two queries onto one flag would silently
        // return the wrong media list.
        let flags = [
            MediaQuery::Default.flags(),
            MediaQuery::Borderless.flags(),
            MediaQuery::Exact.flags(),
            MediaQuery::Ready.flags(),
        ];
        for (i, a) in flags.iter().enumerate() {
            for b in flags.iter().skip(i + 1) {
                assert_ne!(a, b, "media query flags must not collide");
            }
        }
    }

    fn postcard(width_um: u32, length_um: u32, margins: Margins) -> MediaDescriptor {
        MediaDescriptor {
            canonical_name: "jpn_hagaki_100x148mm".into(),
            width_um,
            length_um,
            margins,
            borderless: margins.is_full_bleed(),
        }
    }

    #[test]
    fn recognises_exact_postcard() {
        let media = postcard(
            100_000,
            148_000,
            Margins {
                top_um: 3_000,
                bottom_um: 3_000,
                left_um: 3_000,
                right_um: 3_000,
            },
        );
        assert!(media.is_postcard_100x148());
        assert!(!media.margins.is_full_bleed());
    }

    #[test]
    fn recognises_slightly_off_postcard() {
        // Brother_MFC_7460DN reported 100.2 x 147.8 mm for the same stock.
        let media = postcard(100_200, 147_800, Margins::default());
        assert!(media.is_postcard_100x148());
    }

    #[test]
    fn rejects_neighbouring_sizes() {
        // A6 is 105 x 148 mm and must not pass as a postcard.
        let a6 = MediaDescriptor {
            canonical_name: "iso_a6_105x148mm".into(),
            width_um: 105_000,
            length_um: 148_000,
            margins: Margins::default(),
            borderless: false,
        };
        assert!(!a6.is_postcard_100x148());
    }

    #[test]
    fn borderless_postcard_has_no_margins() {
        let media = postcard(100_000, 148_000, Margins::default());
        assert!(media.is_postcard_100x148());
        assert!(media.margins.is_full_bleed());
        assert!(media.borderless);
    }

    #[test]
    fn media_without_reported_dimensions_matches_nothing() {
        // Ready media can come back as a name with no resolvable size.
        let unknown = MediaDescriptor {
            canonical_name: "jpn_hagaki_100x148mm".into(),
            width_um: 0,
            length_um: 0,
            margins: Margins::default(),
            borderless: false,
        };
        assert!(!unknown.has_known_dimensions());
        assert!(!unknown.is_postcard_100x148());
        // Even a wildly generous tolerance must not make it match.
        assert!(!unknown.matches_size(100_000, 148_000, 1_000_000));
    }

    #[test]
    fn ready_media_empty_is_not_missing() {
        let empty = ReadyMedia::Present(vec![]);
        let missing = ReadyMedia::NotAdvertised;
        assert_ne!(empty, missing);
        // An empty-but-reported list is still knowledge; a missing one is not.
        assert!(empty.is_known());
        assert!(!missing.is_known());
        assert!(empty.descriptors().is_empty());
        assert!(missing.descriptors().is_empty());
    }
}
