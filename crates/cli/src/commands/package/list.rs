use std::collections::HashMap;

use console::style;
use selfie::package::{event, service::PackageService};

use crate::{
    config::CliConfig,
    display_manager::{DisplayManager, OperationHandle},
};

use super::common;

pub(crate) struct ListCommand<'a> {
    config: &'a CliConfig,
    display: DisplayManager,
    show_all: bool,
}

impl<'a> ListCommand<'a> {
    pub(crate) fn new(config: &'a CliConfig, display: DisplayManager, show_all: bool) -> Self {
        Self {
            config,
            display,
            show_all,
        }
    }
}

impl ListCommand<'_> {
    pub(crate) async fn handle_command(&self, service: &impl PackageService) -> i32 {
        let event_stream = service.list(self.show_all).await;

        let processor = crate::event_processor::EventProcessor::new(self.display.clone());
        let config = self.config;
        let display = &self.display;
        let show_all = self.show_all;
        let use_colors = config.use_colors();
        let is_tty = display.is_tty();

        // Track spinners by package name for in-place resolution
        let mut spinners: HashMap<String, OperationHandle> = HashMap::new();
        let mut max_name_len: usize = 0;

        let result = processor
            .process_events(event_stream, |event| {
                handle_list_event(
                    event,
                    config,
                    display,
                    use_colors,
                    show_all,
                    is_tty,
                    &mut spinners,
                    &mut max_name_len,
                )
            })
            .await;
        result.exit_code
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_list_event(
    event: &event::PackageEvent,
    config: &CliConfig,
    display: &DisplayManager,
    use_colors: bool,
    show_all: bool,
    is_tty: bool,
    spinners: &mut HashMap<String, OperationHandle>,
    max_name_len: &mut usize,
) -> bool {
    match event {
        event::PackageEvent::PackageListReady { packages, .. } => {
            // Compute column width from longest package name
            *max_name_len = packages.iter().map(|p| p.name.len()).max().unwrap_or(0);

            // Create one spinner per package in sorted order
            for pkg in packages {
                let version = format!("v{}", pkg.version);
                let msg = if show_all {
                    let envs = common::format_environment_names(
                        &pkg.environments,
                        config.environment(),
                        config,
                    );
                    format!(
                        "{:<width$}  {:<10}  checking...  ({envs})",
                        pkg.name,
                        version,
                        width = *max_name_len
                    )
                } else {
                    format!(
                        "{:<width$}  {:<10}  checking...",
                        pkg.name,
                        version,
                        width = *max_name_len
                    )
                };

                if is_tty {
                    let handle = display.start_list_spinner(&msg);
                    spinners.insert(pkg.name.clone(), handle);
                } else {
                    // Non-TTY: just print a placeholder (will be overwritten conceptually)
                    // We'll print the final result in PackageListItemCompleted instead
                }
            }
            true // Handled
        }

        event::PackageEvent::PackageListItemCompleted { package_item, .. } => {
            let status_text = format_check_status(package_item.status.as_ref(), use_colors);
            let version = format!("v{}", package_item.version);

            let line = if show_all {
                let envs = common::format_environment_names(
                    &package_item.environments,
                    config.environment(),
                    config,
                );
                format!(
                    "{:<width$}  {:<10}  {status_text}  ({envs})",
                    package_item.name,
                    version,
                    width = *max_name_len
                )
            } else {
                format!(
                    "{:<width$}  {:<10}  {status_text}",
                    package_item.name,
                    version,
                    width = *max_name_len
                )
            };

            if is_tty {
                if let Some(handle) = spinners.remove(&package_item.name) {
                    match &package_item.status {
                        Some(event::CheckResult::Success { .. }) => {
                            handle.finish_success_in_place(&line);
                        }
                        Some(event::CheckResult::Failed { .. })
                        | Some(event::CheckResult::CommandNotFound) => {
                            handle.finish_failure_in_place(&line);
                        }
                        Some(event::CheckResult::NoCheckCommand) => {
                            handle.finish_warning_in_place(&line);
                        }
                        Some(event::CheckResult::Error(_)) => {
                            handle.finish_failure_in_place(&line);
                        }
                        None => {
                            handle.finish_warning_in_place(&line);
                        }
                    }
                }
            } else {
                // Non-TTY: print result line directly
                let prefix = match &package_item.status {
                    Some(event::CheckResult::Success { .. }) => "\u{2713}",
                    Some(event::CheckResult::Failed { .. })
                    | Some(event::CheckResult::CommandNotFound)
                    | Some(event::CheckResult::Error(_)) => "\u{2717}",
                    Some(event::CheckResult::NoCheckCommand) | None => "\u{26a0}",
                };
                println!("{prefix} {line}");
            }
            true // Handled
        }

        event::PackageEvent::PackageListLoaded { package_list, .. } => {
            // Print summary
            println!();
            println!(
                "\u{1f4c1} Package directory: {}",
                package_list.package_directory
            );

            if package_list.valid_packages.is_empty() && package_list.environment_stats.is_empty() {
                println!("No packages found.");
            } else if package_list.valid_packages.is_empty() {
                println!(
                    "No packages found for environment '{}'.",
                    config.environment()
                );
                display_environment_stats(&package_list.environment_stats, config);
            } else {
                let valid = package_list.valid_packages.len();
                let invalid = package_list.invalid_packages.len();
                if invalid > 0 {
                    println!("{valid} valid, {invalid} invalid");
                } else {
                    println!("{valid} packages");
                }
            }

            // Display invalid packages if any
            if !package_list.invalid_packages.is_empty() {
                display_invalid_packages(&package_list.invalid_packages, config);
            }
            true // Handled
        }

        event::PackageEvent::Progress { .. } => {
            true // Suppress — spinners are the progress indicator
        }

        _ => false, // Use default handling for other events
    }
}

fn format_check_status(status: Option<&event::CheckResult>, use_colors: bool) -> String {
    match status {
        Some(event::CheckResult::Success { .. }) => {
            if use_colors {
                style("Installed").green().to_string()
            } else {
                "Installed".to_string()
            }
        }
        Some(event::CheckResult::Failed { .. }) => {
            if use_colors {
                style("Not installed").cyan().to_string()
            } else {
                "Not installed".to_string()
            }
        }
        Some(event::CheckResult::NoCheckCommand) => {
            if use_colors {
                style("No check").yellow().to_string()
            } else {
                "No check".to_string()
            }
        }
        Some(event::CheckResult::CommandNotFound) => {
            if use_colors {
                style("Cmd not found").red().to_string()
            } else {
                "Cmd not found".to_string()
            }
        }
        Some(event::CheckResult::Error(e)) => {
            if use_colors {
                style(format!("Error: {e}")).red().to_string()
            } else {
                format!("Error: {e}")
            }
        }
        None => {
            if use_colors {
                style("N/A").dim().to_string()
            } else {
                "N/A".to_string()
            }
        }
    }
}

fn display_invalid_packages(
    invalid_packages: &[selfie::package::event::InvalidPackageInfo],
    config: &CliConfig,
) {
    if invalid_packages.is_empty() {
        return;
    }

    eprintln!();
    eprintln!("\u{26a0} Invalid package files:");

    for invalid_package in invalid_packages {
        let filename = std::path::Path::new(&invalid_package.path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&invalid_package.path);

        let clean_error = clean_error_message(&invalid_package.error, &invalid_package.path);

        if config.use_colors() {
            eprintln!(
                "  {} {}: {}",
                style("\u{2717}").red(),
                style(filename).red(),
                style(clean_error).dim()
            );
        } else {
            eprintln!("  \u{2717} {filename}: {clean_error}");
        }
    }
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

fn display_environment_stats(
    environment_stats: &std::collections::HashMap<String, usize>,
    config: &CliConfig,
) {
    if environment_stats.is_empty() {
        return;
    }

    println!();
    println!("\u{1f4ca} Packages by environment in this directory:");

    // Sort environments by package count (descending), then by name
    let mut env_counts: Vec<(String, usize)> = environment_stats
        .iter()
        .map(|(env, count)| (env.clone(), *count))
        .collect();
    env_counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    let mut table = common::create_formatted_table();
    table.set_header(vec!["Environment", "Package Count"]);

    for (env_name, count) in env_counts {
        let env_styled = if config.use_colors() {
            if env_name == config.environment() {
                console::style(&env_name).green().bold().to_string()
            } else {
                env_name.clone()
            }
        } else {
            env_name.clone()
        };

        let count_str = count.to_string();
        let count_styled = if config.use_colors() {
            console::style(count_str).cyan().to_string()
        } else {
            count_str
        };

        table.add_row(vec![env_styled, count_styled]);
    }

    println!("{table}");

    if config.use_colors() {
        println!(
            "\u{1f4a1} Try: {} to see packages for a different environment",
            console::style("--environment <env>").yellow()
        );
        println!(
            "   or: {} to see all packages regardless of environment",
            console::style("--all").yellow()
        );
    } else {
        println!("\u{1f4a1} Try: --environment <env> to see packages for a different environment");
        println!("   or: --all to see all packages regardless of environment");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use selfie::package::event::PackageListItem;
    use test_common::{ALT_TEST_ENV, TEST_ENV, TEST_VERSION};

    /// Helper to call handle_list_event with test defaults (non-TTY, no spinners)
    fn test_handle_event(event: &event::PackageEvent, config: &CliConfig, show_all: bool) -> bool {
        let display = DisplayManager::new(false);
        let use_colors = config.use_colors();
        let mut spinners = HashMap::new();
        let mut max_name_len = 0;
        handle_list_event(
            event,
            config,
            &display,
            use_colors,
            show_all,
            false,
            &mut spinners,
            &mut max_name_len,
        )
    }

    #[test]
    fn test_list_command_new() {
        let config = CliConfig::wrap_for_test(test_common::test_config());
        let display = DisplayManager::new(false);

        let command = ListCommand::new(&config, display, false);
        // Just test that construction doesn't panic
        assert_eq!(command.config.environment(), "test-env");
        assert!(!command.show_all);
    }

    #[test]
    fn test_create_table() {
        let table = common::create_formatted_table();
        // Just test that table creation doesn't panic
        let _table_str = table.to_string();
    }

    #[test]
    fn test_format_environments() {
        let config = CliConfig::wrap_for_test(test_common::test_config());
        let environments = vec![TEST_ENV.to_string(), ALT_TEST_ENV.to_string()];

        let result = common::format_environment_names(&environments, TEST_ENV, &config);

        // Just test that it doesn't panic and returns something
        assert!(!result.is_empty());
    }

    #[test]
    fn test_display_invalid_packages_empty() {
        let config = CliConfig::wrap_for_test(test_common::test_config());
        let invalid_packages = vec![];

        // Should not panic with empty list
        display_invalid_packages(&invalid_packages, &config);
    }

    #[test]
    fn test_display_invalid_packages_with_items() {
        let config = CliConfig::wrap_for_test(test_common::test_config());
        let invalid_packages = vec![selfie::package::event::InvalidPackageInfo {
            path: "/path/to/test-package.yml".to_string(),
            error: "missing field `name`".to_string(),
        }];

        // Should not panic
        display_invalid_packages(&invalid_packages, &config);
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
    fn test_format_check_status_installed() {
        let result = format_check_status(
            Some(&event::CheckResult::Success {
                stdout: String::new(),
                stderr: String::new(),
            }),
            false,
        );
        assert_eq!(result, "Installed");
    }

    #[test]
    fn test_format_check_status_not_installed() {
        let result = format_check_status(
            Some(&event::CheckResult::Failed {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(1),
            }),
            false,
        );
        assert_eq!(result, "Not installed");
    }

    #[test]
    fn test_format_check_status_no_check() {
        let result = format_check_status(Some(&event::CheckResult::NoCheckCommand), false);
        assert_eq!(result, "No check");
    }

    #[test]
    fn test_format_check_status_cmd_not_found() {
        let result = format_check_status(Some(&event::CheckResult::CommandNotFound), false);
        assert_eq!(result, "Cmd not found");
    }

    #[test]
    fn test_format_check_status_error() {
        let result = format_check_status(
            Some(&event::CheckResult::Error("test error".to_string())),
            false,
        );
        assert_eq!(result, "Error: test error");
    }

    #[test]
    fn test_format_check_status_na() {
        let result = format_check_status(None, false);
        assert_eq!(result, "N/A");
    }

    #[test]
    fn test_format_check_status_with_colors() {
        // Just verify these don't panic and return non-empty strings
        let result = format_check_status(
            Some(&event::CheckResult::Success {
                stdout: String::new(),
                stderr: String::new(),
            }),
            true,
        );
        assert!(result.contains("Installed"));

        let result = format_check_status(None, true);
        assert!(result.contains("N/A"));
    }

    #[test]
    fn test_display_environment_stats_empty() {
        let config = CliConfig::wrap_for_test(test_common::test_config());
        let environment_stats = std::collections::HashMap::new();

        // Should not panic with empty stats
        display_environment_stats(&environment_stats, &config);
    }

    #[test]
    fn test_display_environment_stats_single_environment() {
        let config = CliConfig::wrap_for_test(test_common::test_config());
        let mut environment_stats = std::collections::HashMap::new();
        environment_stats.insert("macos".to_string(), 3);

        // Should not panic with single environment
        display_environment_stats(&environment_stats, &config);
    }

    #[test]
    fn test_display_environment_stats_multiple_environments() {
        let config = CliConfig::wrap_for_test(test_common::test_config());
        let mut environment_stats = std::collections::HashMap::new();
        environment_stats.insert("macos".to_string(), 3);
        environment_stats.insert("ubuntu".to_string(), 2);
        environment_stats.insert("windows".to_string(), 1);

        // Should not panic with multiple environments
        display_environment_stats(&environment_stats, &config);
    }

    #[test]
    fn test_display_environment_stats_with_colors() {
        let config = CliConfig::wrap_for_test_with_colors(test_common::test_config());
        let mut environment_stats = std::collections::HashMap::new();
        environment_stats.insert(TEST_ENV.to_string(), 2);
        environment_stats.insert("other-env".to_string(), 1);

        // Should not panic with colors enabled
        display_environment_stats(&environment_stats, &config);
    }

    #[test]
    fn test_handle_list_event_empty_packages_and_stats() {
        let config = CliConfig::wrap_for_test(test_common::test_config());
        let package_list = selfie::package::event::PackageListData {
            valid_packages: vec![],
            invalid_packages: vec![],
            current_environment: TEST_ENV.to_string(),
            package_directory: "/test/path".to_string(),
            environment_stats: std::collections::HashMap::new(),
        };

        let event = event::PackageEvent::PackageListLoaded {
            operation_info: test_common::create_test_operation_info("package_list", "", TEST_ENV),
            package_list,
        };

        // Should handle empty packages and empty stats (shows "No packages found.")
        let result = test_handle_event(&event, &config, false);
        assert!(result);
    }

    #[test]
    fn test_handle_list_event_no_packages_but_has_environment_stats() {
        let config = CliConfig::wrap_for_test(test_common::test_config());
        let mut environment_stats = std::collections::HashMap::new();
        environment_stats.insert("macos".to_string(), 3);
        environment_stats.insert("ubuntu".to_string(), 2);

        let package_list = selfie::package::event::PackageListData {
            valid_packages: vec![],
            invalid_packages: vec![],
            current_environment: TEST_ENV.to_string(),
            package_directory: "/test/path".to_string(),
            environment_stats,
        };

        let event = event::PackageEvent::PackageListLoaded {
            operation_info: test_common::create_test_operation_info("package_list", "", TEST_ENV),
            package_list,
        };

        // Should handle no packages but with environment stats (shows environment stats)
        let result = test_handle_event(&event, &config, false);
        assert!(result);
    }

    #[test]
    fn test_handle_list_event_with_valid_packages() {
        let config = CliConfig::wrap_for_test(test_common::test_config());
        let package_item = selfie::package::event::PackageListItem {
            name: "test-package".to_string(),
            version: TEST_VERSION.to_string(),
            environments: vec![TEST_ENV.to_string()],
            status: Some(event::CheckResult::Success {
                stdout: String::new(),
                stderr: String::new(),
            }),
        };

        let mut environment_stats = std::collections::HashMap::new();
        environment_stats.insert(TEST_ENV.to_string(), 1);

        let package_list = selfie::package::event::PackageListData {
            valid_packages: vec![package_item],
            invalid_packages: vec![],
            current_environment: TEST_ENV.to_string(),
            package_directory: "/test/path".to_string(),
            environment_stats,
        };

        let event = event::PackageEvent::PackageListLoaded {
            operation_info: test_common::create_test_operation_info("package_list", "", TEST_ENV),
            package_list,
        };

        // Should handle valid packages (displays summary)
        let result = test_handle_event(&event, &config, false);
        assert!(result);
    }

    #[test]
    fn test_handle_list_event_with_invalid_packages_only() {
        let config = CliConfig::wrap_for_test(test_common::test_config());
        let invalid_package = selfie::package::event::InvalidPackageInfo {
            path: "/test/invalid.yml".to_string(),
            error: "missing field `name`".to_string(),
        };

        let package_list = selfie::package::event::PackageListData {
            valid_packages: vec![],
            invalid_packages: vec![invalid_package],
            current_environment: TEST_ENV.to_string(),
            package_directory: "/test/path".to_string(),
            environment_stats: std::collections::HashMap::new(),
        };

        let event = event::PackageEvent::PackageListLoaded {
            operation_info: test_common::create_test_operation_info("package_list", "", TEST_ENV),
            package_list,
        };

        // Should handle invalid packages only (shows "No packages found." + invalid list)
        let result = test_handle_event(&event, &config, false);
        assert!(result);
    }

    #[test]
    fn test_handle_list_event_mixed_packages_and_environment_stats() {
        let config = CliConfig::wrap_for_test(test_common::test_config());
        let package_item = selfie::package::event::PackageListItem {
            name: "test-package".to_string(),
            version: TEST_VERSION.to_string(),
            environments: vec![TEST_ENV.to_string()],
            status: Some(event::CheckResult::Success {
                stdout: String::new(),
                stderr: String::new(),
            }),
        };

        let invalid_package = selfie::package::event::InvalidPackageInfo {
            path: "/test/invalid.yml".to_string(),
            error: "missing field `name`".to_string(),
        };

        let mut environment_stats = std::collections::HashMap::new();
        environment_stats.insert(TEST_ENV.to_string(), 1);
        environment_stats.insert("other-env".to_string(), 2);

        let package_list = selfie::package::event::PackageListData {
            valid_packages: vec![package_item],
            invalid_packages: vec![invalid_package],
            current_environment: TEST_ENV.to_string(),
            package_directory: "/test/path".to_string(),
            environment_stats,
        };

        let event = event::PackageEvent::PackageListLoaded {
            operation_info: test_common::create_test_operation_info("package_list", "", TEST_ENV),
            package_list,
        };

        // Should handle mixed scenario (shows summary + invalid list)
        let result = test_handle_event(&event, &config, false);
        assert!(result);
    }

    #[test]
    fn test_handle_list_event_package_list_ready() {
        let config = CliConfig::wrap_for_test(test_common::test_config());
        let packages = vec![
            PackageListItem {
                name: "alpha".to_string(),
                version: TEST_VERSION.to_string(),
                environments: vec![TEST_ENV.to_string()],
                status: None,
            },
            PackageListItem {
                name: "beta".to_string(),
                version: TEST_VERSION.to_string(),
                environments: vec![TEST_ENV.to_string()],
                status: None,
            },
        ];

        let event = event::PackageEvent::PackageListReady {
            operation_info: test_common::create_test_operation_info("package_list", "", TEST_ENV),
            packages,
        };

        // In non-TTY mode, PackageListReady should be handled but not create spinners
        let result = test_handle_event(&event, &config, false);
        assert!(result);
    }

    #[test]
    fn test_handle_list_event_package_item_completed() {
        let config = CliConfig::wrap_for_test(test_common::test_config());
        let package_item = PackageListItem {
            name: "test-package".to_string(),
            version: TEST_VERSION.to_string(),
            environments: vec![TEST_ENV.to_string()],
            status: Some(event::CheckResult::Success {
                stdout: String::new(),
                stderr: String::new(),
            }),
        };

        let event = event::PackageEvent::PackageListItemCompleted {
            operation_info: test_common::create_test_operation_info("package_list", "", TEST_ENV),
            package_item,
        };

        // In non-TTY mode, should print the result line directly
        let result = test_handle_event(&event, &config, false);
        assert!(result);
    }

    #[test]
    fn test_handle_list_event_progress_suppressed() {
        let config = CliConfig::wrap_for_test(test_common::test_config());

        let event = event::PackageEvent::Progress {
            operation_info: test_common::create_test_operation_info("package_list", "", TEST_ENV),
            message: "Checking packages...".to_string(),
            step: 1,
            total_steps: 5,
            percent_complete: 0.2,
        };

        // Progress events should be suppressed (return true = handled)
        let result = test_handle_event(&event, &config, false);
        assert!(result);
    }
}
