//! The guard that keeps a default test run from printing.
//!
//! Three integration tests create real CUPS jobs and one cancels them. They
//! are protected twice over: `#[ignore]` keeps `cargo test` from selecting
//! them, and a compile-time feature check makes them return immediately even
//! when selected explicitly. This file asserts the guard is still in place, so
//! removing either half fails the build rather than quietly enabling printing
//! on whatever machine runs CI next.

/// Tests that touch a real CUPS job. Each must carry both guards.
const JOB_TOUCHING_TESTS: &[&str] = &[
    "fn test_integration_job_lifecycle()",
    "fn test_integration_job_with_options()",
    "fn test_integration_job_cancellation()",
    "fn test_integration_error_handling()",
];

fn integration_source() -> String {
    // CARGO_MANIFEST_DIR is the crate root regardless of where cargo is run.
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/integration_tests.rs");
    std::fs::read_to_string(&path).expect("integration test source is readable")
}

#[test]
fn default_build_cannot_create_print_jobs() {
    // The whole point of the feature: absent it, nothing may print.
    assert!(
        !cfg!(feature = "allow-print-job-tests"),
        "the default test build must not enable allow-print-job-tests; \
         a CI job that turns this on can send real jobs to a real printer"
    );
}

#[test]
fn job_touching_tests_are_ignored_by_default() {
    let source = integration_source();
    for signature in JOB_TOUCHING_TESTS {
        let position = source
            .find(signature)
            .unwrap_or_else(|| panic!("{signature} not found; update this guard"));
        // Look at the attributes immediately above the function.
        let preceding = &source[position.saturating_sub(300)..position];
        assert!(
            preceding.contains("#[ignore"),
            "{signature} must be #[ignore]d so a plain `cargo test` cannot select it"
        );
    }
}

#[test]
fn job_touching_tests_check_the_feature_at_runtime() {
    let source = integration_source();
    for signature in JOB_TOUCHING_TESTS {
        let position = source
            .find(signature)
            .unwrap_or_else(|| panic!("{signature} not found; update this guard"));
        // The guard must be the first thing the body does, before any CUPS
        // call, so `--include-ignored` alone still cannot print.
        let body_start = &source[position..position + 400];
        assert!(
            body_start.contains("print_job_tests_allowed()"),
            "{signature} must return early unless print_job_tests_allowed()"
        );
    }
}

#[test]
fn guard_helper_is_compile_time_not_environment_driven() {
    let source = integration_source();
    let helper = source
        .find("fn print_job_tests_allowed()")
        .map(|start| &source[start..start + 400])
        .expect("guard helper exists");
    assert!(
        helper.contains("cfg!(feature = \"allow-print-job-tests\")"),
        "the guard must be a compile-time feature check"
    );
    assert!(
        !helper.contains("env::var") && !helper.contains("env!"),
        "the guard must not be switchable by an environment variable at runtime"
    );
}
