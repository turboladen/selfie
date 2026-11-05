// What an install does when its command asks a question on the terminal, as
// `sudo` does for a password. The question has to reach whoever can answer it,
// and when nobody can, the install has to fail instead of waiting.

use std::time::{Duration, Instant};

use futures::StreamExt;
use selfie::package::SpecOrigin;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use selfie::{
    commands::{PromptHandler, ShellCommandRunner, TerminalAnswer, TerminalReply},
    config::SelfieConfigBuilder,
    fs::RealFileSystem,
    package::{
        event::{CheckFailure, OperationFailure, OperationResult, PackageEvent},
        repository::YamlPackageRepository,
        service::{InstallOptions, PackageService, PackageServiceImpl},
    },
};

// The install succeeds only for the right password, which it reads from the
// terminal and not from stdin, and records that it did by creating the file the
// check looks for.
const INSTALL: &str = r#"printf 'Password: ' > /dev/tty; IFS= read -r pw < /dev/tty; [ "$pw" = open-sesame ] && touch installed"#;

fn service(temp: &TempDir, runner: ShellCommandRunner) -> impl PackageService {
    let package_dir = temp.path().to_path_buf();
    std::fs::write(
        package_dir.join("pkg.yml"),
        format!(
            "name: pkg\nenvironments:\n  test:\n    check: \"test -f installed\"\n    install: |-\n      {INSTALL}\n"
        ),
    )
    .unwrap();

    let config = SelfieConfigBuilder::default()
        .environment("test")
        .package_directory(&package_dir)
        .build();

    PackageServiceImpl::new(
        YamlPackageRepository::new(RealFileSystem, package_dir, SpecOrigin::PackageDirectory),
        YamlPackageRepository::new(
            RealFileSystem,
            config.dotfiles_directory(),
            SpecOrigin::DotfilesDirectory,
        ),
        runner,
        test_common::isolated_git_adapter(),
        config,
        CancellationToken::new(),
    )
}

fn runner() -> ShellCommandRunner {
    ShellCommandRunner::new(ShellCommandRunner::default_shell(), Duration::from_secs(30))
}

async fn install(service: impl PackageService) -> Vec<PackageEvent> {
    service
        .install("pkg", InstallOptions::default())
        .await
        .collect()
        .await
}

fn completed(events: &[PackageEvent]) -> &OperationResult {
    events
        .iter()
        .find_map(|event| match event {
            PackageEvent::Completed { result, .. } => Some(result),
            _ => None,
        })
        .expect("the install should complete")
}

#[tokio::test]
async fn an_install_that_asks_for_a_password_gets_the_answer_and_finishes() {
    let temp = TempDir::new().unwrap();
    let handler = PromptHandler::new(|_| async {
        TerminalReply::Answer(TerminalAnswer::new("open-sesame".to_string()))
    });

    let events = install(service(&temp, runner().with_prompt_handler(handler))).await;

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

    let events = install(service(&temp, runner())).await;

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
