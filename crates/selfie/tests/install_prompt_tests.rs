use std::time::{Duration, Instant};

use tempfile::TempDir;

use selfie::{
    commands::{PromptHandler, ShellCommandRunner, TerminalAnswer},
    package::{
        event::{CheckFailure, OperationFailure, OperationResult, PackageEvent},
        service::{InstallOptions, PackageService},
    },
};

// The install succeeds only for the right password, which it reads from the
// terminal and not from stdin, and records that it did by creating the file the
// check looks for.
const INSTALL: &str = r#"printf 'Password: ' > /dev/tty; IFS= read -r pw < /dev/tty; [ "$pw" = open-sesame ] && touch installed"#;

fn runner() -> ShellCommandRunner {
    ShellCommandRunner::new(ShellCommandRunner::default_shell(), Duration::from_secs(30))
}

// Install `pkg`, whose spec lives in `temp`, with `runner` running its commands.
async fn install(temp: &TempDir, runner: ShellCommandRunner) -> Vec<PackageEvent> {
    std::fs::write(
        temp.path().join("pkg.yml"),
        format!(
            "name: pkg\nenvironments:\n  test:\n    check: \"test -f installed\"\n    install: |-\n      {INSTALL}\n"
        ),
    )
    .unwrap();
    let service = test_common::create_service_test_service_with_runner(temp, runner);

    test_common::collect_events(service.install("pkg", InstallOptions::default()).await).await
}

fn completed(events: &[PackageEvent]) -> &OperationResult {
    test_common::get_operation_result(events).expect("the install should complete")
}

#[tokio::test]
async fn an_install_that_asks_for_a_password_gets_the_answer_and_finishes() {
    let temp = TempDir::new().unwrap();
    let handler =
        PromptHandler::new(|_| async { Some(TerminalAnswer::new("open-sesame".to_string())) });

    let events = install(&temp, runner().with_prompt_handler(handler)).await;

    assert!(
        matches!(completed(&events), OperationResult::Success(_)),
        "the install did not finish: {:?}",
        completed(&events)
    );
    // The control: the password was checked, since the file exists only when the
    // command read the right one.
    assert!(temp.path().join("installed").exists());
}

#[tokio::test]
async fn an_install_that_asks_and_cannot_be_answered_fails_naming_the_question() {
    let temp = TempDir::new().unwrap();
    let started = Instant::now();

    let events = install(&temp, runner()).await;

    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the install waited {:?} for an answer",
        started.elapsed()
    );
    let OperationResult::Failure(OperationFailure::Check(failure)) = completed(&events) else {
        panic!("expected an input failure, got {:?}", completed(&events));
    };
    assert!(
        matches!(failure, CheckFailure::InputRequired { .. }),
        "got {failure:?}"
    );
    assert_eq!(failure.kind(), "input_required");
    assert!(failure.to_string().contains("Password:"), "{failure}");
    assert!(!temp.path().join("installed").exists());
}
