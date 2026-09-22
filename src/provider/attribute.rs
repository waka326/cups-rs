//! Attribute-level result states.
//!
//! A bare `Vec` cannot say why it is empty. The old accessors returned an
//! empty vector when the attribute was missing, when it carried no values,
//! and when the lookup itself failed, so a caller could not tell "this
//! printer offers no choices" from "this printer never mentioned the
//! attribute". These states are kept apart here.

use super::value::IppValue;

/// What came back when an attribute was queried.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttributeState<T> {
    /// The attribute was present and carried values.
    Present(Vec<T>),
    /// The attribute was present but held no values.
    Empty,
    /// The printer did not advertise the attribute at all.
    ///
    /// This is not a statement about capability: an attribute that was never
    /// advertised says nothing about whether the underlying feature works.
    NotAdvertised,
}

impl<T> AttributeState<T> {
    /// The values, if any were present. `Empty` and `NotAdvertised` both
    /// yield `None` — use the variant itself to tell them apart.
    pub fn values(&self) -> Option<&[T]> {
        match self {
            Self::Present(values) => Some(values),
            Self::Empty | Self::NotAdvertised => None,
        }
    }

    /// Whether the printer said anything at all about this attribute.
    pub fn is_advertised(&self) -> bool {
        !matches!(self, Self::NotAdvertised)
    }

    pub fn map<U, F: FnMut(T) -> U>(self, f: F) -> AttributeState<U> {
        match self {
            Self::Present(values) => AttributeState::Present(values.into_iter().map(f).collect()),
            Self::Empty => AttributeState::Empty,
            Self::NotAdvertised => AttributeState::NotAdvertised,
        }
    }

    /// Build a state from values, treating an empty collection as `Empty`
    /// rather than as `Present(vec![])`.
    ///
    /// Only use this when the attribute is known to exist; a missing
    /// attribute must be reported as `NotAdvertised` by the caller.
    pub fn from_present(values: Vec<T>) -> Self {
        if values.is_empty() {
            Self::Empty
        } else {
            Self::Present(values)
        }
    }
}

impl AttributeState<IppValue> {
    /// Whether any advertised value permits `candidate`.
    ///
    /// Ranges are honoured, so `copies` advertised as 1-9999 accepts 1.
    /// A missing or empty attribute accepts nothing: absence of evidence is
    /// not treated as permission.
    pub fn accepts_integer(&self, candidate: i32) -> bool {
        self.values()
            .is_some_and(|values| values.iter().any(|value| value.accepts_integer(candidate)))
    }

    /// Whether the given keyword/name/text value was advertised.
    pub fn contains_str(&self, candidate: &str) -> bool {
        self.values()
            .is_some_and(|values| values.iter().any(|value| value.as_str() == Some(candidate)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_not_advertised_are_different() {
        let empty: AttributeState<IppValue> = AttributeState::Empty;
        let missing: AttributeState<IppValue> = AttributeState::NotAdvertised;
        assert_ne!(empty, missing);
        assert!(empty.is_advertised());
        assert!(!missing.is_advertised());
    }

    #[test]
    fn from_present_collapses_empty_input() {
        let state = AttributeState::<IppValue>::from_present(vec![]);
        assert_eq!(state, AttributeState::Empty);
        assert_ne!(state, AttributeState::NotAdvertised);
    }

    #[test]
    fn copies_range_accepts_one() {
        // What the three spiked printers actually advertise.
        let copies = AttributeState::Present(vec![IppValue::Range {
            lower: 1,
            upper: 9999,
        }]);
        assert!(copies.accepts_integer(1));
        assert!(!copies.accepts_integer(0));
    }

    #[test]
    fn missing_attribute_grants_nothing() {
        let missing: AttributeState<IppValue> = AttributeState::NotAdvertised;
        assert!(!missing.accepts_integer(1));
        assert!(!missing.contains_str("one-sided"));

        let empty: AttributeState<IppValue> = AttributeState::Empty;
        assert!(!empty.accepts_integer(1));
        assert!(!empty.contains_str("one-sided"));
    }

    #[test]
    fn sides_keyword_lookup() {
        let sides = AttributeState::Present(vec![
            IppValue::Keyword("one-sided".into()),
            IppValue::Keyword("two-sided-long-edge".into()),
        ]);
        assert!(sides.contains_str("one-sided"));
        assert!(!sides.contains_str("three-sided"));
    }

    #[test]
    fn quality_enum_lookup() {
        // Epson advertises only 4 and 5; 3 (draft) is absent.
        let quality = AttributeState::Present(vec![IppValue::Enum(4), IppValue::Enum(5)]);
        assert!(quality.accepts_integer(5));
        assert!(!quality.accepts_integer(3));
    }
}
