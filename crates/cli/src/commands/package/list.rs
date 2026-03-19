use std::borrow::Cow;

use console::style;
use selfie::package::{event, service::PackageService};

use crate::{
    config::CliConfig,
    display_manager::{DisplayManager, OperationHandle},
    status_style,
};

use super::common;

/// Result variant for a formatted list item, determining which finish method to call.
enum ListItemResult {
    Success,
    Failure,
    Warning,
}

/// Mutable state accumulated while processing list events.
#[derive(Default)]
struct ListState {
    /// Single progress spinner showing "Checking (N/M)..." (TTY only).
    progress_spinner: Option<OperationHandle>,
    /// Buffered (name, formatted_line) pairs for sorted output at the end.
    buffered_lines: Vec<(String, String)>,
    max_name_len: usize,
    total_packages: usize,
    checked_count: usize,
}

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

        let mut state = ListState::default();

        let result = processor
            .process_events(event_stream, |event| {
                handle_list_event(
                    event, config, display, use_colors, show_all, is_tty, &mut state,
                )
            })
            .await;
        result.exit_code
    }
}

/// Extract formatted content and result variant from a list item.
///
/// Returns the result variant (for choosing the prefix symbol) and the content
/// string **without** the prefix — the caller prepends it via `plain_prefix()`.
fn format_list_item(
    package_item: &event::PackageListItem,
    config: &CliConfig,
    use_colors: bool,
    show_all: bool,
    max_name_len: usize,
) -> (ListItemResult, String) {
    let status_text = status_style::format_check_result(package_item.status.as_ref(), use_colors);
    let version = format!("v{}", package_item.version);

    let result = match &package_item.status {
        Some(event::CheckResult::Success { .. }) => ListItemResult::Success,
        Some(event::CheckResult::Failed { .. })
        | Some(event::CheckResult::CommandNotFound)
        | Some(event::CheckResult::Error(_)) => ListItemResult::Failure,
        Some(event::CheckResult::NoCheckCommand) | None => ListItemResult::Warning,
    };

    let content = if show_all {
        let envs = common::format_environment_names(
            &package_item.environments,
            config.environment(),
            config,
        );
        format!(
            "{:<width$}  {:<10}  {status_text}  ({envs})",
            package_item.name,
            version,
            width = max_name_len
        )
    } else {
        format!(
            "{:<width$}  {:<10}  {status_text}",
            package_item.name,
            version,
            width = max_name_len
        )
    };

    (result, content)
}

/// Build a plain-text prefix for buffered output.
fn plain_prefix(result: &ListItemResult, use_colors: bool) -> Cow<'static, str> {
    match result {
        ListItemResult::Success => {
            if use_colors {
                style("✓").green().bold().to_string().into()
            } else {
                "✓".into()
            }
        }
        ListItemResult::Failure => {
            if use_colors {
                style("✗").red().bold().to_string().into()
            } else {
                "✗".into()
            }
        }
        ListItemResult::Warning => {
            if use_colors {
                style("⚠").yellow().bold().to_string().into()
            } else {
                "⚠".into()
            }
        }
    }
}

fn handle_list_event(
    event: &event::PackageEvent,
    config: &CliConfig,
    display: &DisplayManager,
    use_colors: bool,
    show_all: bool,
    is_tty: bool,
    state: &mut ListState,
) -> bool {
    match event {
        event::PackageEvent::PackageListReady { packages, .. } => {
            state.max_name_len = packages.iter().map(|p| p.name.len()).max().unwrap_or(0);
            state.total_packages = packages.len();
            state.checked_count = 0;

            if is_tty && !packages.is_empty() {
                // Single progress spinner — results are buffered and printed
                // sorted after all checks complete. Using one spinner avoids
                // the rendering issues that arise when 100+ spinners tick
                // simultaneously in MultiProgress.
                state.progress_spinner = Some(display.start_list_spinner(format!(
                    "Checking packages (0/{})...",
                    state.total_packages
                )));
            }
            true
        }

        event::PackageEvent::PackageListItemCompleted { package_item, .. } => {
            state.checked_count += 1;
            let (result, content) = format_list_item(
                package_item,
                config,
                use_colors,
                show_all,
                state.max_name_len,
            );

            // Buffer result for sorted output at the end
            let prefix = plain_prefix(&result, use_colors);
            let line = format!("{prefix} {content}");
            state.buffered_lines.push((package_item.name.clone(), line));

            // Update progress spinner (TTY only)
            if let Some(ref progress) = state.progress_spinner {
                progress.set_message(format!(
                    "Checking packages ({}/{})...",
                    state.checked_count, state.total_packages
                ));
            }
            true
        }

        event::PackageEvent::PackageListLoaded { package_list, .. } => {
            // Clear progress spinner before printing results
            if let Some(handle) = state.progress_spinner.take() {
                handle.finish_clear();
            }

            // Print buffered lines sorted by name
            state.buffered_lines.sort_by(|a, b| a.0.cmp(&b.0));
            for (_, line) in &state.buffered_lines {
                println!("{line}");
            }

            // Print invalid packages inline with error status,
            // filtered to the current environment (unless --all)
            if !package_list.invalid_packages.is_empty() {
                let current_env = config.environment();
                for invalid in &package_list.invalid_packages {
                    let clean_error = clean_error_message(&invalid.error, &invalid.path);

                    if !show_all && !error_matches_environment(&clean_error, current_env) {
                        continue;
                    }

                    let filename = std::path::Path::new(&invalid.path)
                        .file_stem()
                        .and_then(|n| n.to_str())
                        .unwrap_or(&invalid.path);

                    let error_text = if use_colors {
                        style(clean_error).red().to_string()
                    } else {
                        clean_error
                    };

                    let prefix = if use_colors {
                        style("✗").red().bold().to_string()
                    } else {
                        "✗".to_string()
                    };

                    let line = format!(
                        "{prefix} {:<width$}  {:<10}  {error_text}",
                        filename,
                        "",
                        width = state.max_name_len
                    );

                    println!("{line}");
                }
            }

            println!();
            println!("Package directory: {}", package_list.package_directory);

            let valid = package_list.valid_packages.len();
            let invalid = package_list.invalid_packages.len();
            let total = valid + invalid;

            if total == 0 && package_list.environment_stats.is_empty() {
                println!("No packages found.");
            } else if valid == 0 && invalid == 0 {
                println!(
                    "No packages found for environment '{}'.",
                    config.environment()
                );
                display_environment_stats(&package_list.environment_stats, config);
            } else if invalid > 0 {
                println!("{valid} valid, {invalid} invalid");
            } else {
                println!("{valid} packages");
            }
            true
        }

        event::PackageEvent::Progress { .. } => {
            true // Suppress — spinner is the progress indicator
        }

        _ => false,
    }
}

/// Check if an error message is relevant to a specific environment.
///
/// Since invalid packages failed to parse, we can't inspect their environment
/// list directly. Instead, check if the error mentions the environment name
/// (e.g., "environments.macos-home: missing field"). Errors that don't mention
/// any environment are shown regardless (they affect all environments).
fn error_matches_environment(error: &str, environment: &str) -> bool {
    if error.contains("environments.") {
        // Error is environment-specific — only show if it matches
        error.contains(&format!("environments.{environment}"))
    } else {
        // Error is not environment-specific (e.g., missing `name` field) — always show
        true
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
    println!("Packages by environment in this directory:");

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
            "Try: {} to see packages for a different environment",
            console::style("--environment <env>").yellow()
        );
        println!(
            "   or: {} to see all packages regardless of environment",
            console::style("--all").yellow()
        );
    } else {
        println!("Try: --environment <env> to see packages for a different environment");
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
        let mut state = ListState::default();
        handle_list_event(
            event, config, &display, use_colors, show_all, false, &mut state,
        )
    }

    /// Helper to call handle_list_event with explicit is_tty flag
    fn test_handle_event_with_tty(
        event: &event::PackageEvent,
        config: &CliConfig,
        is_tty: bool,
        state: &mut ListState,
    ) -> bool {
        let display = DisplayManager::new(false);
        let use_colors = config.use_colors();
        handle_list_event(event, config, &display, use_colors, false, is_tty, state)
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
    fn test_format_environments() {
        let config = CliConfig::wrap_for_test(test_common::test_config());
        let environments = vec![TEST_ENV.to_string(), ALT_TEST_ENV.to_string()];

        let result = common::format_environment_names(&environments, TEST_ENV, &config);

        // Just test that it doesn't panic and returns something
        assert!(!result.is_empty());
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
        let result = status_style::format_check_result(
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
        let result = status_style::format_check_result(
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
        let result =
            status_style::format_check_result(Some(&event::CheckResult::NoCheckCommand), false);
        assert_eq!(result, "No check");
    }

    #[test]
    fn test_format_check_status_cmd_not_found() {
        let result =
            status_style::format_check_result(Some(&event::CheckResult::CommandNotFound), false);
        assert_eq!(result, "Cmd not found");
    }

    #[test]
    fn test_format_check_status_error() {
        let result = status_style::format_check_result(
            Some(&event::CheckResult::Error("test error".to_string())),
            false,
        );
        assert_eq!(result, "Error: test error");
    }

    #[test]
    fn test_format_check_status_na() {
        let result = status_style::format_check_result(None, false);
        assert_eq!(result, "N/A");
    }

    #[test]
    fn test_format_check_status_with_colors() {
        // Just verify these don't panic and return non-empty strings
        let result = status_style::format_check_result(
            Some(&event::CheckResult::Success {
                stdout: String::new(),
                stderr: String::new(),
            }),
            true,
        );
        assert!(result.contains("Installed"));

        let result = status_style::format_check_result(None, true);
        assert!(result.contains("N/A"));
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
    fn test_handle_list_event_non_tty_skips_spinner() {
        // When stdout is piped (is_tty=false), no progress spinner should be
        // created; output goes to buffered_lines printed at the end.
        let config = CliConfig::wrap_for_test(test_common::test_config());
        let mut state = ListState::default();

        let packages = vec![PackageListItem {
            name: "alpha".to_string(),
            version: TEST_VERSION.to_string(),
            environments: vec![TEST_ENV.to_string()],
            status: None,
        }];

        let event = event::PackageEvent::PackageListReady {
            operation_info: test_common::create_test_operation_info("package_list", "", TEST_ENV),
            packages,
        };

        test_handle_event_with_tty(&event, &config, false, &mut state);

        assert!(
            state.progress_spinner.is_none(),
            "Non-TTY mode must not create a progress spinner"
        );
    }

    #[test]
    fn test_handle_list_event_tty_creates_progress_spinner() {
        // When both stdout and stderr are terminals (is_tty=true),
        // a single progress spinner should be created for the checking phase.
        let config = CliConfig::wrap_for_test(test_common::test_config());
        let mut state = ListState::default();

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

        test_handle_event_with_tty(&event, &config, true, &mut state);

        assert!(
            state.progress_spinner.is_some(),
            "TTY mode should create a progress spinner"
        );
        assert_eq!(state.total_packages, 2);
    }

    /// Helper: send Ready + two out-of-order Completed events and return the state.
    fn send_buffered_results(is_tty: bool) -> ListState {
        let config = CliConfig::wrap_for_test(test_common::test_config());
        let mut state = ListState::default();

        let ready_event = event::PackageEvent::PackageListReady {
            operation_info: test_common::create_test_operation_info("package_list", "", TEST_ENV),
            packages: vec![
                PackageListItem {
                    name: "zebra".to_string(),
                    version: TEST_VERSION.to_string(),
                    environments: vec![TEST_ENV.to_string()],
                    status: None,
                },
                PackageListItem {
                    name: "alpha".to_string(),
                    version: TEST_VERSION.to_string(),
                    environments: vec![TEST_ENV.to_string()],
                    status: None,
                },
            ],
        };
        test_handle_event_with_tty(&ready_event, &config, is_tty, &mut state);

        let completed_zebra = event::PackageEvent::PackageListItemCompleted {
            operation_info: test_common::create_test_operation_info("package_list", "", TEST_ENV),
            package_item: PackageListItem {
                name: "zebra".to_string(),
                version: TEST_VERSION.to_string(),
                environments: vec![TEST_ENV.to_string()],
                status: Some(event::CheckResult::Success {
                    stdout: String::new(),
                    stderr: String::new(),
                }),
            },
        };
        test_handle_event_with_tty(&completed_zebra, &config, is_tty, &mut state);

        let completed_alpha = event::PackageEvent::PackageListItemCompleted {
            operation_info: test_common::create_test_operation_info("package_list", "", TEST_ENV),
            package_item: PackageListItem {
                name: "alpha".to_string(),
                version: TEST_VERSION.to_string(),
                environments: vec![TEST_ENV.to_string()],
                status: Some(event::CheckResult::Failed {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: Some(1),
                }),
            },
        };
        test_handle_event_with_tty(&completed_alpha, &config, is_tty, &mut state);

        state
    }

    #[test]
    fn test_non_tty_buffers_results() {
        let state = send_buffered_results(false);

        assert_eq!(state.buffered_lines.len(), 2, "Results should be buffered");
        // Buffered in arrival order (zebra first, alpha second)
        assert_eq!(state.buffered_lines[0].0, "zebra");
        assert_eq!(state.buffered_lines[1].0, "alpha");
        assert_eq!(state.checked_count, 2);
    }

    #[test]
    fn test_tty_buffers_results() {
        // The key change: TTY mode now buffers results too (instead of
        // resolving per-package spinners in-place).
        let state = send_buffered_results(true);

        assert_eq!(
            state.buffered_lines.len(),
            2,
            "TTY mode must also buffer results"
        );
        assert_eq!(state.buffered_lines[0].0, "zebra");
        assert_eq!(state.buffered_lines[1].0, "alpha");
        assert_eq!(state.checked_count, 2);
    }

    #[test]
    fn test_package_list_loaded_sorts_buffered_lines() {
        // Verify that PackageListLoaded sorts the buffered lines by name.
        let config = CliConfig::wrap_for_test(test_common::test_config());
        let mut state = send_buffered_results(false);

        // Confirm arrival order before PackageListLoaded
        assert_eq!(state.buffered_lines[0].0, "zebra");
        assert_eq!(state.buffered_lines[1].0, "alpha");

        let loaded_event = event::PackageEvent::PackageListLoaded {
            operation_info: test_common::create_test_operation_info("package_list", "", TEST_ENV),
            package_list: selfie::package::event::PackageListData {
                valid_packages: vec![],
                invalid_packages: vec![],
                current_environment: TEST_ENV.to_string(),
                package_directory: "/test/path".to_string(),
                environment_stats: std::collections::HashMap::new(),
            },
        };
        test_handle_event_with_tty(&loaded_event, &config, false, &mut state);

        // After PackageListLoaded, buffered lines must be sorted alphabetically
        assert_eq!(state.buffered_lines[0].0, "alpha");
        assert_eq!(state.buffered_lines[1].0, "zebra");
    }

    #[test]
    fn test_format_list_item_variants() {
        let config = CliConfig::wrap_for_test(test_common::test_config());

        // Success variant
        let success_item = PackageListItem {
            name: "ripgrep".to_string(),
            version: TEST_VERSION.to_string(),
            environments: vec![TEST_ENV.to_string()],
            status: Some(event::CheckResult::Success {
                stdout: String::new(),
                stderr: String::new(),
            }),
        };
        let (result, content) = format_list_item(&success_item, &config, false, false, 10);
        assert!(matches!(result, ListItemResult::Success));
        assert!(content.contains("ripgrep"));
        assert!(content.contains("Installed"));

        // Failure variant
        let failure_item = PackageListItem {
            name: "missing-pkg".to_string(),
            version: TEST_VERSION.to_string(),
            environments: vec![TEST_ENV.to_string()],
            status: Some(event::CheckResult::Failed {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(1),
            }),
        };
        let (result, content) = format_list_item(&failure_item, &config, false, false, 12);
        assert!(matches!(result, ListItemResult::Failure));
        assert!(content.contains("missing-pkg"));
        assert!(content.contains("Not installed"));

        // Warning variant (no check command)
        let warning_item = PackageListItem {
            name: "no-check".to_string(),
            version: TEST_VERSION.to_string(),
            environments: vec![TEST_ENV.to_string()],
            status: Some(event::CheckResult::NoCheckCommand),
        };
        let (result, content) = format_list_item(&warning_item, &config, false, false, 10);
        assert!(matches!(result, ListItemResult::Warning));
        assert!(content.contains("no-check"));
        assert!(content.contains("No check"));

        // N/A variant (status is None)
        let na_item = PackageListItem {
            name: "pending".to_string(),
            version: TEST_VERSION.to_string(),
            environments: vec![TEST_ENV.to_string()],
            status: None,
        };
        let (result, _) = format_list_item(&na_item, &config, false, false, 10);
        assert!(matches!(result, ListItemResult::Warning));
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

        // In non-TTY mode, should buffer the result line
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
