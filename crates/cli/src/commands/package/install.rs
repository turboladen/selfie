use selfie::{
    config::AppConfig,
    package::{
        event::{OperationResult, PackageEvent, error::StreamedError},
        port::{PackageError, PackageRepoError},
        service::PackageService,
    },
};
use std::collections::VecDeque;
use tracing::info;

use crate::{
    commands::package::common::{self, create_package_service, report_status},
    event_processor::EventProcessor,
    terminal_progress_reporter::TerminalProgressReporter,
};

/// Manages a scrolling window of installation output without progress bars
struct InstallationDisplay {
    output_lines: VecDeque<String>,
    max_lines: usize,
    first_output: bool,
}

impl InstallationDisplay {
    fn new() -> Self {
        Self {
            output_lines: VecDeque::new(),
            max_lines: 5,
            first_output: true,
        }
    }

    fn add_output_line(&mut self, line: &str, is_stderr: bool) {
        let formatted_line = if is_stderr {
            format!("    🔧 {}", line.trim())
        } else {
            format!("    📦 {}", line.trim())
        };

        // Print header only on first output
        if self.first_output {
            eprintln!(
                "
📦 Installation output:"
            );
            self.first_output = false;
        }

        // Print the new line immediately
        eprintln!("{formatted_line}");

        self.output_lines.push_back(formatted_line);

        // Keep only the last max_lines
        while self.output_lines.len() > self.max_lines {
            self.output_lines.pop_front();
        }
    }

    fn set_status(status: &str) {
        report_status(status);
    }
}

/// Handles events specifically for the install command
struct InstallEventHandler<'a> {
    config: &'a AppConfig,
    display: &'a mut InstallationDisplay,
}

impl<'a> InstallEventHandler<'a> {
    fn new(config: &'a AppConfig, display: &'a mut InstallationDisplay) -> Self {
        Self { config, display }
    }

    fn handle_event(&mut self, event: &PackageEvent) -> bool {
        #[allow(clippy::match_same_arms)]
        match event {
            PackageEvent::CheckResultCompleted { check_result, .. } => {
                Self::handle_check_result_completed(check_result)
            }
            PackageEvent::Info { output, .. } => self.handle_info_event(output),
            PackageEvent::Progress { message, .. } => self.handle_progress_event(message),
            PackageEvent::Completed { result, .. } => {
                // Skip duplicate error display for environment configuration errors
                match result {
                    OperationResult::Failure(failure) => {
                        match failure {
                            selfie::package::event::OperationFailure::EnvironmentError(_) => {
                                true // Handled - we already showed the error message above
                            }
                            _ => false, // Use default failure handling for other types of failures
                        }
                    }
                    OperationResult::Success(_) => false,
                }
            }
            _ => false, // Use default handling for other events
        }
    }

    fn handle_check_result_completed(
        check_result: &selfie::package::event::CheckResultData,
    ) -> bool {
        match &check_result.result {
            selfie::package::event::CheckResult::Success => {
                InstallationDisplay::set_status("Package is already installed");
            }
            selfie::package::event::CheckResult::Failed { .. } => {
                InstallationDisplay::set_status(
                    "Package not currently installed, proceeding with installation",
                );
            }
            selfie::package::event::CheckResult::NoCheckCommand => {
                InstallationDisplay::set_status(
                    "No check command defined, proceeding with installation",
                );
            }
            selfie::package::event::CheckResult::CommandNotFound => {
                InstallationDisplay::set_status(
                    "Check command not found, proceeding with installation",
                );
            }
            selfie::package::event::CheckResult::Error(err) => {
                InstallationDisplay::set_status(&format!(
                    "Check error ({err}), proceeding with installation"
                ));
            }
        }
        true // Handled
    }

    fn handle_info_event(&mut self, output: &selfie::package::event::ConsoleOutput) -> bool {
        if self.config.verbose() {
            // In verbose mode, show all output immediately
            false // Use default handler (prints to stdout/stderr)
        } else {
            // In non-verbose mode, show in scrolling window
            match output {
                selfie::package::event::ConsoleOutput::Stdout(line) => {
                    for line in line.lines() {
                        if !line.trim().is_empty() {
                            self.display.add_output_line(line, false);
                        }
                    }
                }
                selfie::package::event::ConsoleOutput::Stderr(line) => {
                    for line in line.lines() {
                        if !line.trim().is_empty() {
                            self.display.add_output_line(line, true);
                        }
                    }
                }
            }
            true // Handled
        }
    }

    fn handle_progress_event(&mut self, message: &str) -> bool {
        if self.config.verbose() {
            false // Use default progress handling
        } else {
            InstallationDisplay::set_status(message);
            true // Handled
        }
    }
}

pub(crate) async fn handle_install(
    package_name: &str,
    config: &AppConfig,
    reporter: TerminalProgressReporter,
) -> i32 {
    info!("Installing package: {}", package_name);

    // Create the package service
    let service = create_package_service(config);

    // Call the service's install method to get an event stream
    let event_stream = service.install(package_name).await;

    // Create installation display
    let mut display = InstallationDisplay::new();

    report_status(&format!("Installing {package_name}..."));

    // Process the event stream with custom handling for install-specific events
    let processor = EventProcessor::new(reporter);
    let mut event_handler = InstallEventHandler::new(config, &mut display);
    let result = processor
        .process_events(event_stream, |event| {
            // Check for environment errors first
            if let PackageEvent::Error { error, .. } = event {
                if let StreamedError::PackageRepoError(PackageRepoError::PackageError(pkg_error)) =
                    error
                {
                    if matches!(
                        pkg_error.as_ref(),
                        selfie::package::port::PackageError::EnvironmentNotFound { .. }
                    ) {
                        handle_environment_not_found_error(error, config);
                        return true; // Handled completely - prevent duplicate error display
                    }
                }
            }

            event_handler.handle_event(event)
        })
        .await;

    // Return proper exit code - 1 for environment errors, otherwise use result from processor
    if result.environment_error_handled {
        1
    } else {
        result.exit_code
    }
}

/// Handle environment not found errors with helpful suggestions
fn handle_environment_not_found_error(error: &StreamedError, config: &AppConfig) {
    // Show helpful information about available environments
    println!();

    // Try to extract environment information from the structured error
    if let StreamedError::PackageRepoError(PackageRepoError::PackageError(package_error)) = error {
        if let PackageError::EnvironmentNotFound {
            package_name,
            available_environments,
            ..
        } = package_error.as_ref()
        {
            common::display_environment_summary(
                package_name,
                config.environment(),
                available_environments,
                config,
                "install",
            );
            return;
        }
    }

    // Fallback to generic suggestion if we can't extract environment info
    common::display_generic_environment_suggestion(
        "package",
        config.environment(),
        config,
        "install",
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_common::{test_config, test_config_verbose};

    fn create_mock_reporter() -> TerminalProgressReporter {
        TerminalProgressReporter::new(false)
    }

    #[tokio::test]
    async fn test_handle_install_basic() {
        let config = test_config();
        let reporter = create_mock_reporter();

        // This will fail without proper setup, but tests that the function can be called
        let _result = handle_install("test-package", &config, reporter).await;
    }

    #[test]
    fn test_installation_display() {
        let mut display = InstallationDisplay::new();

        // Test adding output lines
        display.add_output_line("test output", false);
        assert_eq!(display.output_lines.len(), 1);

        // Test max lines behavior
        for i in 0..10 {
            display.add_output_line(&format!("line {i}"), false);
        }
        assert_eq!(display.output_lines.len(), display.max_lines);
    }

    #[test]
    fn test_install_event_handler_progress_verbose() {
        let config = test_config_verbose();
        let mut display = InstallationDisplay::new();
        let mut handler = InstallEventHandler::new(&config, &mut display);

        // Test progress handling in verbose mode
        let handled = handler.handle_progress_event("Installing package");
        assert!(!handled); // Should use default handling in verbose mode
    }

    #[test]
    fn test_install_event_handler_progress_non_verbose() {
        let config = test_config();
        let mut display = InstallationDisplay::new();
        let mut handler = InstallEventHandler::new(&config, &mut display);

        // Test progress handling in non-verbose mode
        let handled = handler.handle_progress_event("Installing package");
        assert!(handled); // Should be handled in non-verbose mode
    }

    #[test]
    fn test_install_event_handler_info_verbose() {
        let config = test_config_verbose();
        let mut display = InstallationDisplay::new();
        let mut handler = InstallEventHandler::new(&config, &mut display);

        // Test info handling in verbose mode
        let output = selfie::package::event::ConsoleOutput::Stdout("test output".to_string());
        let handled = handler.handle_info_event(&output);
        assert!(!handled); // Should use default handling in verbose mode
    }

    #[test]
    fn test_install_event_handler_info_non_verbose() {
        let config = test_config();
        let mut display = InstallationDisplay::new();
        let mut handler = InstallEventHandler::new(&config, &mut display);

        // Test info handling in non-verbose mode
        let output = selfie::package::event::ConsoleOutput::Stdout("test output".to_string());
        let handled = handler.handle_info_event(&output);
        assert!(handled); // Should be handled in non-verbose mode
    }

    #[test]
    fn test_install_event_handler_check_result() {
        // Test check result handling
        let check_result = selfie::package::event::CheckResultData {
            package_name: "test-package".to_string(),
            environment: "test".to_string(),
            check_command: Some("which test-package".to_string()),
            result: selfie::package::event::CheckResult::Success,
        };

        let handled = InstallEventHandler::handle_check_result_completed(&check_result);
        assert!(handled); // Check results should always be handled
    }
}
