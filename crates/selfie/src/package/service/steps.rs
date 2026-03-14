use std::borrow::Cow;

use crate::{
    commands::runner::{CommandError, CommandOutput, CommandRunner},
    config::AppConfig,
    package::{
        EnvironmentConfig, GetPackage,
        event::{ConsoleOutput, EventSender},
        port::{PackageRepoError, PackageRepository},
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
    config: &AppConfig,
    sender: &EventSender,
    progress: &mut ProgressTracker,
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
        .execute_streaming(&wrapped_cmd, config.command_timeout(), tx)
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
