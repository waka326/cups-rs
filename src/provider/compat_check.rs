//! Compile-time checks for the CUPS 2 / CUPS 3 FFI differences.
//!
//! The two versions do not agree on the types their accessors use:
//!
//! | accessor | CUPS 2 | CUPS 3 |
//! |---|---|---|
//! | `ippGetCount` returns | `int` | `size_t` |
//! | `ippGetBoolean` returns | `int` | `bool` |
//! | element index arguments | `int` | `size_t` |
//!
//! Provider code routes those through `crate::compat`, so a call that assumed
//! one version's types would stop compiling on the other. These assertions
//! make that contract explicit rather than leaving it to whichever version
//! happens to be built.
//!
//! Media flags differ too: CUPS 2 declares them as `#define`s and CUPS 3 as an
//! `enum`, so the generated bindings share no name. The crate's own
//! `constants::MEDIA_FLAGS_*` are used instead; the values were checked
//! against `cups/cups.h` in both the macOS 2.3.4 SDK and libcups v3.0.3 and
//! agree.

#[cfg(test)]
mod tests {
    use crate::compat::{count_to_usize, cups_bool, usize_to_count};
    use crate::constants;

    #[test]
    fn index_conversion_round_trips() {
        // Whatever the underlying integer type is, converting an index out and
        // back must land on the same element.
        for index in [0usize, 1, 2, 7, 4096] {
            assert_eq!(count_to_usize(usize_to_count(index)), index);
        }
    }

    #[test]
    fn count_conversion_yields_usable_length() {
        // decode_all sizes a Vec from this; a negative or wrapped value would
        // either panic or silently truncate the attribute.
        let zero = count_to_usize(usize_to_count(0));
        assert_eq!(zero, 0);
        let some = count_to_usize(usize_to_count(3));
        assert_eq!(some, 3);
    }

    #[test]
    fn cups_bool_normalises_both_representations() {
        // CUPS 2 hands back a C int, CUPS 3 a bool. Both must reach the same
        // Rust bool, or `Boolean` values would invert on one version.
        #[cfg(cups2)]
        {
            assert!(cups_bool(1));
            assert!(cups_bool(-1), "any non-zero C int is true");
            assert!(!cups_bool(0));
        }
        #[cfg(cups3)]
        {
            assert!(cups_bool(true));
            assert!(!cups_bool(false));
        }
    }

    #[test]
    fn media_flags_agree_across_versions() {
        // Values verified against cups/cups.h in macOS SDK 2.3.4 (#define) and
        // libcups v3.0.3 (enum cups_media_flags_e).
        assert_eq!(constants::MEDIA_FLAGS_DEFAULT, 0x00);
        assert_eq!(constants::MEDIA_FLAGS_BORDERLESS, 0x01);
        assert_eq!(constants::MEDIA_FLAGS_DUPLEX, 0x02);
        assert_eq!(constants::MEDIA_FLAGS_EXACT, 0x04);
        assert_eq!(constants::MEDIA_FLAGS_READY, 0x08);
    }

    #[test]
    fn exactly_one_cups_generation_is_selected() {
        // Both or neither would mean build.rs picked wrong.
        let cups2 = cfg!(cups2);
        let cups3 = cfg!(cups3);
        assert!(
            cups2 ^ cups3,
            "exactly one of cups2/cups3 must be configured"
        );
    }
}
