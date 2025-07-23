//! Shell command runner adapter implementation
//!
//! This module provides a concrete implementation of the `CommandRunner` trait
//! that executes commands through a system shell. It supports both blocking
//! and streaming execution modes with configurable timeouts.

use std::{
    path::Path,
    process::{Output, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;

use tokio::{io::AsyncReadExt, process::Command};

use super::runner::{CommandError, CommandOutput, CommandRunner, OutputChunk};

/// Shell command runner implementation
///
/// Executes commands using a system shell (e.g., `/bin/sh`, `/bin/bash`).
/// Provides both simple execution and streaming output capabilities with
/// configurable timeouts and working directory support.
#[derive(Clone, Debug)]
pub struct ShellCommandRunner {
    /// Path to the shell executable to use for command execution
    shell: String,

    /// Default timeout for commands when no explicit timeout is provided
    default_timeout: Duration,
}

impl ShellCommandRunner {
    /// Create a new shell command runner
    ///
    /// # Arguments
    ///
    /// * `shell` - Path to the shell executable (e.g., "/bin/sh", "/bin/bash")
    /// * `default_timeout` - Default timeout for command execution
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use selfie::commands::ShellCommandRunner;
    ///
    /// let runner = ShellCommandRunner::new("/bin/sh", Duration::from_secs(30));
    /// ```
    #[must_use]
    pub fn new(shell: &str, default_timeout: Duration) -> Self {
        Self {
            shell: shell.to_string(),
            default_timeout,
        }
    }
}

#[async_trait]
impl CommandRunner for ShellCommandRunner {
    /// Check if a command is available in the current environment
    ///
    /// Uses the shell's `command -v` to test if the specified command
    /// can be found in the current PATH. This is shell-agnostic and
    /// works across different Unix-like systems.
    ///
    /// # Arguments
    ///
    /// * `command` - The command name to check (e.g., "npm", "git", "python")
    ///
    /// # Returns
    ///
    /// `true` if the command is available, `false` otherwise
    async fn is_command_available(&self, command: &str) -> bool {
        // Shell-agnostic way to check if a command exists
        let check_cmd = format!("command -v {command} >/dev/null 2>&1");

        match self.execute(&check_cmd).await {
            Ok(output) => output.is_success(),
            Err(_) => false,
        }
    }

    /// Execute a command using the default timeout
    ///
    /// Runs the specified shell command and waits for completion, using
    /// the default timeout configured for this runner instance.
    ///
    /// # Arguments
    ///
    /// * `command` - The shell command to execute
    ///
    /// # Errors
    ///
    /// Returns [`CommandError`] if:
    /// - The command cannot be started (IO error)
    /// - The command times out (exceeds default timeout)
    /// - Any other execution error occurs
    async fn execute(&self, command: &str) -> Result<CommandOutput, CommandError> {
        self.execute_with_timeout(command, self.default_timeout)
            .await
    }

    /// Execute a command with a specific timeout
    ///
    /// Runs the specified shell command and waits for completion within
    /// the given timeout duration. The command will be terminated if it
    /// doesn't complete in time.
    ///
    /// # Arguments
    ///
    /// * `command` - The shell command to execute
    /// * `timeout` - Maximum duration to wait for completion
    ///
    /// # Errors
    ///
    /// Returns [`CommandError`] if:
    /// - The command cannot be started (IO error)
    /// - The command times out before completion
    /// - The shell returns an error executing the command
    async fn execute_with_timeout(
        &self,
        command: &str,
        timeout: Duration,
    ) -> Result<CommandOutput, CommandError> {
        let start_time = Instant::now();

        let mut cmd = Command::new(&self.shell);
        cmd.arg("-c").arg(command).stdin(Stdio::null());

        let working_directory =
            std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());

        // Execute the command within the context of a timeout
        let output = tokio::time::timeout(timeout, cmd.output())
            .await
            .map_err(|_| CommandError::Timeout {
                command: command.to_string(),
                timeout,
                working_directory: working_directory.clone(),
            })?
            .map_err(|e| CommandError::IoError {
                command: command.to_string(),
                working_directory: working_directory.clone(),
                source: Arc::new(e),
            })?;

        let duration = start_time.elapsed();

        Ok(CommandOutput { output, duration })
    }

    /// Execute a command with streaming output processing
    ///
    /// Runs the command and streams stdout/stderr output through the provided
    /// callback as it becomes available. This allows real-time processing of
    /// command output, which is useful for long-running commands or when
    /// providing user feedback.
    ///
    /// # Arguments
    ///
    /// * `command` - The shell command to execute
    /// * `timeout` - Maximum duration to wait for completion
    /// * `callback` - Function called with each chunk of output
    ///
    /// # Errors
    ///
    /// Returns [`CommandError`] if:
    /// - The command cannot be started (IO error)
    /// - The command times out before completion
    /// - Output stream handling fails
    /// - The callback function encounters an error
    async fn execute_streaming(
        &self,
        command: &str,
        timeout: Duration,
        output_sender: tokio::sync::mpsc::Sender<OutputChunk>,
    ) -> Result<CommandOutput, CommandError> {
        let start_time = Instant::now();

        let mut cmd = Command::new(&self.shell);
        cmd.arg("-c")
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                return Err(CommandError::IoError {
                    command: command.to_string(),
                    working_directory: Path::new(".").to_path_buf(),
                    source: Arc::new(e),
                });
            }
        };

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CommandError::StdoutSpawn(command.to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| CommandError::StderrSpawn(command.to_string()))?;

        let mut stdout = tokio::io::BufReader::new(stdout);
        let mut stderr = tokio::io::BufReader::new(stderr);

        let mut full_stdout = Vec::new();
        let mut full_stderr = Vec::new();

        let mut stdout_buf = vec![0; 1024]; // Buffer of 1024 bytes
        let mut stderr_buf = vec![0; 1024]; // Buffer of 1024 bytes

        let timeout_future = tokio::time::timeout(timeout, async {
            let mut stdout_done = false;
            let mut stderr_done = false;
            let mut process_done = false;
            let mut exit_status = None;

            loop {
                tokio::select! {
                    result = stdout.read(&mut stdout_buf), if !stdout_done => {
                        if handle_chunked_read_result_streaming(result, &mut full_stdout, &mut stdout_buf, &output_sender, OutputChunk::Stdout)? {
                            stdout_done = true;  // EOF reached
                        }
                    },
                    result = stderr.read(&mut stderr_buf), if !stderr_done => {
                        if handle_chunked_read_result_streaming(result, &mut full_stderr, &mut stderr_buf, &output_sender, OutputChunk::Stderr)? {
                            stderr_done = true;  // EOF reached
                        }
                    },
                    status = child.wait(), if !process_done => {
                        exit_status = Some(status.map_err(|e| CommandError::IoError {
                            command: command.to_string(),
                            working_directory: std::env::current_dir()
                                .unwrap_or_else(|_| Path::new(".").to_path_buf()),
                            source: Arc::new(e),
                        })?);
                        process_done = true;
                    }
                }

                // Exit when process is done AND both streams are done
                if process_done && stdout_done && stderr_done {
                    break;
                }
            }

            let duration = start_time.elapsed();
            Ok(CommandOutput {
                output: Output {
                    // SAFETY: exit_status is guaranteed to be Some(_) at this point because
                    // the loop only exits when process_done is true, which is only set to true
                    // after exit_status is assigned Some(status) from child.wait()
                    status: exit_status.unwrap(),
                    stdout: full_stdout,
                    stderr: full_stderr,
                },
                duration,
            })
        });

        if let Ok(result) = timeout_future.await {
            result
        } else {
            let _ = child.kill().await;
            Err(CommandError::Timeout {
                command: command.to_string(),
                timeout,
                working_directory: std::env::current_dir()
                    .unwrap_or_else(|_| Path::new(".").to_path_buf()),
            })
        }
    }
}

/// Handle the result of reading a chunk with real-time streaming callback
///
/// Processes the result of an async read operation, updating the full output
/// buffer and calling the callback immediately for real-time streaming.
///
/// # Arguments
///
/// * `result` - Result of the read operation
/// * `full_output` - Buffer to accumulate complete output
/// * `buffer` - Read buffer containing the latest chunk
/// * `output_sender` - Mutable sender channel for streaming chunks
/// * `output_type` - Function to wrap chunks as stdout or stderr
///
/// # Returns
///
/// Returns `Ok(true)` if EOF reached, `Ok(false)` to continue reading
///
/// # Errors
///
/// Returns [`CommandError`] if the read operation failed (IO error)
fn handle_chunked_read_result_streaming(
    result: Result<usize, tokio::io::Error>,
    full_output: &mut Vec<u8>,
    buffer: &mut [u8],
    output_sender: &tokio::sync::mpsc::Sender<OutputChunk>,
    output_type: fn(String) -> OutputChunk,
) -> Result<bool, CommandError> {
    match result {
        Ok(0) => Ok(true), // End of stream
        Ok(n) => {
            full_output.extend_from_slice(&buffer[..n]);
            let chunk = String::from_utf8_lossy(&buffer[..n]).to_string();
            // Send the chunk immediately for real-time streaming
            let _ = output_sender.try_send(output_type(chunk));
            // Note: Don't clear the buffer here - tokio reuses it for the next read
            Ok(false) // Continue reading
        }
        Err(e) => Err(CommandError::IoError {
            command: "streaming command".to_string(),
            working_directory: std::env::current_dir()
                .unwrap_or_else(|_| Path::new(".").to_path_buf()),
            source: Arc::new(e),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // These tests will actually run commands on the system
    // They could be skipped in CI environments if necessary
    #[tokio::test]
    async fn test_shell_command_runner_basic() {
        let runner = ShellCommandRunner::new("/bin/sh", Duration::from_secs(10));

        // Test a basic echo command
        let result = runner.execute("echo hello").await;
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.stdout_str().contains("hello"));
        assert!(output.is_success());

        // Test command failure
        let result = runner.execute("exit 1").await;
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(!output.is_success());
        assert_eq!(output.exit_code(), 1);
    }

    #[tokio::test]
    async fn test_command_availability() {
        let runner = ShellCommandRunner::new("/bin/sh", Duration::from_secs(10));

        // "echo" should be available in most environments
        assert!(runner.is_command_available("echo").await);

        // A random string should not be a valid command
        let random_cmd = "xyzabc123notarealcommand";
        assert!(!runner.is_command_available(random_cmd).await);
    }

    // This test relies on timing and could be flaky
    // Consider skipping or adjusting in CI environments
    #[tokio::test]
    async fn test_timeout() {
        let runner = ShellCommandRunner::new("/bin/sh", Duration::from_millis(100));

        // Command that should timeout (sleep for 1s)
        // Note: This is a simple test and may be flaky since timeouts aren't enforced
        // in a separate thread in our implementation
        let result = runner
            .execute_with_timeout("sleep 1", Duration::from_millis(10))
            .await;
        assert!(matches!(result, Err(CommandError::Timeout { .. })));
    }

    // Error handling tests
    #[tokio::test]
    async fn test_command_timeout_error() {
        let runner = ShellCommandRunner::new("/bin/sh", Duration::from_millis(50));

        // Create a command that will timeout
        let result = runner
            .execute_with_timeout("sleep 1", Duration::from_millis(10))
            .await;

        assert!(result.is_err());
        let error = result.unwrap_err();
        match error {
            CommandError::Timeout { .. } => {
                // Expected timeout error
            }
            _ => panic!("Expected CommandError::Timeout, got: {error:?}"),
        }
    }

    #[tokio::test]
    async fn test_command_io_error() {
        let runner = ShellCommandRunner::new("/bin/sh", Duration::from_secs(5));

        // Try to execute a command that doesn't exist
        let result = runner.execute("nonexistent_command_12345_xyz").await;

        // Command might succeed but with non-zero exit code, or fail
        if let Ok(output) = result {
            // If command executes, it should fail (non-zero exit code)
            assert!(!output.is_success());
        }
        // If result is Err, that's also acceptable for this test
    }

    #[tokio::test]
    async fn test_command_permission_denied() {
        let runner = ShellCommandRunner::new("/bin/sh", Duration::from_secs(5));

        // Try to access a file that should not be accessible
        let result = runner
            .execute("cat /root/.ssh/id_rsa 2>/dev/null || echo 'permission denied'")
            .await;

        // This should either succeed with "permission denied" message or fail
        // Either way, we're testing that the command runner handles the scenario
        if let Ok(output) = result {
            assert!(
                output.stdout_str().contains("permission denied")
                    || !output.stderr_str().is_empty()
            );
        }
        // If it fails, that's also acceptable for this test
    }

    #[tokio::test]
    async fn test_command_invalid_syntax() {
        let runner = ShellCommandRunner::new("/bin/sh", Duration::from_secs(5));

        // Try to execute a command with invalid syntax
        let result = runner.execute("if [ 1 -eq 1 ; then echo 'unclosed'").await;

        // This should fail due to invalid shell syntax
        if let Ok(output) = result {
            // Some shells might handle this gracefully
            assert!(!output.is_success());
        }
        // If it errors, that's also expected
    }

    #[tokio::test]
    async fn test_error_display_formatting() {
        // Test that our error types format correctly
        let timeout_error = CommandError::Timeout {
            command: "test-command".to_string(),
            timeout: Duration::from_millis(100),
            working_directory: PathBuf::from("/tmp"),
        };
        assert!(
            timeout_error
                .to_string()
                .contains("Command timed out after 100ms")
        );
        assert!(timeout_error.to_string().contains("test-command"));

        let io_error = std::io::Error::other("test error");
        let cmd_error = CommandError::IoError {
            command: "test-command".to_string(),
            working_directory: PathBuf::from("/tmp"),
            source: Arc::new(io_error),
        };
        assert!(cmd_error.to_string().contains("test-command"));

        let stdout_error = CommandError::StdoutSpawn("stdout issue".to_string());
        assert_eq!(
            stdout_error.to_string(),
            "Failed spawning stdout during command: stdout issue"
        );

        let stderr_error = CommandError::StderrSpawn("stderr issue".to_string());
        assert_eq!(
            stderr_error.to_string(),
            "Failed spawning stderr during command: stderr issue"
        );
    }

    #[tokio::test]
    async fn test_command_with_large_output() {
        let runner = ShellCommandRunner::new("/bin/sh", Duration::from_secs(5));

        // Generate a large amount of output to test buffering
        let result = runner
            .execute("for i in $(seq 1 1000); do echo \"Line $i\"; done")
            .await;

        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.is_success());
        assert!(output.stdout_str().lines().count() >= 1000);
    }

    #[tokio::test]
    async fn test_command_output_methods() {
        let runner = ShellCommandRunner::new("/bin/sh", Duration::from_secs(5));

        // Test that our output methods work correctly
        let result = runner.execute("echo 'test output'").await;

        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.is_success());
        assert!(output.stdout_str().contains("test output"));

        // Test that stderr_str() method exists and returns a string
        let _stderr = output.stderr_str(); // Just verify the method works
    }

    #[tokio::test]
    async fn test_command_exit_code_handling() {
        let runner = ShellCommandRunner::new("/bin/sh", Duration::from_secs(5));

        // Command that exits with non-zero status
        let result = runner.execute("exit 42").await;

        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(!output.is_success());
        assert_eq!(output.exit_code(), 42);
    }
}
