//! Typed IPP values.
//!
//! The older `Vec<String>` accessors in this crate guess at a value's type by
//! trying `ippGetString`, then `ippGetInteger`, then `ippGetBoolean` and
//! keeping whichever answers first. That guess is wrong for any tag those
//! three accessors do not cover:
//!
//! - `copies-supported` is a `rangeOfInteger`; none of the three match, so
//!   `ippGetInteger` returns 0 and the caller sees `["0"]` instead of 1-9999.
//! - `orientation-requested-default` is `no-value`; `ippGetBoolean` returns 0
//!   and the caller sees `Some("false")`, a default that does not exist.
//!
//! Everything here dispatches on `ippGetValueTag` instead, so a value is only
//! decoded by an accessor that matches its actual tag.

use std::ffi::CStr;
use std::ptr;

use crate::bindings;
use crate::compat::{count_to_usize, cups_bool, usize_to_count};

/// How a resolution's numbers should be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolutionUnits {
    PerInch,
    PerCentimeter,
    /// A units code libcups reported that IPP does not define.
    Other(i32),
}

impl ResolutionUnits {
    fn from_code(code: i32) -> Self {
        if code == bindings::ipp_res_e_IPP_RES_PER_INCH as i32 {
            Self::PerInch
        } else if code == bindings::ipp_res_e_IPP_RES_PER_CM as i32 {
            Self::PerCentimeter
        } else {
            Self::Other(code)
        }
    }
}

/// A single IPP value with its type preserved.
///
/// `NoValue`, `Unknown` and `Unsupported` are distinct variants because IPP
/// distinguishes them: collapsing any of them into `Boolean(false)` or an
/// empty string loses the difference between "there is no default" and "the
/// default is false".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IppValue {
    Keyword(String),
    Name(String),
    Text(String),
    Uri(String),
    Charset(String),
    Language(String),
    MimeType(String),
    Integer(i32),
    /// An enum value. The number is kept as-is; what `5` means depends on the
    /// attribute, and that mapping belongs to the caller.
    Enum(i32),
    Boolean(bool),
    /// `rangeOfInteger`. Both bounds are inclusive.
    Range {
        lower: i32,
        upper: i32,
    },
    Resolution {
        cross_feed: i32,
        feed: i32,
        units: ResolutionUnits,
    },
    /// The printer explicitly said there is no value.
    NoValue,
    /// The printer explicitly said the value is unknown.
    Unknown,
    /// The printer explicitly said the attribute is unsupported.
    Unsupported,
    /// A tag this crate does not decode.
    ///
    /// Only the tag number is kept. libcups exposes no public accessor that
    /// returns the raw bytes of an arbitrary tag — `ippGetOctetString` is
    /// documented for `octetString` and was verified to return NULL for
    /// integer, enum, boolean, keyword, range, no-value and resolution
    /// attributes. Rather than invent bytes, the identity of the tag is
    /// preserved and the value is left undecoded.
    UnknownTag {
        tag: u32,
    },
}

impl IppValue {
    /// The integer a caller can use for a numeric comparison, if this value
    /// carries one. `Range` is deliberately excluded: a range is not a single
    /// number, and callers should use [`IppValue::accepts_integer`].
    pub fn as_integer(&self) -> Option<i32> {
        match self {
            Self::Integer(value) | Self::Enum(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Keyword(value)
            | Self::Name(value)
            | Self::Text(value)
            | Self::Uri(value)
            | Self::Charset(value)
            | Self::Language(value)
            | Self::MimeType(value) => Some(value.as_str()),
            _ => None,
        }
    }

    /// Whether this value permits `candidate`.
    ///
    /// A `Range` accepts anything between its inclusive bounds; `Integer` and
    /// `Enum` accept only their exact value. This is what lets a caller ask
    /// "does this printer accept copies=1?" without knowing whether the
    /// printer advertised a range or a single number.
    pub fn accepts_integer(&self, candidate: i32) -> bool {
        match self {
            Self::Range { lower, upper } => *lower <= candidate && candidate <= *upper,
            Self::Integer(value) | Self::Enum(value) => *value == candidate,
            _ => false,
        }
    }

    /// Whether this value is one of IPP's out-of-band markers rather than
    /// actual data.
    pub fn is_out_of_band(&self) -> bool {
        matches!(self, Self::NoValue | Self::Unknown | Self::Unsupported)
    }
}

/// Read the string at `index`, if libcups returns one.
unsafe fn string_at(attr: *mut bindings::ipp_attribute_t, index: usize) -> Option<String> {
    let ptr = unsafe { bindings::ippGetString(attr, usize_to_count(index), ptr::null_mut()) };
    if ptr.is_null() {
        return None;
    }
    Some(
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned(),
    )
}

/// Decode one value of `attr` at `index`, dispatching on the attribute's
/// actual value tag.
///
/// Returns `None` only when a tag that should carry data yields nothing —
/// a malformed attribute — so callers can tell that apart from a tag that is
/// merely undecoded (`UnknownTag`).
///
/// # Safety
///
/// `attr` must be a valid, non-null `ipp_attribute_t` owned by a live IPP
/// message, and `index` must be less than `ippGetCount(attr)`.
pub unsafe fn decode_value(attr: *mut bindings::ipp_attribute_t, index: usize) -> Option<IppValue> {
    // CUPS 2 indexes with c_int and CUPS 3 with usize; the conversion lives in
    // the crate's compat layer so the rest of this function stays
    // version-agnostic.
    let element = usize_to_count(index);
    let tag = unsafe { bindings::ippGetValueTag(attr) };

    // Out-of-band tags carry no data; they are the answer themselves.
    if tag == bindings::ipp_tag_e_IPP_TAG_NOVALUE {
        return Some(IppValue::NoValue);
    }
    if tag == bindings::ipp_tag_e_IPP_TAG_UNKNOWN {
        return Some(IppValue::Unknown);
    }
    if tag == bindings::ipp_tag_e_IPP_TAG_UNSUPPORTED_VALUE {
        return Some(IppValue::Unsupported);
    }

    if tag == bindings::ipp_tag_e_IPP_TAG_INTEGER {
        return Some(IppValue::Integer(unsafe {
            bindings::ippGetInteger(attr, element)
        }));
    }
    if tag == bindings::ipp_tag_e_IPP_TAG_ENUM {
        return Some(IppValue::Enum(unsafe {
            bindings::ippGetInteger(attr, element)
        }));
    }
    if tag == bindings::ipp_tag_e_IPP_TAG_BOOLEAN {
        // CUPS 2 returns a C int and CUPS 3 a bool; cups_bool normalises both.
        return Some(IppValue::Boolean(cups_bool(unsafe {
            bindings::ippGetBoolean(attr, element)
        })));
    }

    if tag == bindings::ipp_tag_e_IPP_TAG_RANGE {
        let mut upper: i32 = 0;
        let lower = unsafe { bindings::ippGetRange(attr, element, &mut upper) };
        return Some(IppValue::Range { lower, upper });
    }

    if tag == bindings::ipp_tag_e_IPP_TAG_RESOLUTION {
        let mut feed: i32 = 0;
        let mut units: bindings::ipp_res_t = bindings::ipp_res_e_IPP_RES_PER_INCH;
        let cross_feed =
            unsafe { bindings::ippGetResolution(attr, element, &mut feed, &mut units) };
        return Some(IppValue::Resolution {
            cross_feed,
            feed,
            units: ResolutionUnits::from_code(units as i32),
        });
    }

    // String-like tags. Each keeps its own variant so a keyword is never
    // mistaken for free text.
    let string_variant: Option<fn(String) -> IppValue> = if tag
        == bindings::ipp_tag_e_IPP_TAG_KEYWORD
    {
        Some(IppValue::Keyword)
    } else if tag == bindings::ipp_tag_e_IPP_TAG_NAME || tag == bindings::ipp_tag_e_IPP_TAG_NAMELANG
    {
        Some(IppValue::Name)
    } else if tag == bindings::ipp_tag_e_IPP_TAG_TEXT || tag == bindings::ipp_tag_e_IPP_TAG_TEXTLANG
    {
        Some(IppValue::Text)
    } else if tag == bindings::ipp_tag_e_IPP_TAG_URI {
        Some(IppValue::Uri)
    } else if tag == bindings::ipp_tag_e_IPP_TAG_CHARSET {
        Some(IppValue::Charset)
    } else if tag == bindings::ipp_tag_e_IPP_TAG_LANGUAGE {
        Some(IppValue::Language)
    } else if tag == bindings::ipp_tag_e_IPP_TAG_MIMETYPE {
        Some(IppValue::MimeType)
    } else {
        None
    };

    if let Some(build) = string_variant {
        return unsafe { string_at(attr, index) }.map(build);
    }

    // Anything else keeps its tag identity rather than being forced into a
    // type it is not.
    Some(IppValue::UnknownTag { tag: tag as u32 })
}

/// Decode every value of `attr`.
///
/// # Safety
///
/// `attr` must be a valid, non-null `ipp_attribute_t` owned by a live IPP
/// message.
pub unsafe fn decode_all(attr: *mut bindings::ipp_attribute_t) -> Vec<IppValue> {
    let count = count_to_usize(unsafe { bindings::ippGetCount(attr) });
    let mut values = Vec::with_capacity(count);
    for index in 0..count {
        if let Some(value) = unsafe { decode_value(attr, index) } {
            values.push(value);
        }
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_accepts_values_inside_bounds() {
        // copies-supported on all three spiked printers is 1-9999.
        let copies = IppValue::Range {
            lower: 1,
            upper: 9999,
        };
        assert!(copies.accepts_integer(1));
        assert!(copies.accepts_integer(9999));
        assert!(!copies.accepts_integer(0));
        assert!(!copies.accepts_integer(10_000));
    }

    #[test]
    fn range_is_not_a_single_integer() {
        // The old code turned this attribute into ["0"]; nothing here should
        // present a range as if it were one number.
        let copies = IppValue::Range {
            lower: 1,
            upper: 9999,
        };
        assert_eq!(copies.as_integer(), None);
        assert_ne!(copies, IppValue::Integer(0));
    }

    #[test]
    fn enum_keeps_its_number() {
        // print-quality high is enum 5; the meaning stays with the caller.
        let quality = IppValue::Enum(5);
        assert_eq!(quality.as_integer(), Some(5));
        assert!(quality.accepts_integer(5));
        assert!(!quality.accepts_integer(4));
    }

    #[test]
    fn no_value_is_not_false() {
        // orientation-requested-default is no-value on all three printers;
        // the old code reported Some("false").
        let default = IppValue::NoValue;
        assert_ne!(default, IppValue::Boolean(false));
        assert!(default.is_out_of_band());
        assert_eq!(default.as_integer(), None);
        assert_eq!(default.as_str(), None);
    }

    #[test]
    fn out_of_band_markers_stay_distinct() {
        assert_ne!(IppValue::NoValue, IppValue::Unknown);
        assert_ne!(IppValue::Unknown, IppValue::Unsupported);
        assert!(IppValue::Unknown.is_out_of_band());
        assert!(IppValue::Unsupported.is_out_of_band());
        assert!(!IppValue::Boolean(false).is_out_of_band());
    }

    #[test]
    fn booleans_round_trip() {
        assert_eq!(IppValue::Boolean(true).as_integer(), None);
        assert!(!IppValue::Boolean(true).is_out_of_band());
        assert_ne!(IppValue::Boolean(true), IppValue::Boolean(false));
    }

    #[test]
    fn string_kinds_are_not_interchangeable() {
        // A keyword and a free-text value with the same characters are
        // different things in IPP.
        assert_ne!(
            IppValue::Keyword("one-sided".into()),
            IppValue::Text("one-sided".into())
        );
        assert_eq!(
            IppValue::Keyword("one-sided".into()).as_str(),
            Some("one-sided")
        );
    }

    #[test]
    fn unknown_tag_keeps_identity_and_decodes_to_nothing_else() {
        let value = IppValue::UnknownTag { tag: 0x37 };
        assert_eq!(value.as_integer(), None);
        assert_eq!(value.as_str(), None);
        assert!(!value.is_out_of_band());
        assert!(!value.accepts_integer(0));
        // Crucially, it is not silently any known value.
        assert_ne!(value, IppValue::Integer(0));
        assert_ne!(value, IppValue::Boolean(false));
        assert_ne!(value, IppValue::NoValue);
    }

    #[test]
    fn resolution_units_map_known_codes() {
        assert_eq!(
            ResolutionUnits::from_code(bindings::ipp_res_e_IPP_RES_PER_INCH as i32),
            ResolutionUnits::PerInch
        );
        assert_eq!(
            ResolutionUnits::from_code(bindings::ipp_res_e_IPP_RES_PER_CM as i32),
            ResolutionUnits::PerCentimeter
        );
        // An undefined units code keeps its number rather than being coerced.
        assert_eq!(ResolutionUnits::from_code(9), ResolutionUnits::Other(9));
    }
}
