use std::borrow::Cow;

use tokio_util::sync::CancellationToken;

use crate::{
    commands::runner::{CommandError, CommandOutput, CommandRunner},
    config::SelfieConfig,
    package::{
        EnvironmentConfig, GetPackage, Package,
        event::{ConsoleOutput, EventSender, OperationFailure, OperationResult, RefusedSpec},
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
        Err(e) => Err(e),
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
        reason: refusal.to_string(),
    }))
}

/// The environments a listing shows, which decide when it refuses a spec.
#[derive(Debug, Clone, Copy)]
pub(super) enum Shown<'a> {
    /// The current environment only: a spec is refused when apply would refuse
    /// it here.
    Current(&'a str),
    /// Every environment: a spec is refused when a key in any of them, or at its
    /// top level, cannot be trusted.
    Every,
}

/// Split `packages` into the specs readable in the environments `shown` covers
/// and the ones refused there, each with its refusal.
pub(super) fn separate_refused<'a>(
    packages: impl IntoIterator<Item = &'a Package>,
    shown: Shown<'_>,
) -> (Vec<&'a Package>, Vec<(&'a Package, RefusedSpec)>) {
    let mut readable = Vec::new();
    let mut refused = Vec::new();
    for package in packages {
        let refusal = match shown {
            Shown::Current(environment) => package.spec_refusal(environment),
            Shown::Every => package.listing_refusal(),
        };
        match refusal {
            Some(reason) => refused.push((package, RefusedSpec::new(package, reason))),
            None => readable.push(package),
        }
    }
    (readable, refused)
}

/// Shared step: build an `EnvironmentNotFound` error when the current
/// environment isn't defined in the package file.
#[allow(clippy::result_large_err)] // OperationResult is the standard error type across the service layer
pub(super) fn handle_missing_environment<T>(
    package_name: &str,
    package_blob: &GetPackage,
    current_env: &str,
) -> Result<T, Box<OperationResult>> {
    let err = PackageError::EnvironmentNotFound {
        package_name: package_name.to_string(),
        environment: current_env.to_string(),
        available_environments: package_blob
            .package
            .environments()
            .keys()
            .cloned()
            .collect(),
        package_file: package_blob.package.path().clone(),
    };
    Err(Box::new(OperationResult::Failure(err.into())))
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

/// Step to execute a command with streaming output for real-time feedback
pub async fn execute_command_streaming<CR>(
    command_runner: &CR,
    cmd: &str,
    command_type: &str,
    config: &SelfieConfig,
    sender: &EventSender,
    progress: &mut ProgressTracker,
    token: &CancellationToken,
) -> Result<CommandOutput, CommandError>
where
    CR: CommandRunner,
{
    use crate::commands::runner::OutputChunk;
    use tokio::sync::mpsc;

    // Check for potentially unsafe multi-line scripts and send warning if needed
    check_command_safety(cmd, sender).await;

    let is_final_execution = progress.current_step() + 1 == progress.total_steps();
    let step_message = if is_final_execution {
        format!("Executing final `{command_type}` command: `{cmd}`")
    } else {
        format!("Executing package's `{command_type}` command: `{cmd}`")
    };

    progress.next(sender, step_message).await;

    // Create a channel for streaming output
    let (tx, mut rx) = mpsc::channel::<OutputChunk>(1000);

    // Clone the sender for the async task
    let sender_clone = sender.clone();

    // Spawn a task to handle the streaming output
    let output_task = tokio::spawn(async move {
        while let Some(chunk) = rx.recv().await {
            match chunk {
                OutputChunk::Stdout(line) => {
                    sender_clone.send_info(ConsoleOutput::Stdout(line)).await;
                }
                OutputChunk::Stderr(line) => {
                    sender_clone.send_info(ConsoleOutput::Stderr(line)).await;
                }
            }
        }
    });

    // Wrap the command to run in the package directory
    let package_dir = config.package_directory();
    let wrapped_cmd = format!("cd '{}' && {}", package_dir.display(), cmd);

    // Execute the wrapped command with streaming channel
    let result = command_runner
        .execute_streaming(&wrapped_cmd, config.command_timeout(), tx, token)
        .await;

    // Wait for the output task to finish and handle any task errors
    if let Err(join_error) = output_task.await {
        sender
            .send_warning(format!("Output streaming task failed: {join_error}"))
            .await;
    }

    match result {
        Ok(output) => {
            if output.is_success() {
                if is_final_execution {
                    sender
                        .send_debug(format!(
                            "Final command execution completed successfully (step {}/{})",
                            progress.current_step(),
                            progress.total_steps()
                        ))
                        .await;
                }
            } else {
                sender
                    .send_warning(format!(
                        "Command failed at step {}/{}: exit code {}",
                        progress.current_step(),
                        progress.total_steps(),
                        output.exit_code()
                    ))
                    .await;
            }
            Ok(output)
        }
        Err(error) => Err(error),
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
}
