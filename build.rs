use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

// Parsing helpers live in their own module so the crate's test suite can cover
// them; build.rs is a separate crate that `cargo test` never runs tests for.
#[path = "build_support/discovery.rs"]
mod discovery;

use discovery::{CupsGeneration, parse_include_paths, parse_link_flags, parse_major_version};

/// Everything bindgen needs after a successful discovery.
struct CupsBuild {
    include_paths: Vec<PathBuf>,
}

/// Run a discovery helper and return its stdout, or `None` if the tool is
/// missing or exits non-zero.
///
/// Arguments are passed as structured argv; no shell is involved.
fn run_tool<S: AsRef<OsStr>>(program: &str, args: &[S]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// Locate CUPS headers through the active SDK.
///
/// Apple's `cups-config --cflags` prints nothing because the headers live in
/// the selected SDK rather than on the default include path. The SDK is only
/// accepted when the header actually exists there, so a stale or wrong
/// `xcrun` answer cannot produce a half-configured build.
fn sdk_include_path() -> Option<PathBuf> {
    let sdk = run_tool("xcrun", &["--show-sdk-path"])?;
    let candidate = Path::new(sdk.trim()).join("usr/include");
    candidate.join("cups/cups.h").exists().then_some(candidate)
}

/// Discover the system CUPS through `cups-config`.
///
/// This is the vendor-supported discovery tool on macOS, where the system
/// CUPS ships no `cups.pc` and pkg-config therefore always fails.
///
/// Returns `None` — never a partially populated result — when the version is
/// unrecognised, no library can be linked, or no usable include path exists.
/// Callers treat that as "this route did not work" rather than as a hard
/// build failure, so the remaining routes still get a chance.
fn probe_with_cups_config() -> Option<(CupsBuild, CupsGeneration)> {
    let version = run_tool("cups-config", &["--version"])?;
    let generation = CupsGeneration::from_major(parse_major_version(&version)?)?;

    let link = parse_link_flags(&run_tool("cups-config", &["--libs"])?);
    if link.libs.is_empty() {
        // Without a library to link against, the rest is useless.
        return None;
    }

    let mut include_paths = parse_include_paths(&run_tool("cups-config", &["--cflags"])?);
    if include_paths.is_empty() {
        include_paths.extend(sdk_include_path());
    }
    if include_paths.is_empty() {
        return None;
    }

    // Link directives are only emitted once the probe as a whole has
    // succeeded, so a rejected probe leaves no half-applied configuration.
    for dir in &link.search_paths {
        println!("cargo:rustc-link-search=native={dir}");
    }
    for lib in &link.libs {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }

    // Always announce the fallback: silently taking this route on a platform
    // that was expected to use pkg-config would hide a broken environment.
    println!(
        "cargo:warning=cups-rs: pkg-config found no CUPS; using cups-config \
         (version {}, building as {})",
        version.trim(),
        generation.cfg()
    );

    Some((CupsBuild { include_paths }, generation))
}

fn from_pkg_config(library: pkg_config::Library) -> CupsBuild {
    CupsBuild {
        include_paths: library.include_paths,
    }
}

/// Resolve CUPS for an explicitly requested generation.
///
/// pkg-config is tried first so existing environments behave exactly as
/// before. The cups-config fallback is only accepted when it reports the same
/// generation that was requested: `force-cups2` must never be satisfied by a
/// CUPS 3 installation.
fn discover_forced(requested: CupsGeneration) -> (CupsBuild, CupsGeneration) {
    let pkg_name = match requested {
        CupsGeneration::Cups2 => "cups",
        CupsGeneration::Cups3 => "cups3",
    };

    if let Ok(library) = pkg_config::probe_library(pkg_name) {
        return (from_pkg_config(library), requested);
    }

    match probe_with_cups_config() {
        Some((build, found)) if found == requested => (build, requested),
        Some((_, found)) => panic!(
            "feature force-{} was requested but cups-config reports {}; \
             refusing to build against a different CUPS generation",
            requested.cfg(),
            found.cfg()
        ),
        None => panic!("Failed to find {pkg_name} with pkg-config or cups-config"),
    }
}

/// Resolve CUPS with no explicit preference.
///
/// Order: pkg-config cups3, pkg-config cups, cups-config. Every route reports
/// which generation it found; there is no default to fall back on.
fn discover_auto() -> (CupsBuild, CupsGeneration) {
    if let Ok(library) = pkg_config::probe_library("cups3") {
        return (from_pkg_config(library), CupsGeneration::Cups3);
    }

    if let Ok(library) = pkg_config::probe_library("cups") {
        return (from_pkg_config(library), CupsGeneration::Cups2);
    }

    match probe_with_cups_config() {
        Some((build, generation)) => (build, generation),
        None => panic!("Failed to find cups3 or cups with pkg-config or cups-config"),
    }
}

fn main() {
    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-changed=build_support/discovery.rs");
    println!("cargo:rustc-check-cfg=cfg(cups2)");
    println!("cargo:rustc-check-cfg=cfg(cups3)");

    let force_cups2 = env::var_os("CARGO_FEATURE_FORCE_CUPS2").is_some();
    let force_cups3 = env::var_os("CARGO_FEATURE_FORCE_CUPS3").is_some();

    if force_cups2 && force_cups3 {
        panic!("features force-cups2 and force-cups3 cannot be enabled together");
    }

    let (build, generation) = if force_cups2 {
        discover_forced(CupsGeneration::Cups2)
    } else if force_cups3 {
        discover_forced(CupsGeneration::Cups3)
    } else {
        discover_auto()
    };

    println!("cargo:rustc-cfg={}", generation.cfg());

    let mut builder = bindgen::Builder::default()
        .header("wrapper.h")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        // Allow CUPS types and functions
        .allowlist_function("cups.*")
        .allowlist_type("cups_.*")
        .allowlist_var("CUPS_.*")
        // Allow HTTP types and functions (needed for http_t)
        .allowlist_function("http.*")
        .allowlist_type("http_.*")
        .allowlist_var("HTTP_.*")
        // Allow IPP types and functions
        .allowlist_function("ipp.*")
        .allowlist_type("ipp_.*")
        .allowlist_var("IPP_.*");

    // Lets wrapper.h include cups/dnssd.h, which only CUPS 3 has.
    if generation == CupsGeneration::Cups3 {
        builder = builder.clang_arg("-DCUPS_RS_CUPS3");
    }

    for include_path in build.include_paths {
        builder = builder.clang_arg(format!("-I{}", include_path.display()));
    }

    let bindings = builder.generate().expect("Unable to generate bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Couldn't write bindings!");
}
