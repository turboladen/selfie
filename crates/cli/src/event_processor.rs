//! Event processing utilities for CLI commands
//!
//! This module provides a reusable event processor that can handle package events
//! from the selfie library and present them consistently across different CLI commands.
//!
//! # Usage
//!
//! ## Event Processing with Custom Handlers
//!
//! All commands use `process_events` which allows custom handling
//! of specific events while providing default behavior for standard events:
//!
//! ```rust,ignore
//! async fn handle_command_with_custom_progress(
//!     reporter: TerminalProgressReporter,
//!     event_stream: EventStream
//! ) -> i32 {
//!     let processor = EventProcessor::new(reporter);
//!
//!     processor.process_events(event_stream, |event| {
//!         match event {
//!             PackageEvent::Progress { percent_complete, step, total_steps, message, .. } => {
//!                 // Custom progress handling
//!                 println!("[{:.0}%] Step {}/{}: {}",
//!                     percent_complete * 100.0, step, total_steps, message);
//!                 true // Handled - continue processing
//!             }
//!             PackageEvent::Warning { message, .. } => {
//!                 // Custom warning handling
//!                 eprintln!("Warning: {}", message);
//!                 true // Handled - continue processing
//!             }
//!             _ => false, // Use default handling for other events
//!         }
//!     }).await
//! }
//! ```
//!
//! The custom handler should return:
//! - `true` to continue processing after handling the event (skip default handling)
//! - `false` to use the default handling for the event

use futures::StreamExt;
use selfie::package::{
    event::{ConsoleOutput, EventStream, OperationResult, PackageEvent, error::StreamedError},
    port::{PackageListError, PackageRepoError},
};

use crate::terminal_progress_reporter::TerminalProgressReporter;

/// Result of processing events, including metadata about what was encountered
#[derive(Debug, Clone)]
pub struct EventProcessingResult {
    /// The exit code for the operation
    pub exit_code: i32,
    /// Whether an environment configuration error was encountered and handled
    pub environment_error_handled: bool,
    /// Whether any errors were encountered during processing
    pub had_errors: bool,
}

impl EventProcessingResult {
    fn new() -> Self {
        Self {
            exit_code: 0,
            environment_error_handled: false,
            had_errors: false,
        }
    }
}

/// A reusable event processor for handling package operation events
///
/// This processor standardizes how events from the selfie library are handled
/// and displayed in the CLI, reducing boilerplate across different commands.
#[derive(Debug)]
pub struct EventProcessor {
    reporter: TerminalProgressReporter,
}

impl EventProcessor {
    /// Create a new event processor with the given reporter
    pub fn new(reporter: TerminalProgressReporter) -> Self {
        Self { reporter }
    }

    /// Process events from the stream with a custom event handler
    ///
    /// This allows commands to provide custom handling for specific event types
    /// while still getting the default behavior for standard events.
    ///
    /// The custom handler should return:
    /// - `true` if the event was handled (skip default handling)
    /// - `false` to use default handling for the event
    pub async fn process_events<F>(
        self,
        mut stream: EventStream,
        mut custom_handler: F,
    ) -> EventProcessingResult
    where
        F: FnMut(&PackageEvent) -> bool,
    {
        let mut result = EventProcessingResult::new();

        while let Some(event) = stream.next().await {
            // Check for environment errors before custom handling
            if let PackageEvent::Error { error, .. } = &event {
                if let selfie::package::event::error::StreamedError::PackageRepoError(
                    selfie::package::port::PackageRepoError::PackageError(pkg_error),
                ) = error
                {
                    if matches!(
                        pkg_error.as_ref(),
                        selfie::package::port::PackageError::EnvironmentNotFound { .. }
                    ) {
                        result.environment_error_handled = true;
                    }
                }
            }

            // Try custom handler first
            if custom_handler(&event) {
                // Custom handler handled the event, continue to next event
                continue;
            }

            // Fall back to default handling
            if self.handle_event(event, &mut result) {
                break;
            }
        }

        result
    }

    /// Handle a single event and update the result as needed
    ///
    /// Returns true if processing should stop (early termination)
    fn handle_event(&self, event: PackageEvent, result: &mut EventProcessingResult) -> bool {
        match event {
            PackageEvent::Started { operation_info } => {
                // Handle list operations differently since they don't have a specific package name
                let message = if operation_info.package_name.is_empty() {
                    format!(
                        "{} in environment '{}'",
                        operation_info.operation_type.to_string().to_title_case(),
                        operation_info.environment
                    )
                } else {
                    format!(
                        "{} package '{}' in environment '{}'",
                        operation_info.operation_type.to_string().to_title_case(),
                        operation_info.package_name,
                        operation_info.environment
                    )
                };
                self.reporter.report_info(message);
            }

            PackageEvent::Progress { message, .. } => {
                self.reporter.report_progress(message);
            }

            PackageEvent::Info { output, .. } => {
                handle_console_output(output);
            }

            PackageEvent::Trace { message, .. } => {
                tracing::trace!("{}", message);
            }

            PackageEvent::Debug { message, .. } => {
                tracing::debug!("{}", message);
            }

            PackageEvent::Warning { message, .. } => {
                self.reporter.report_warning(message);
                // Warnings don't set failure exit code by default
            }

            PackageEvent::Error { message, error, .. } => {
                // Check for specific error types that need special handling
                match &error {
                    StreamedError::PackageRepoError(PackageRepoError::PackageListError(
                        PackageListError::PackageDirectoryNotFound(path),
                    )) => {
                        // Handle this specific error case directly since it needs special formatting
                        self.reporter.report_error(format!(
                            "Package directory not found: {}",
                            path.display()
                        ));
                        self.reporter.report_suggestion(format!(
                            "Run 'selfie config --package-directory <path>' to set a different directory, or create the directory with 'mkdir -p {}'",
                            path.display()
                        ));
                    }
                    _ => {
                        self.reporter.report_error(format!("{message}: {error}"));
                    }
                }
                result.exit_code = 1;
                result.had_errors = true;
            }

            PackageEvent::Completed {
                result: op_result, ..
            } => match op_result {
                OperationResult::Success(success) => {
                    self.reporter.report_success(&success.to_string());
                }
                OperationResult::Failure(err) => {
                    self.reporter.report_error(&err.to_string());
                    result.exit_code = 1;
                    result.had_errors = true;
                }
            },

            PackageEvent::Canceled { reason, .. } => {
                self.reporter
                    .report_warning(format!("Operation canceled: {reason}"));
                result.exit_code = 1;
                result.had_errors = true;
                return true; // Stop processing after cancellation
            }

            PackageEvent::PackageInfoLoaded { .. }
            | PackageEvent::EnvironmentStatusChecked { .. }
            | PackageEvent::PackageListLoaded { .. }
            | PackageEvent::CheckResultCompleted { .. }
            | PackageEvent::ValidationResultCompleted { .. } => {
                // These structured events are handled by command-specific handlers
                // If no custom handler processed them, just continue
            }
        }

        false // Continue processing
    }
}

/// Handle console output appropriately
fn handle_console_output(output: ConsoleOutput) {
    match output {
        ConsoleOutput::Stdout(msg) => {
            println!("{msg}");
        }
        ConsoleOutput::Stderr(msg) => {
            eprintln!("{msg}");
        }
    }
}

/// Extension trait to add title case conversion to strings
trait ToTitleCase {
    fn to_title_case(&self) -> String;
}

impl ToTitleCase for str {
    fn to_title_case(&self) -> String {
        // Replace underscores with spaces and convert to title case
        let cleaned = self.replace('_', " ");
        let mut chars = cleaned.chars();
        match chars.next() {
            None => String::new(),
            Some(first) => {
                first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;

    #[test]
    fn test_to_title_case() {
        assert_eq!("check".to_title_case(), "Check");
        assert_eq!("install".to_title_case(), "Install");
        assert_eq!("VALIDATE".to_title_case(), "Validate");
        assert_eq!("".to_title_case(), "");
    }

    #[test]
    fn test_event_processor_creation() {
        let reporter = TerminalProgressReporter::new(false);
        let processor = EventProcessor::new(reporter);

        // Just verify it can be created
        assert!(std::mem::size_of_val(&processor) > 0);
    }

    #[tokio::test]
    async fn test_process_empty_stream() {
        let reporter = TerminalProgressReporter::new(false);
        let processor = EventProcessor::new(reporter);

        let events: Vec<PackageEvent> = vec![];
        let event_stream = Box::pin(stream::iter(events));
        let result = processor.process_events(event_stream, |_event| false).await;

        // Empty stream should return success
        assert_eq!(result.exit_code, 0);
        assert!(!result.environment_error_handled);
        assert!(!result.had_errors);
    }

    #[tokio::test]
    async fn test_custom_handler_behavior() {
        let reporter = TerminalProgressReporter::new(false);
        let processor = EventProcessor::new(reporter);

        let events: Vec<PackageEvent> = vec![];
        let event_stream = Box::pin(stream::iter(events));

        // Test that custom handler gets called with None for empty stream
        let mut handler_called = false;
        let result = processor
            .process_events(event_stream, |_event| {
                handler_called = true;
                true
            })
            .await;

        assert_eq!(result.exit_code, 0);
        // Handler should not be called for empty stream
        assert!(!handler_called);
    }

    #[tokio::test]
    async fn test_integration_with_actual_service() {
        use selfie::{
            commands::ShellCommandRunner,
            fs::real::RealFileSystem,
            package::{
                repository::YamlPackageRepository,
                service::{PackageService, PackageServiceImpl},
            },
        };

        // Create a minimal config for testing
        let config = selfie::config::AppConfigBuilder::default()
            .environment("test")
            .package_directory("/tmp/nonexistent")
            .command_timeout_unchecked(1)
            .use_colors(false)
            .build();

        let repo = YamlPackageRepository::new(RealFileSystem, config.package_directory().clone());
        let command_runner = ShellCommandRunner::new("/bin/sh", config.command_timeout());
        let service = PackageServiceImpl::new(repo, command_runner, config);

        let reporter = TerminalProgressReporter::new(false);
        let processor = EventProcessor::new(reporter);

        // Test with a nonexistent package - should get events but ultimately fail
        let event_stream = service.check("nonexistent-test-package").await;
        let result = processor.process_events(event_stream, |_event| false).await;

        // Should return error exit code since package doesn't exist
        assert_eq!(result.exit_code, 1);
        assert!(result.had_errors);
    }

    #[tokio::test]
    async fn test_custom_handler_with_real_events() {
        use selfie::{
            commands::ShellCommandRunner,
            fs::real::RealFileSystem,
            package::{
                repository::YamlPackageRepository,
                service::{PackageService, PackageServiceImpl},
            },
        };

        // Create a minimal config for testing
        let config = selfie::config::AppConfigBuilder::default()
            .environment("test")
            .package_directory("/tmp/nonexistent")
            .command_timeout_unchecked(1)
            .use_colors(false)
            .build();

        let repo = YamlPackageRepository::new(RealFileSystem, config.package_directory().clone());
        let command_runner = ShellCommandRunner::new("/bin/sh", config.command_timeout());
        let service = PackageServiceImpl::new(repo, command_runner, config);

        let reporter = TerminalProgressReporter::new(false);
        let processor = EventProcessor::new(reporter);

        let mut progress_events_seen = 0;
        let mut started_events_seen = 0;
        let mut completed_events_seen = 0;

        let event_stream = service.check("nonexistent-test-package").await;
        let result = processor
            .process_events(event_stream, |event| {
                match event {
                    PackageEvent::Started { .. } => {
                        started_events_seen += 1;
                        false // Use default handling
                    }
                    PackageEvent::Progress { .. } => {
                        progress_events_seen += 1;
                        false // Use default handling
                    }
                    PackageEvent::Completed { .. } => {
                        completed_events_seen += 1;
                        false // Use default handling
                    }
                    _ => false,
                }
            })
            .await;

        // Should have seen some events even though it failed
        assert_eq!(started_events_seen, 1);
        assert!(progress_events_seen > 0);
        assert_eq!(completed_events_seen, 1);
        assert_eq!(result.exit_code, 1);
        assert!(result.had_errors);
    }

    #[test]
    fn test_title_case_with_different_operations() {
        // Test the ToTitleCase trait with operation names that might come from the system
        assert_eq!("package_check".to_title_case(), "Package check");
        assert_eq!("package_install".to_title_case(), "Package install");
        assert_eq!("package_validate".to_title_case(), "Package validate");
        assert_eq!("PACKAGE_LIST".to_title_case(), "Package list");
    }
}
