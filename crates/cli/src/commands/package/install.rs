use selfie::package::{
    event::{OperationFailure, OperationResult, PackageEvent},
    port::PackageError,
    service::PackageService,
};
use std::collections::VecDeque;
use tracing::info;

use crate::{
    commands::package::common::{self, report_status},
    config::CliConfig,
    event_processor::EventProcessor,
    terminal_progress_reporter::TerminalProgressReporter,
};

/// Manages a scrolling window of installation output without progress bars
struct InstallationDisplay {
    reporter: TerminalProgressReporter,
    output_lines: VecDeque<String>,
    max_lines: usize,
    first_output: bool,
}

impl InstallationDisplay {
    fn new(reporter: TerminalProgressReporter) -> Self {
        Self {
            reporter,
            output_lines: VecDeque::new(),
            max_lines: 5,
            first_output: true,
        }
    }

    fn add_output_line(&mut self, line: &str, is_stderr: bool) {
        let formatted_line = if is_stderr {
            self.reporter.format_stderr_output(line)
        } else {
            self.reporter.format_stdout_output(line)
        };

        // Print header only on first output
        if self.first_output {
            eprintln!("{}", self.reporter.format_output_header());
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
    config: &'a CliConfig,
    display: &'a mut InstallationDisplay,
}

impl<'a> InstallEventHandler<'a> {
    fn new(config: &'a CliConfig, display: &'a mut InstallationDisplay) -> Self {
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
            _ => false, // Use default handling for other events
        }
    }

    fn handle_check_result_completed(
        check_result: &selfie::package::event::CheckResultData,
    ) -> bool {
        match &check_result.result {
            selfie::package::event::CheckResult::Success { .. } => {
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
    service: &impl PackageService,
    package_name: &str,
    config: &CliConfig,
    reporter: TerminalProgressReporter,
) -> i32 {
    info!("Installing package: {}", package_name);

    // Call the service's install method to get an event stream
    let event_stream = service.install(package_name).await;

    // Create installation display
    let mut display = InstallationDisplay::new(reporter);

    // Track whether we handled an environment error in the Completed arm
    let mut env_error_handled = false;

    report_status(&format!("Installing {package_name}..."));

    // Process the event stream with custom handling for install-specific events
    let processor = EventProcessor::new(reporter);
    let mut event_handler = InstallEventHandler::new(config, &mut display);
    let result = processor
        .process_events(event_stream, |event| {
            // Check for environment errors in Completed events
            if let PackageEvent::Completed {
                result: OperationResult::Failure(failure),
                ..
            } = event
                && failure.is_environment_error()
            {
                display_environment_error(failure, config);
                env_error_handled = true;
                return true; // Handled
            }

            event_handler.handle_event(event)
        })
        .await;

    if env_error_handled {
        1
    } else {
        result.exit_code
    }
}

/// Display environment error with helpful suggestions from the typed failure data
fn display_environment_error(failure: &OperationFailure, config: &CliConfig) {
    println!();

    if let OperationFailure::Package(PackageError::EnvironmentNotFound {
        package_name,
        available_environments,
        ..
    }) = failure
    {
        common::display_environment_summary(
            package_name,
            config.environment(),
            available_environments,
            config,
            "install",
        );
    } else {
        common::display_generic_environment_suggestion(
            "package",
            config.environment(),
            config,
            "install",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CliSection;
    use test_common::test_config;

    fn create_mock_reporter() -> TerminalProgressReporter {
        TerminalProgressReporter::new(false)
    }

    fn cli_config_verbose() -> CliConfig {
        CliConfig::new(
            test_config(),
            CliSection {
                verbose: true,
                use_colors: false,
            },
        )
    }

    fn cli_config_default() -> CliConfig {
        CliConfig::new(test_config(), CliSection::default())
    }

    #[tokio::test]
    async fn test_handle_install_basic() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config = CliConfig::wrap_for_test(test_common::test_config_with_dir(temp_dir.path()));
        let service = test_common::create_test_service(&temp_dir);
        let reporter = create_mock_reporter();

        // This will fail without proper setup, but tests that the function can be called
        let _result = handle_install(&service, "test-package", &config, reporter).await;
    }

    #[test]
    fn test_installation_display() {
        let reporter = create_mock_reporter();
        let mut display = InstallationDisplay::new(reporter);

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
        let config = cli_config_verbose();
        let reporter = create_mock_reporter();
        let mut display = InstallationDisplay::new(reporter);
        let mut handler = InstallEventHandler::new(&config, &mut display);

        // Test progress handling in verbose mode
        let handled = handler.handle_progress_event("Installing package");
        assert!(!handled); // Should use default handling in verbose mode
    }

    #[test]
    fn test_install_event_handler_progress_non_verbose() {
        let config = cli_config_default();
        let reporter = create_mock_reporter();
        let mut display = InstallationDisplay::new(reporter);
        let mut handler = InstallEventHandler::new(&config, &mut display);

        // Test progress handling in non-verbose mode
        let handled = handler.handle_progress_event("Installing package");
        assert!(handled); // Should be handled in non-verbose mode
    }

    #[test]
    fn test_install_event_handler_info_verbose() {
        let config = cli_config_verbose();
        let reporter = create_mock_reporter();
        let mut display = InstallationDisplay::new(reporter);
        let mut handler = InstallEventHandler::new(&config, &mut display);

        // Test info handling in verbose mode
        let output = selfie::package::event::ConsoleOutput::Stdout("test output".to_string());
        let handled = handler.handle_info_event(&output);
        assert!(!handled); // Should use default handling in verbose mode
    }

    #[test]
    fn test_install_event_handler_info_non_verbose() {
        let config = cli_config_default();
        let reporter = create_mock_reporter();
        let mut display = InstallationDisplay::new(reporter);
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
            result: selfie::package::event::CheckResult::Success {
                stdout: String::new(),
                stderr: String::new(),
            },
        };

        let handled = InstallEventHandler::handle_check_result_completed(&check_result);
        assert!(handled); // Check results should always be handled
    }
}
