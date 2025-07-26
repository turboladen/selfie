use console::style;
use selfie::{
    config::AppConfig,
    package::{event::PackageEvent, service::PackageService},
};

use crate::terminal_progress_reporter::TerminalProgressReporter;

use super::common;

pub(crate) struct ListCommand<'a> {
    config: &'a AppConfig,
    reporter: TerminalProgressReporter,
    show_all: bool,
}

impl<'a> ListCommand<'a> {
    pub(crate) fn new(
        config: &'a AppConfig,
        reporter: TerminalProgressReporter,
        show_all: bool,
    ) -> Self {
        Self {
            config,
            reporter,
            show_all,
        }
    }
}

impl ListCommand<'_> {
    pub(crate) async fn handle_command(&self) -> i32 {
        // Create the package service implementation
        let service = common::create_package_service(self.config);

        // Call the service's list method to get an event stream
        match service.list(self.show_all).await {
            Ok(event_stream) => {
                // Process the event stream with custom handling for structured data
                let processor = crate::event_processor::EventProcessor::new(self.reporter);
                let config = self.config;
                processor
                    .process_events(event_stream, move |event| {
                        handle_list_event(event, config, self.show_all)
                    })
                    .await
            }
            Err(e) => {
                self.reporter
                    .report_error(format!("Failed to list packages: {e}"));
                1
            }
        }
    }
}

fn handle_list_event(event: &PackageEvent, config: &AppConfig, show_all: bool) -> bool {
    match event {
        PackageEvent::PackageListLoaded { package_list, .. } => {
            // Show package directory path
            println!("📁 Package directory: {}", package_list.package_directory);

            if package_list.valid_packages.is_empty() && package_list.invalid_packages.is_empty() {
                println!("No packages found.");
            } else {
                display_packages_table(&package_list.valid_packages, config, show_all);

                // Display invalid packages in a separate table to stderr
                if !package_list.invalid_packages.is_empty() {
                    display_invalid_packages_table(&package_list.invalid_packages, config);
                }
            }
            true // Handled
        }
        _ => false, // Use default handling for other events
    }
}

fn display_packages_table(
    packages: &[selfie::package::event::PackageListItem],
    config: &AppConfig,
    show_all: bool,
) {
    if packages.is_empty() {
        return;
    }

    let mut table = common::create_formatted_table();

    if show_all {
        table.set_header(vec!["Name", "Version", "Environments", "Status"]);
    } else {
        table.set_header(vec!["Name", "Version", "Status"]);
    }

    for package in packages {
        let package_name = if config.use_colors() {
            style(&package.name).magenta().bold().to_string()
        } else {
            package.name.clone()
        };

        let version = if config.use_colors() {
            style(format!("v{}", package.version)).dim().to_string()
        } else {
            format!("v{}", package.version)
        };

        let status = format_status(package.status.as_ref(), config);

        if show_all {
            let environments = common::format_environment_names(
                &package.environments,
                config.environment(),
                config,
            );
            table.add_row(vec![package_name, version, environments, status]);
        } else {
            table.add_row(vec![package_name, version, status]);
        }
    }

    println!("{table}");
}

fn format_status(
    status: Option<&selfie::package::event::CheckResult>,
    config: &AppConfig,
) -> String {
    match status {
        Some(selfie::package::event::CheckResult::Success) => {
            if config.use_colors() {
                console::style("✅ Installed").green().to_string()
            } else {
                "✅ Installed".to_string()
            }
        }
        Some(selfie::package::event::CheckResult::Failed { .. }) => {
            if config.use_colors() {
                console::style("📦 Not installed").cyan().to_string()
            } else {
                "📦 Not installed".to_string()
            }
        }
        Some(selfie::package::event::CheckResult::NoCheckCommand) => {
            if config.use_colors() {
                console::style("⚠️ No check").yellow().to_string()
            } else {
                "⚠️ No check".to_string()
            }
        }
        Some(selfie::package::event::CheckResult::CommandNotFound) => {
            if config.use_colors() {
                console::style("🔍 Cmd not found").red().to_string()
            } else {
                "🔍 Cmd not found".to_string()
            }
        }
        Some(selfie::package::event::CheckResult::Error(_)) => {
            if config.use_colors() {
                console::style("💥 Error").red().to_string()
            } else {
                "💥 Error".to_string()
            }
        }
        None => {
            if config.use_colors() {
                console::style("⚪ N/A").dim().to_string()
            } else {
                "⚪ N/A".to_string()
            }
        }
    }
}

fn display_invalid_packages_table(
    invalid_packages: &[selfie::package::event::InvalidPackageInfo],
    config: &AppConfig,
) {
    if invalid_packages.is_empty() {
        return;
    }

    eprintln!();
    eprintln!("⚠️ Invalid Package Files:");

    let mut table = common::create_formatted_table();
    table.set_header(vec!["Package File", "Issue"]);

    for invalid_package in invalid_packages {
        // Extract just the filename from the full path
        let filename = std::path::Path::new(&invalid_package.path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&invalid_package.path);

        let filename_styled = if config.use_colors() {
            console::style(filename).red().to_string()
        } else {
            filename.to_string()
        };

        // Clean up the error message by removing redundant file path information
        let clean_error = clean_error_message(&invalid_package.error, &invalid_package.path);

        let error_styled = if config.use_colors() {
            console::style(clean_error).dim().to_string()
        } else {
            clean_error
        };

        table.add_row(vec![filename_styled, error_styled]);
    }

    eprintln!("{table}");
}

fn clean_error_message(error: &str, file_path: &str) -> String {
    // Remove redundant file path information from error messages
    let error = error.replace(
        &format!("YAML parsing error reading package file `{file_path}`:"),
        "",
    );
    let error = error.trim();

    // Clean up common patterns
    if error.starts_with("missing field") {
        error.to_string()
    } else if error.contains("missing field") {
        // For environment-specific errors, simplify the format
        if let Some(env_part) = error.split(':').next() {
            if env_part.contains("environments.") {
                format!("{}: missing field", env_part.trim())
            } else {
                error.to_string()
            }
        } else {
            error.to_string()
        }
    } else {
        error.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use selfie::package::event::PackageListItem;
    use test_common::{ALT_TEST_ENV, TEST_ENV, TEST_VERSION, test_config, test_config_with_colors};

    fn create_mock_reporter() -> TerminalProgressReporter {
        TerminalProgressReporter::new(false)
    }

    #[test]
    fn test_list_command_new() {
        let config = test_config();
        let reporter = create_mock_reporter();

        let command = ListCommand::new(&config, reporter, false);
        // Just test that construction doesn't panic
        assert_eq!(command.config.environment(), "test-env");
        assert!(!command.show_all);
    }

    #[test]
    fn test_display_packages_table_empty() {
        let config = test_config();
        let packages = vec![];

        // Should not panic with empty list
        display_packages_table(&packages, &config, false);
        display_packages_table(&packages, &config, true);
    }

    #[test]
    fn test_display_packages_table_single_package() {
        let config = test_config();
        let packages = vec![PackageListItem {
            name: "test-package".to_string(),
            version: TEST_VERSION.to_string(),
            environments: vec![TEST_ENV.to_string()],
            status: Some(selfie::package::event::CheckResult::Success),
        }];

        // Should not panic
        display_packages_table(&packages, &config, false);
        display_packages_table(&packages, &config, true);
    }

    #[test]
    fn test_display_packages_table_with_colors() {
        let config = test_config_with_colors();
        let packages = vec![PackageListItem {
            name: "test-package".to_string(),
            version: TEST_VERSION.to_string(),
            environments: vec![TEST_ENV.to_string()],
            status: Some(selfie::package::event::CheckResult::Success),
        }];

        // Should not panic with colors enabled
        display_packages_table(&packages, &config, false);
        display_packages_table(&packages, &config, true);
    }

    #[test]
    fn test_create_table() {
        let table = common::create_formatted_table();
        // Just test that table creation doesn't panic
        let _table_str = table.to_string();
    }

    #[test]
    fn test_format_environments() {
        let config = test_config();
        let environments = vec![TEST_ENV.to_string(), ALT_TEST_ENV.to_string()];

        let result = common::format_environment_names(&environments, TEST_ENV, &config);

        // Just test that it doesn't panic and returns something
        assert!(!result.is_empty());
    }

    #[test]
    fn test_display_invalid_packages_table_empty() {
        let config = test_config();
        let invalid_packages = vec![];

        // Should not panic with empty list
        display_invalid_packages_table(&invalid_packages, &config);
    }

    #[test]
    fn test_display_invalid_packages_table_with_items() {
        let config = test_config();
        let invalid_packages = vec![selfie::package::event::InvalidPackageInfo {
            path: "/path/to/test-package.yml".to_string(),
            error: "missing field `name`".to_string(),
        }];

        // Should not panic
        display_invalid_packages_table(&invalid_packages, &config);
    }

    #[test]
    fn test_clean_error_message() {
        let file_path = "/path/to/package.yml";

        // Test removing redundant file path
        let error1 =
            "YAML parsing error reading package file `/path/to/package.yml`: missing field `name`";
        let cleaned1 = clean_error_message(error1, file_path);
        assert_eq!(cleaned1, "missing field `name`");

        // Test environment-specific error
        let error2 = "environments.macos-work: missing field `install` at line 15 column 5";
        let cleaned2 = clean_error_message(error2, file_path);
        assert_eq!(cleaned2, "environments.macos-work: missing field");

        // Test simple missing field error
        let error3 = "missing field `name`";
        let cleaned3 = clean_error_message(error3, file_path);
        assert_eq!(cleaned3, "missing field `name`");
    }

    #[test]
    fn test_format_status_n_a() {
        let config = test_config();
        let config_with_colors = test_config_with_colors();

        // Test N/A status without colors
        let result = format_status(None, &config);
        assert_eq!(result, "⚪ N/A");

        // Test N/A status with colors
        let result_colored = format_status(None, &config_with_colors);
        assert!(result_colored.contains("⚪ N/A"));
    }

    #[test]
    fn test_format_status_not_installed() {
        let config = test_config();
        let config_with_colors = test_config_with_colors();

        // Test "Not installed" status without colors
        let result = format_status(
            Some(&selfie::package::event::CheckResult::Failed {
                stdout: String::new(),
                stderr: "".to_string(),
                exit_code: Some(1),
            }),
            &config,
        );
        assert_eq!(result, "📦 Not installed");

        // Test "Not installed" status with colors (should contain the emoji and text)
        let result_colored = format_status(
            Some(&selfie::package::event::CheckResult::Failed {
                stdout: String::new(),
                stderr: "".to_string(),
                exit_code: Some(1),
            }),
            &config_with_colors,
        );
        assert!(result_colored.contains("📦 Not installed"));
    }

    #[test]
    fn test_format_status_command_not_found() {
        let config = test_config();
        let config_with_colors = test_config_with_colors();

        // Test "Cmd not found" status without colors
        let result = format_status(
            Some(&selfie::package::event::CheckResult::CommandNotFound),
            &config,
        );
        assert_eq!(result, "🔍 Cmd not found");

        // Test "Cmd not found" status with colors
        let result_colored = format_status(
            Some(&selfie::package::event::CheckResult::CommandNotFound),
            &config_with_colors,
        );
        assert!(result_colored.contains("🔍 Cmd not found"));
    }

    #[test]
    fn test_format_status_error() {
        let config = test_config();
        let config_with_colors = test_config_with_colors();

        // Test "Error" status without colors
        let result = format_status(
            Some(&selfie::package::event::CheckResult::Error(
                "test error".to_string(),
            )),
            &config,
        );
        assert_eq!(result, "💥 Error");

        // Test "Error" status with colors
        let result_colored = format_status(
            Some(&selfie::package::event::CheckResult::Error(
                "test error".to_string(),
            )),
            &config_with_colors,
        );
        assert!(result_colored.contains("💥 Error"));
    }
}
