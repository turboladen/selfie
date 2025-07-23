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
}

impl<'a> ListCommand<'a> {
    pub(crate) fn new(config: &'a AppConfig, reporter: TerminalProgressReporter) -> Self {
        Self { config, reporter }
    }
}

impl ListCommand<'_> {
    pub(crate) async fn handle_command(&self) -> i32 {
        // Create the package service implementation
        let service = common::create_package_service(self.config);

        // Call the service's list method to get an event stream
        match service.list().await {
            Ok(event_stream) => {
                // Process the event stream with custom handling for structured data
                let processor = crate::event_processor::EventProcessor::new(self.reporter);
                let config = self.config;
                processor
                    .process_events(event_stream, move |event| handle_list_event(event, config))
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

fn handle_list_event(event: &PackageEvent, config: &AppConfig) -> bool {
    match event {
        PackageEvent::PackageListLoaded { package_list, .. } => {
            // Show package directory path
            println!("📁 Package directory: {}", package_list.package_directory);

            if package_list.valid_packages.is_empty() && package_list.invalid_packages.is_empty() {
                println!("No packages found.");
            } else {
                display_packages_table(&package_list.valid_packages, config);

                // Report invalid packages as separate messages after the table
                for invalid_package in &package_list.invalid_packages {
                    eprintln!(
                        "⚠️ Invalid package at {}: {}",
                        invalid_package.path, invalid_package.error
                    );
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
) {
    if packages.is_empty() {
        return;
    }

    let mut table = common::create_formatted_table();
    table.set_header(vec!["Name", "Version", "Environments"]);

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

        let environments =
            common::format_environment_names(&package.environments, config.environment(), config);

        table.add_row(vec![package_name, version, environments]);
    }

    println!("{table}");
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

        let command = ListCommand::new(&config, reporter);
        // Just test that construction doesn't panic
        assert_eq!(command.config.environment(), "test-env");
    }

    #[test]
    fn test_display_packages_table_empty() {
        let config = test_config();
        let packages = vec![];

        // Should not panic with empty list
        display_packages_table(&packages, &config);
    }

    #[test]
    fn test_display_packages_table_single_package() {
        let config = test_config();
        let packages = vec![PackageListItem {
            name: "test-package".to_string(),
            version: TEST_VERSION.to_string(),
            environments: vec![TEST_ENV.to_string()],
        }];

        // Should not panic
        display_packages_table(&packages, &config);
    }

    #[test]
    fn test_display_packages_table_with_colors() {
        let config = test_config_with_colors();
        let packages = vec![PackageListItem {
            name: "test-package".to_string(),
            version: TEST_VERSION.to_string(),
            environments: vec![TEST_ENV.to_string()],
        }];

        // Should not panic with colors enabled
        display_packages_table(&packages, &config);
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
}
