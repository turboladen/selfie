// A timed-out install's output never reaches `tracing`, which bypasses the
// event stream. This is the only test in its binary, so no other test thread
// reaches the same call sites while the capture is being set up.

mod install_timeout;

use install_timeout::{SECRET, install};
use tempfile::TempDir;

// Current-thread, so the relay task the install spawns runs on the thread the
// capture is installed on.
#[tokio::test(flavor = "current_thread")]
async fn a_timed_out_installs_output_never_reaches_tracing() {
    let dir = TempDir::new().unwrap();
    // Reaches every call site an install's timeout does before the capture
    // starts, as `capture_tracing` requires.
    install(dir.path(), "warmup", "echo warm; echo warm >&2; sleep 4").await;
    let log = test_common::capture_tracing(tracing::Level::TRACE);

    let events = install(dir.path(), "app", test_common::LONG_THEN_QUIET).await;

    let logged = log.text();
    test_common::assert_secret_free(&logged, SECRET, "the tracing log");
    assert!(!logged.is_empty(), "nothing was logged");
    assert!(
        test_common::relays_output_containing(&events, SECRET),
        "the install relayed no secret, so the test observed nothing"
    );
}
