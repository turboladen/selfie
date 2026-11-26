// An install that runs into its silence limit reports what it had printed by
// then: the first and last 1000 bytes of each pipe, and nothing between. These
// run real `/bin/sh` commands, each of which goes quiet with `sleep 4` past a
// 2-second limit, so a run that never times out fails on its result, never
// hangs.

mod install_timeout;

use std::time::Duration;

use install_timeout::SECRET;
use selfie::{
    commands::runner::{TimeLimit, TimedOut},
    package::event::{CommandFailure, OperationFailure, OperationResult, PackageEvent},
};
use tempfile::TempDir;

// Install `command` from a fresh directory.
async fn install(command: &str) -> Vec<PackageEvent> {
    let dir = TempDir::new().unwrap();
    install_timeout::install(dir.path(), "app", command).await
}

// The failure's timeout, stdout and stderr, when it is an install's timeout.
fn install_timed_out(events: &[PackageEvent]) -> (&TimedOut, &str, &str) {
    match test_common::get_operation_result(events) {
        Some(OperationResult::Failure(OperationFailure::CommandError(
            CommandFailure::InstallTimedOut {
                timed_out,
                stdout,
                stderr,
            },
        ))) => (timed_out, stdout.as_str(), stderr.as_str()),
        other => panic!("expected an install's timeout, got {other:?}"),
    }
}

fn failure(events: &[PackageEvent]) -> &OperationFailure {
    match test_common::get_operation_result(events) {
        Some(OperationResult::Failure(failure)) => failure,
        other => panic!("expected a failure, got {other:?}"),
    }
}

fn assert_stopped(timed_out: &TimedOut, command: &str) {
    assert_eq!(
        *timed_out,
        TimedOut::new(command, Duration::from_secs(2), TimeLimit::Silence)
    );
}

#[tokio::test]
async fn stdout_only_short() {
    let command = "echo out-line; sleep 4";
    let events = install(command).await;

    let (timed_out, stdout, stderr) = install_timed_out(&events);

    assert_stopped(timed_out, command);
    assert_eq!(stdout, "out-line\n");
    assert_eq!(stderr, "");
}

#[tokio::test]
async fn stderr_only_short() {
    let command = "echo 'waiting for lock' >&2; sleep 4";
    let events = install(command).await;

    let (timed_out, stdout, stderr) = install_timed_out(&events);

    assert_stopped(timed_out, command);
    assert_eq!(stdout, "");
    assert_eq!(stderr, "waiting for lock\n");
}

#[tokio::test]
async fn neither() {
    let events = install("sleep 4").await;

    let (timed_out, stdout, stderr) = install_timed_out(&events);

    assert_stopped(timed_out, "sleep 4");
    assert_eq!((stdout, stderr), ("", ""));
}

// Past the bound, each pipe keeps its two ends and names what it dropped, and
// a secret printed between them reaches no event but the streamed output.
#[tokio::test]
async fn both_long_with_the_secret_elided() {
    let events = install(test_common::LONG_THEN_QUIET).await;

    let (_, stdout, stderr) = install_timed_out(&events);

    for (pipe, tail) in [("stdout", stdout), ("stderr", stderr)] {
        assert!(tail.contains("bytes elided"), "{pipe}: {tail}");
        assert!(tail.starts_with("1\n2\n"), "{pipe}: {tail}");
        assert!(tail.ends_with("1300\n"), "{pipe}: {tail}");
    }
    let failure = failure(&events);
    test_common::assert_secret_free(&format!("{failure:?}"), SECRET, "the failure");
    test_common::assert_secret_free(&failure.to_string(), SECRET, "the failure's sentence");
    test_common::assert_secret_free(
        install_timed_out(&events).0.command(),
        SECRET,
        "the fixture's own command",
    );
    let others: Vec<&PackageEvent> = events
        .iter()
        .filter(|event| !matches!(event, PackageEvent::Info { .. }))
        .collect();
    test_common::assert_secret_free(&format!("{others:?}"), SECRET, "every other event");
    // Positive control: the command did print the secret, and the stream relayed it.
    assert!(
        test_common::relays_output_containing(&events, SECRET),
        "the install relayed no secret, so the test observed nothing"
    );
}

// The last bytes of each pipe are forwarded whole, so a secret among them
// reaches the failure.
#[tokio::test]
async fn a_secret_in_the_kept_tail_is_forwarded() {
    let command = "seq 1 1300; cat secret.txt; sleep 4";
    let events = install(command).await;

    let (_, stdout, _) = install_timed_out(&events);

    assert!(stdout.contains(SECRET), "{stdout}");
    assert!(stdout.contains("bytes elided"), "{stdout}");
    test_common::assert_secret_free(
        install_timed_out(&events).0.command(),
        SECRET,
        "the fixture's own command",
    );
}

// Just past the bound, the windows hold the whole text, and the secret between
// its two kept ends must still be cut. 1092 bytes come before the secret's 24,
// and 1040 after it, so it sits between the first and last 1000.
#[tokio::test]
async fn a_secret_between_the_windows_of_a_short_overflow() {
    let command = "seq 1 300; cat secret.txt; seq 301 560; sleep 4";
    let events = install(command).await;

    let (_, stdout, _) = install_timed_out(&events);

    assert!(stdout.contains("bytes elided"), "{stdout}");
    assert!(stdout.ends_with("560\n"), "{stdout}");
    let failure = failure(&events);
    test_common::assert_secret_free(&format!("{failure:?}"), SECRET, "the failure");
    test_common::assert_secret_free(&failure.to_string(), SECRET, "the failure's sentence");
    assert!(
        test_common::relays_output_containing(&events, SECRET),
        "the install relayed no secret, so the test observed nothing"
    );
}
