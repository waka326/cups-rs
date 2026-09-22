use cups_rs::*;

pub fn setup_test_environment() {
    std::env::set_var("CUPS_SERVER", "localhost");
}

pub fn create_test_document() -> Vec<u8> {
    format!(
        "Test Document\n=============\n\nGenerated at: {}\n\nThis is a test document for CUPS integration testing.\n",
        chrono::Utc::now()
    ).into_bytes()
}

/// Cancel jobs left behind by the print-job tests.
///
/// Gated on the same feature as the tests that create them: without it this
/// does nothing, so a default test run can never cancel a job that belongs to
/// whoever owns the machine.
#[cfg(feature = "allow-print-job-tests")]
pub fn cleanup_test_jobs() {
    if let Ok(jobs) = get_active_jobs(None) {
        for job in jobs {
            if job.title.contains("Test") || job.title.contains("Integration") {
                println!("Cleaning up test job: {}", job.id);
                let _ = cancel_job(job.id);
            }
        }
    }
}

/// Without the opt-in feature there are no test jobs to clean up, because none
/// were ever created.
#[cfg(not(feature = "allow-print-job-tests"))]
pub fn cleanup_test_jobs() {}

/// Whether this build is allowed to create real CUPS print jobs.
///
/// Every test that could reach a printer checks this first. It is a compile
/// -time constant, so the default build cannot be talked into printing at
/// runtime by an environment variable or a stray configuration file.
pub const PRINT_JOB_TESTS_ENABLED: bool = cfg!(feature = "allow-print-job-tests");
