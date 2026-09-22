//! Pure parsing helpers shared by `build.rs` and the crate's test suite.
//!
//! `build.rs` runs as its own crate, so `cargo test` never executes tests
//! placed inside it. Keeping the parsing logic here — with no I/O and no
//! build-dependency types — lets the same code be covered by the normal test
//! suite and by CI.

use std::path::PathBuf;

/// Which CUPS generation the crate is being built against.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CupsGeneration {
    Cups2,
    Cups3,
}

impl CupsGeneration {
    pub fn cfg(self) -> &'static str {
        match self {
            Self::Cups2 => "cups2",
            Self::Cups3 => "cups3",
        }
    }

    /// Map the major version reported by `cups-config --version`.
    ///
    /// Anything outside 2 and 3 is rejected rather than guessed at: a future
    /// CUPS 4 must not silently build as CUPS 3.
    pub fn from_major(major: u32) -> Option<Self> {
        match major {
            2 => Some(Self::Cups2),
            3 => Some(Self::Cups3),
            _ => None,
        }
    }
}

/// Parse the major version out of `cups-config --version` output such as
/// "2.3.4". Returns `None` for anything that is not a leading integer.
pub fn parse_major_version(version: &str) -> Option<u32> {
    version.trim().split('.').next()?.parse().ok()
}

/// Extract `-I` include paths from a `cups-config --cflags` string.
///
/// `cups-config` emits a plain space-separated list, so splitting on
/// whitespace is sufficient and avoids handing the string to a shell.
pub fn parse_include_paths(cflags: &str) -> Vec<PathBuf> {
    cflags
        .split_whitespace()
        .filter_map(|flag| flag.strip_prefix("-I"))
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Link directives extracted from `cups-config --libs`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LinkFlags {
    pub libs: Vec<String>,
    pub search_paths: Vec<String>,
}

/// Extract `-l` / `-L` directives from a `cups-config --libs` string.
///
/// Other flags (`-framework`, `-Wl,...`, optimisation flags) are not link
/// directives Cargo understands here and are intentionally ignored rather
/// than forwarded blindly.
pub fn parse_link_flags(libs: &str) -> LinkFlags {
    let mut parsed = LinkFlags::default();

    for flag in libs.split_whitespace() {
        if let Some(dir) = flag.strip_prefix("-L").filter(|dir| !dir.is_empty()) {
            parsed.search_paths.push(dir.to_string());
        } else if let Some(lib) = flag.strip_prefix("-l").filter(|lib| !lib.is_empty()) {
            parsed.libs.push(lib.to_string());
        }
    }

    parsed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_major_version_from_real_output() {
        // The value macOS 2.3.4 actually reports.
        assert_eq!(parse_major_version("2.3.4"), Some(2));
        assert_eq!(parse_major_version("3.0.0"), Some(3));
        assert_eq!(parse_major_version(" 2.4.7 \n"), Some(2));
    }

    #[test]
    fn rejects_unparsable_version() {
        assert_eq!(parse_major_version(""), None);
        assert_eq!(parse_major_version("unknown"), None);
        assert_eq!(parse_major_version("v2.3.4"), None);
    }

    #[test]
    fn maps_only_known_generations() {
        assert_eq!(CupsGeneration::from_major(2), Some(CupsGeneration::Cups2));
        assert_eq!(CupsGeneration::from_major(3), Some(CupsGeneration::Cups3));
        // A future CUPS must not be silently treated as a known generation.
        assert_eq!(CupsGeneration::from_major(4), None);
        assert_eq!(CupsGeneration::from_major(1), None);
        assert_eq!(CupsGeneration::from_major(0), None);
    }

    #[test]
    fn parses_include_paths() {
        assert_eq!(
            parse_include_paths("-I/usr/include -I/opt/cups/include"),
            vec![
                PathBuf::from("/usr/include"),
                PathBuf::from("/opt/cups/include"),
            ]
        );
    }

    #[test]
    fn apple_empty_cflags_yields_no_include_paths() {
        // Apple's cups-config prints nothing; the SDK lookup handles that case.
        assert!(parse_include_paths("").is_empty());
        assert!(parse_include_paths("   \n").is_empty());
    }

    #[test]
    fn ignores_non_include_flags() {
        assert!(parse_include_paths("-D_REENTRANT -O2").is_empty());
    }

    #[test]
    fn parses_link_flags() {
        let flags = parse_link_flags("-L/opt/cups/lib -lcups -lz");
        assert_eq!(flags.search_paths, vec!["/opt/cups/lib".to_string()]);
        assert_eq!(flags.libs, vec!["cups".to_string(), "z".to_string()]);
    }

    #[test]
    fn parses_macos_link_flags() {
        // What macOS 2.3.4 actually reports.
        let flags = parse_link_flags("-lcups");
        assert_eq!(flags.libs, vec!["cups".to_string()]);
        assert!(flags.search_paths.is_empty());
    }

    #[test]
    fn ignores_non_link_flags() {
        // Frameworks and linker flags must not be mistaken for libraries.
        let flags = parse_link_flags("-lcups -framework CoreFoundation -Wl,-search_paths_first");
        assert_eq!(flags.libs, vec!["cups".to_string()]);
        assert!(flags.search_paths.is_empty());
    }

    #[test]
    fn empty_link_flags_yield_nothing() {
        assert_eq!(parse_link_flags(""), LinkFlags::default());
    }
}
