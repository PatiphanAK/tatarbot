//! Helpers shared by tests.

/// Sends `tracing` events (info and up) to the test's captured output, so a failing test
/// shows what the code logged, such as ffmpeg's stderr and exit status. Idempotent.
pub fn trace_to_test_output() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_test_writer()
        .try_init();
}
