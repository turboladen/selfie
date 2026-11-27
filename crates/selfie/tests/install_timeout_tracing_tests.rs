// A timed-out install's output never reaches `tracing`, which bypasses the
// event stream.

mod install_timeout;

use install_timeout::{SECRET, install};
use tempfile::TempDir;

// Current-thread, so the relay task the install spawns runs on the thread the
// capture is installed on.
#[tokio::test(flavor = "current_thread")]
async fn a_timed_out_installs_output_never_reaches_tracing() {
    let dir = TempDir::new().unwrap();
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
