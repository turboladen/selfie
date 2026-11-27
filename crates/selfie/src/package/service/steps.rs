use std::borrow::Cow;

use tokio_util::sync::CancellationToken;

use crate::{
    commands::runner::{
        CommandError, CommandRunner, StreamedExit, StreamedOutput, StreamedTail, TailWindows,
        TimedOut,
    },
    config::SelfieConfig,
    package::{
        EnvironmentConfig, GetPackage,
        event::{
            ComposedWarning, ConsoleOutput, EventSender, OperationFailure, OperationResult,
            StepEnding,
        },
        port::{PackageError, PackageRepoError, PackageRepository},
        service::ProgressTracker,
    },
};

/// Step to fetch a package from the repository
pub async fn fetch_package<PR>(
    repo: &PR,
    package_name: &str,
    sender: &EventSender,
    progress: &mut ProgressTracker,
) -> Result<GetPackage, PackageRepoError>
where
    PR: PackageRepository,
{
    progress
        .next(sender, format!("Fetching package: {package_name}"))
        .await;

    match repo.get_package(package_name) {
        Ok(package) => {
            sender.send_trace("Package found").await;
            Ok(package)
        }
        Err(e) => Err(crate::package::port::with_name_hint(repo, e)),
    }
}

/// The failure a single-package command reports for a spec selfie will not
/// read, or `None` when the spec is usable in `environment`.
///
/// Ask this after loading the package and before looking up any command in it.
pub(super) fn refuse_unreadable_spec(
    package_name: &str,
    package_blob: &GetPackage,
    environment: &str,
) -> Option<OperationResult> {
    // Asked before the environment's command is looked up, because that lookup is
    // the harm: a key shadowing `environments:` makes it miss or find a decoy, and
    // what runs is not what the file's author wrote.
    //
    // A failure rather than a skip. Apply works through every package and carries
    // on past one it will not touch; the commands asking here were given a single
    // package name, so there is nothing else to do that would make exiting 0 true.
    //
    // Shared, so a fourth command cannot answer differently about the same file.
    let refusal = package_blob.package.spec_refusal(environment)?;

    Some(OperationResult::Failure(OperationFailure::UnreadableSpec {
        package_name: package_name.to_string(),
        reason: crate::parts::Message::of(&refusal),
    }))
}

/// Shared step: build an `EnvironmentNotFound` error when the current
/// environment isn't defined in the package file.
#[allow(clippy::result_large_err)] // OperationResult is the standard error type across the service layer
pub(super) fn handle_missing_environment<T>(
    package_name: &str,
    package_blob: &GetPackage,
    current_env: &str,
) -> Result<T, Box<OperationResult>> {
    let err = missing_environment(package_name, package_blob, current_env);
    Err(Box::new(OperationResult::Failure(err.into())))
}

/// The `EnvironmentNotFound` error for a package file that does not define
/// `current_env`.
pub(super) fn missing_environment(
    package_name: &str,
    package_blob: &GetPackage,
    current_env: &str,
) -> PackageError {
    PackageError::EnvironmentNotFound {
        package_name: package_name.to_string(),
        environment: current_env.to_string(),
        available_environments: package_blob
            .package
            .environments()
            .keys()
            .cloned()
            .collect(),
        package_file: package_blob.package.path().clone(),
        required_by: None,
    }
}

/// Step to get a specific command from environment config
pub async fn get_command<'a>(
    env_config: &'a EnvironmentConfig,
    command_type: &str,
    command_getter: impl FnOnce(&EnvironmentConfig) -> Option<&str>,
    sender: &EventSender,
    progress: &mut ProgressTracker,
) -> Result<&'a str, Cow<'static, str>> {
    progress
        .next(
            sender,
            format!("Checking if package has `{command_type}` command"),
        )
        .await;

    if let Some(cmd) = command_getter(env_config) {
        sender
            .send_trace(format!("Package has `{command_type}` command"))
            .await;
        Ok(cmd)
    } else {
        progress
            .next(
                sender,
                format!("Package does not have `{command_type}` command"),
            )
            .await;
        Err(format!("No {command_type} command defined").into())
    }
}

/// Step to execute a command in the package directory, streaming its output
/// for real-time feedback. `package_name` names the package the command
/// belongs to, in its step.
#[allow(clippy::too_many_arguments)]
pub async fn execute_command_streaming<CR>(
    command_runner: &CR,
    package_name: &str,
    cmd: &str,
    command_type: &str,
    config: &SelfieConfig,
    sender: &EventSender,
    progress: &mut ProgressTracker,
    token: &CancellationToken,
) -> Result<StreamedOutput, StreamingFailure>
where
    CR: CommandRunner,
{
    use crate::commands::runner::OutputChunk;
    use tokio::sync::mpsc;

    // Check for potentially unsafe multi-line scripts and send warning if needed
    check_command_safety(cmd, sender).await;

    let is_final_execution = progress.current_step() + 1 == progress.total_steps();

    let step = progress
        .next_waiting(
            sender,
            format!("Running the {command_type} command for {package_name}"),
        )
        .await;

    // Create a channel for streaming output
    let (tx, mut rx) = mpsc::channel::<OutputChunk>(1000);

    // Clone the sender for the async task
    let sender_clone = sender.clone();

    // Spawn a task to handle the streaming output. It keeps the ends of each
    // pipe, and nothing more, for a command that runs into its timeout.
    let output_task = tokio::spawn(async move {
        let (mut stdout, mut stderr) = (TailWindows::new(), TailWindows::new());
        while let Some(chunk) = rx.recv().await {
            match chunk {
                OutputChunk::Stdout(line) => {
                    stdout.push(line.as_bytes());
                    sender_clone
                        .send_info(step, ConsoleOutput::Stdout(line))
                        .await;
                }
                OutputChunk::Stderr(line) => {
                    stderr.push(line.as_bytes());
                    sender_clone
                        .send_info(step, ConsoleOutput::Stderr(line))
                        .await;
                }
            }
        }
        (stdout, stderr)
    });

    // The package directory is the child's working directory, never a `cd`
    // pasted in front of the command. `cd dir && cmd` guards only the command's
    // first line: if the `cd` fails, the later lines run wherever selfie started.
    let result = command_runner
        .execute_streaming(
            cmd,
            config.package_directory(),
            config.command_timeout(),
            tx,
            token,
        )
        .await;

    let windows = report_relay_end(output_task.await, sender).await;

    // Ended before anything else is said about the command, so a consumer
    // closes the step before it reports the result.
    sender
        .send_step_ended(
            step,
            ending_of(&result, StreamedExit::is_success, StepEnding::Failed),
        )
        .await;

    match result {
        Ok(output) => {
            if output.is_success() {
                if is_final_execution {
                    sender
                        .send_debug("Final command execution completed successfully")
                        .await;
                }
            } else {
                // A command killed by a signal has no exit code to name.
                let how = output
                    .code()
                    .map_or_else(String::new, |code| format!(" with exit code {code}"));
                sender
                    .send_warning(format!("The `{command_type}` command failed{how}"))
                    .await;
            }
            Ok(StreamedOutput::new(output))
        }
        Err(error) => Err(StreamingFailure::of(error, windows)),
    }
}

/// How a streamed command failed without exiting.
#[derive(Debug)]
pub(crate) enum StreamingFailure {
    /// The command ran into its timeout, and `tail` is what it had printed.
    TimedOut {
        timed_out: TimedOut,
        tail: StreamedTail,
    },
    /// Any other runner failure, which carries none of the command's output.
    Command(CommandError),
}

impl StreamingFailure {
    // Only a timeout keeps the tail. A cancel is the user's own act, and a
    // command that asked for input already names its question. A relay that did
    // not finish leaves no tail, so the timeout is reported with no pipes rather
    // than with empty ones, which would say the command printed nothing.
    // The windows are bounded only here, so a command that succeeds never pays
    // for it.
    fn of(error: CommandError, windows: Option<(TailWindows, TailWindows)>) -> Self {
        match (error, windows) {
            (
                CommandError::Timeout {
                    command,
                    timeout,
                    limit,
                    ..
                },
                Some((stdout, stderr)),
            ) => StreamingFailure::TimedOut {
                timed_out: TimedOut::new(command, timeout, limit),
                tail: StreamedTail::new(stdout, stderr),
            },
            (error, _) => StreamingFailure::Command(error),
        }
    }
}

impl From<StreamingFailure> for OperationFailure {
    fn from(failure: StreamingFailure) -> Self {
        match failure {
            StreamingFailure::TimedOut { timed_out, tail } => {
                OperationFailure::install_timed_out(timed_out, tail)
            }
            StreamingFailure::Command(error) => error.into(),
        }
    }
}

/// Warn when the task relaying a command's output did not finish cleanly, and
/// give back the ends of each pipe when it did.
///
/// The warning is a fixed sentence and never the [`JoinError`] itself.
///
/// [`JoinError`]: tokio::task::JoinError
// A `JoinError`'s `Display` renders the panic payload, and the task that panicked
// was holding the command's output, so the payload can be derived from a
// credential. Only `is_cancelled` is read.
async fn report_relay_end(
    result: Result<(TailWindows, TailWindows), tokio::task::JoinError>,
    sender: &EventSender,
) -> Option<(TailWindows, TailWindows)> {
    let join_error = match result {
        Ok(windows) => return Some(windows),
        Err(join_error) => join_error,
    };
    let message = if join_error.is_cancelled() {
        "The task relaying the command's output was canceled, so some of it may not have been shown"
    } else {
        "The task relaying the command's output failed, so some of it may not have been shown"
    };
    sender
        .send_composed_warning(ComposedWarning::OutputLost(
            crate::parts::Parts::new().text(message).into(),
        ))
        .await;
    None
}

/// How a waiting step that ran a command ended, given what the command
/// returned and whether `succeeded` says its output is a zero exit.
/// `on_nonzero` is the ending for a non-zero exit: a failure for an install
/// or an audit.
pub(crate) fn ending_of<T>(
    result: &Result<T, CommandError>,
    succeeded: impl Fn(&T) -> bool,
    on_nonzero: StepEnding,
) -> StepEnding {
    match result {
        Ok(output) if succeeded(output) => StepEnding::Succeeded,
        Ok(_) => on_nonzero,
        Err(CommandError::Canceled { .. }) => StepEnding::Canceled,
        Err(_) => StepEnding::Failed,
    }
}

/// Checks shell commands for potential safety issues and sends warnings via events
///
/// For multi-line commands without proper error handling, this function sends
/// a warning event suggesting the user add `set -e` or similar error handling to
/// ensure the script exits immediately if any command fails.
///
/// This helps users write safer package installation scripts without
/// automatically modifying their commands.
async fn check_command_safety(command: &str, sender: &EventSender) {
    let trimmed = command.trim();

    // Check if this is a multi-line command (contains newlines after trimming)
    if trimmed.contains('\n') {
        // Check if the command already has error handling
        let lines: Vec<&str> = trimmed.lines().collect();
        let first_meaningful_line = lines
            .iter()
            .find(|line| !line.trim().is_empty() && !line.trim().starts_with('#'))
            .map(|line| line.trim());

        let has_error_handling = if let Some(first_line) = first_meaningful_line {
            first_line.starts_with("set -e")
                || first_line.starts_with("set -o errexit")
                || lines.iter().any(|line| {
                    let trimmed_line = line.trim();
                    trimmed_line == "set -e" || trimmed_line == "set -o errexit"
                })
        } else {
            false
        };

        if !has_error_handling {
            sender
                .send_warning(
                    "Multi-line shell command detected without error handling. \
                        Consider adding 'set -e' at the beginning of your script to \
                        ensure it exits on the first command failure. This prevents \
                        subsequent commands from running after a failure.",
                )
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::event::{
        EventSender, OperationContext, PackageEvent, metadata::OperationType,
    };
    use tokio::sync::mpsc;

    // The relay task holds the command's output, so a panic payload can be
    // derived from a credential. Neither the warning event nor its tracing
    // mirror may carry it.
    #[tokio::test(flavor = "current_thread")]
    async fn a_relay_panic_reports_no_payload() {
        const SECRET: &str = "s3cr3t-v4lue-DO-NOT-LEAK";
        let (tx, mut rx) = mpsc::channel::<PackageEvent>(10);
        let sender = EventSender::new_with_context(
            tx,
            OperationType::PackageInstall,
            "test".to_string(),
            "test".to_string(),
            OperationContext::default(),
        );
        let log = test_common::capture_tracing(tracing::Level::TRACE);
        let panicked: Result<(TailWindows, TailWindows), _> =
            tokio::spawn(async { panic!("relay held TOKEN={SECRET}") }).await;

        let tail = report_relay_end(panicked, &sender).await;
        drop(sender);

        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        test_common::assert_secret_free(&format!("{events:?}"), SECRET, "the events");
        let logged = log.text();
        test_common::assert_secret_free(&logged, SECRET, "the tracing log");
        // Positive controls: the failure was reported, once, and logged.
        let warnings = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    PackageEvent::ComposedWarning {
                        warning: ComposedWarning::OutputLost(_),
                        ..
                    }
                )
            })
            .count();
        assert_eq!(warnings, 1, "{events:?}");
        assert!(!logged.is_empty(), "nothing was logged");
        assert!(tail.is_none(), "{tail:?}");
    }

    fn timeout(limit: crate::commands::runner::TimeLimit) -> CommandError {
        CommandError::Timeout {
            command: "install-app".to_string(),
            timeout: std::time::Duration::from_secs(2),
            limit,
            working_directory: "/p".into(),
        }
    }

    fn windows_of(stdout: &str, stderr: &str) -> (TailWindows, TailWindows) {
        let (mut out, mut err) = (TailWindows::new(), TailWindows::new());
        out.push(stdout.as_bytes());
        err.push(stderr.as_bytes());
        (out, err)
    }

    // A relay that did not finish leaves no tail, and a timeout without one is
    // reported as the runner's own error, with no pipes, rather than as a
    // command that printed nothing.
    #[tokio::test]
    async fn a_relay_that_failed_gives_a_timeout_with_no_tail() {
        let (tx, mut rx) = mpsc::channel::<PackageEvent>(10);
        let sender = EventSender::new_with_context(
            tx,
            OperationType::PackageInstall,
            "test".to_string(),
            "test".to_string(),
            OperationContext::default(),
        );
        let panicked: Result<(TailWindows, TailWindows), _> =
            tokio::spawn(async { panic!("the relay failed") }).await;

        let tail = report_relay_end(panicked, &sender).await;

        assert!(tail.is_none(), "{tail:?}");
        assert!(matches!(
            rx.recv().await,
            Some(PackageEvent::ComposedWarning {
                warning: ComposedWarning::OutputLost(_),
                ..
            })
        ));
        let failure =
            StreamingFailure::of(timeout(crate::commands::runner::TimeLimit::Silence), tail);
        assert!(
            matches!(
                failure,
                StreamingFailure::Command(CommandError::Timeout { .. })
            ),
            "{failure:?}"
        );
        // Control: a relay that finished does give its windows back.
        let finished = tokio::spawn(async { (TailWindows::new(), TailWindows::new()) }).await;
        assert!(report_relay_end(finished, &sender).await.is_some());
    }

    // Only a timeout carries the tail; any other runner error drops it.
    #[test]
    fn an_error_other_than_a_timeout_carries_no_tail() {
        let canceled = CommandError::Canceled {
            command: "install-app".to_string(),
            working_directory: "/p".into(),
        };

        let failure = StreamingFailure::of(canceled, Some(windows_of("out", "err")));

        assert!(
            matches!(
                failure,
                StreamingFailure::Command(CommandError::Canceled { .. })
            ),
            "{failure:?}"
        );
    }

    // A runner's total timeout carries the tail too, and each pipe's output
    // lands in its own field.
    #[tokio::test]
    async fn a_total_timeout_carries_both_pipes() {
        use crate::commands::runner::{MockCommandRunner, OutputChunk, TimeLimit};

        let mut runner = MockCommandRunner::new();
        runner
            .expect_execute_streaming()
            .returning(|_, _, _, output_sender, _| {
                output_sender
                    .try_send(OutputChunk::Stdout("out-a\n".to_string()))
                    .unwrap();
                output_sender
                    .try_send(OutputChunk::Stderr("err-b\n".to_string()))
                    .unwrap();
                Box::pin(async { Err(timeout(TimeLimit::Total)) })
            });
        let config = crate::config::SelfieConfigBuilder::default()
            .environment("test")
            .package_directory("/p")
            .build();
        let (tx, _rx) = mpsc::channel::<PackageEvent>(1000);
        let sender = EventSender::new_with_context(
            tx,
            OperationType::PackageInstall,
            "test".to_string(),
            "test".to_string(),
            OperationContext::default(),
        );
        let mut progress = crate::package::service::ProgressTracker::new(1);

        let failure = execute_command_streaming(
            &runner,
            "app",
            "install-app",
            "install",
            &config,
            &sender,
            &mut progress,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();

        let StreamingFailure::TimedOut { timed_out, tail } = failure else {
            panic!("expected a timeout with its tail, got {failure:?}");
        };
        assert_eq!(timed_out.limit(), TimeLimit::Total);
        assert_eq!(tail.stdout().as_str(), "out-a\n");
        assert_eq!(tail.stderr().as_str(), "err-b\n");
    }

    #[tokio::test]
    async fn test_check_command_safety_single_line() {
        let (tx, _rx) = mpsc::channel::<PackageEvent>(10);
        let sender = EventSender::new_with_context(
            tx,
            OperationType::PackageInstall,
            "test".to_string(),
            "test".to_string(),
            OperationContext::default(),
        );

        // Single line commands should not trigger warnings
        check_command_safety("echo hello", &sender).await;
        // This test just ensures no panic occurs
    }

    #[tokio::test]
    async fn test_check_command_safety_multiline_with_set_e() {
        let (tx, _rx) = mpsc::channel::<PackageEvent>(10);
        let sender = EventSender::new_with_context(
            tx,
            OperationType::PackageInstall,
            "test".to_string(),
            "test".to_string(),
            OperationContext::default(),
        );

        // Multi-line commands with set -e should not trigger warnings
        let command = "set -e\necho hello\necho world";
        check_command_safety(command, &sender).await;
        // This test just ensures no panic occurs
    }

    #[tokio::test]
    async fn test_check_command_safety_multiline_with_set_o_errexit() {
        let (tx, _rx) = mpsc::channel::<PackageEvent>(10);
        let sender = EventSender::new_with_context(
            tx,
            OperationType::PackageInstall,
            "test".to_string(),
            "test".to_string(),
            OperationContext::default(),
        );

        // Multi-line commands with set -o errexit should not trigger warnings
        let command = "set -o errexit\necho hello\necho world";
        check_command_safety(command, &sender).await;
        // This test just ensures no panic occurs
    }

    #[tokio::test]
    async fn test_check_command_safety_multiline_set_e_in_middle() {
        let (tx, _rx) = mpsc::channel::<PackageEvent>(10);
        let sender = EventSender::new_with_context(
            tx,
            OperationType::PackageInstall,
            "test".to_string(),
            "test".to_string(),
            OperationContext::default(),
        );

        // Multi-line commands with set -e in the middle should not trigger warnings
        let command = "echo start\nset -e\necho hello\necho world";
        check_command_safety(command, &sender).await;
        // This test just ensures no panic occurs
    }

    #[tokio::test]
    async fn test_check_command_safety_multiline_without_error_handling() {
        let (tx, mut rx) = mpsc::channel::<PackageEvent>(10);
        let sender = EventSender::new_with_context(
            tx,
            OperationType::PackageInstall,
            "test".to_string(),
            "test".to_string(),
            OperationContext::default(),
        );

        // Multi-line commands without error handling should trigger warnings
        let command = "echo hello\necho world\necho goodbye";
        check_command_safety(command, &sender).await;

        // Check that a warning event was sent
        if let Ok(event) = rx.try_recv() {
            match event {
                PackageEvent::Warning { message, .. } => {
                    assert!(message.contains("Multi-line shell command detected"));
                }
                _ => panic!("Expected warning event, got: {event:?}"),
            }
        } else {
            panic!("Expected warning event to be sent");
        }
    }

    #[tokio::test]
    async fn test_check_command_safety_complex_multiline() {
        let (tx, mut rx) = mpsc::channel::<PackageEvent>(10);
        let sender = EventSender::new_with_context(
            tx,
            OperationType::PackageInstall,
            "test".to_string(),
            "test".to_string(),
            OperationContext::default(),
        );

        // Complex multi-line script without error handling
        let command = r"#!/bin/bash
# Install some package
curl -o package.tar.gz https://example.com/package.tar.gz
tar -xzf package.tar.gz
cd package
./install.sh
cd ..
rm -rf package package.tar.gz";
        check_command_safety(command, &sender).await;

        // Check that a warning event was sent
        if let Ok(event) = rx.try_recv() {
            match event {
                PackageEvent::Warning { message, .. } => {
                    assert!(message.contains("Multi-line shell command detected"));
                }
                _ => panic!("Expected warning event, got: {event:?}"),
            }
        } else {
            panic!("Expected warning event to be sent");
        }
    }

    // Install commands run through a real `/bin/sh`, in a package directory the
    // test controls.
    mod package_directory {
        use std::path::Path;
        use std::time::Duration;

        use super::*;
        use crate::commands::ShellCommandRunner;
        use crate::config::SelfieConfigBuilder;

        // Run `cmd` as an install command with `package_dir` configured.
        async fn install(
            package_dir: &Path,
            cmd: &str,
        ) -> Result<StreamedOutput, StreamingFailure> {
            let config = SelfieConfigBuilder::default()
                .environment("test")
                .package_directory(package_dir)
                .build();
            let runner = ShellCommandRunner::new(
                ShellCommandRunner::default_shell(),
                Duration::from_secs(5),
            );
            let (tx, _rx) = mpsc::channel::<PackageEvent>(1000);
            let sender = EventSender::new_with_context(
                tx,
                OperationType::PackageInstall,
                "test".to_string(),
                "test".to_string(),
                OperationContext::default(),
            );
            let mut progress = crate::package::service::ProgressTracker::new(1);

            execute_command_streaming(
                &runner,
                "bat",
                cmd,
                "install",
                &config,
                &sender,
                &mut progress,
                &CancellationToken::new(),
            )
            .await
        }

        fn assert_names(failure: &StreamingFailure, dir: &Path) {
            let StreamingFailure::Command(error @ CommandError::WorkingDirectoryUnusable(_)) =
                failure
            else {
                panic!("expected WorkingDirectoryUnusable, got: {failure:?}");
            };
            let rendered = error.to_string();
            assert!(
                rendered.contains(&dir.display().to_string()),
                "the failure must name the directory: {rendered}"
            );
        }

        #[tokio::test]
        async fn a_multi_line_install_in_a_missing_package_directory_runs_no_line() {
            let temp = tempfile::tempdir().unwrap();
            let missing = temp.path().join("packages");
            let markers = tempfile::tempdir().unwrap();
            let (command, first, second) = test_common::two_marking_lines(markers.path());

            let error = install(&missing, &command).await.unwrap_err();

            assert_names(&error, &missing);
            assert!(!first.exists(), "the first line ran");
            assert!(!second.exists(), "the second line ran");
        }

        #[tokio::test]
        async fn a_multi_line_install_in_an_unenterable_package_directory_runs_no_line() {
            let temp = tempfile::tempdir().unwrap();
            let locked = test_common::LockedDir::create(&temp.path().join("packages"), 0o000);
            if !locked.holds() {
                eprintln!(
                    "SKIP a_multi_line_install_in_an_unenterable_package_directory_runs_no_line: \
                     still readable"
                );
                return;
            }
            let markers = tempfile::tempdir().unwrap();
            let (command, first, second) = test_common::two_marking_lines(markers.path());

            let error = install(locked.path(), &command).await.unwrap_err();

            assert_names(&error, locked.path());
            assert!(!first.exists(), "the first line ran");
            assert!(!second.exists(), "the second line ran");
        }

        #[tokio::test]
        async fn an_install_runs_in_a_package_directory_whose_name_has_a_quote_and_a_space() {
            let temp = tempfile::tempdir().unwrap();
            let package_dir = temp.path().join("it's a dir");
            std::fs::create_dir(&package_dir).unwrap();
            std::fs::write(package_dir.join("here.marker"), "").unwrap();

            let output = install(&package_dir, "test -f ./here.marker")
                .await
                .unwrap();

            assert!(output.is_success(), "stderr: {}", output.stderr());
        }
    }
}
