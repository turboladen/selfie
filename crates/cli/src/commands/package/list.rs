use crate::tables::{Cell, SafeTable};
use crate::terminal_text::DisplayPath;
use crate::terminal_text::{Shown, lit, shown};
use selfie::package::{event, service::PackageService};

use crate::source_paths::base_directory_line;
use crate::{
    config::CliConfig, display_manager::DisplayManager, event_processor::Exit, status_style,
};
use selfie::package::event::BaseKind;

use crate::commands::presentation::{self, RowResult, row_marker};

/// One package's row, buffered until the list loads.
struct ListRow {
    name: String,
    result: RowResult,
    status: Shown,
    /// The environments cell, filled under `--all`.
    environments: Option<Shown>,
}

/// Mutable state accumulated while processing list events.
#[derive(Default)]
struct ListState {
    /// Rows buffered for sorted output at the end.
    buffered_rows: Vec<ListRow>,
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
    pub(crate) async fn handle_command(&self, service: &impl PackageService) -> Exit {
        let event_stream = service.list(self.show_all).await;

        let processor = crate::event_processor::EventProcessor::new(self.display.clone());
        let config = self.config;
        let display = &self.display;
        let show_all = self.show_all;
        let use_colors = config.use_colors();

        let mut state = ListState::default();

        let result = processor
            .process_events(event_stream, |event| {
                handle_list_event(event, config, display, use_colors, show_all, &mut state)
            })
            .await;
        result.exit
    }
}

/// The row for one checked package: its marker, its status cell and, under
/// `--all`, its environments cell.
fn format_list_item(
    package_item: &event::PackageListItem,
    config: &CliConfig,
    use_colors: bool,
    show_all: bool,
) -> ListRow {
    use event::{CheckFailure, EnvironmentStatus, UnknownStatus};

    let status = status_style::format_list_status(&package_item.status, use_colors);

    let result = match &package_item.status {
        EnvironmentStatus::Installed => RowResult::Success,
        // A canceled check says nothing about the package, so like a row with
        // nothing to check it is a warning, not a failure.
        EnvironmentStatus::Unknown(
            UnknownStatus::CheckFailed(CheckFailure::Canceled { .. })
            | UnknownStatus::NoCheckCommand
            | UnknownStatus::NotInEnvironment,
        ) => RowResult::Warning,
        EnvironmentStatus::NotInstalled | EnvironmentStatus::Unknown(_) => RowResult::Failure,
    };

    let environments = show_all.then(|| {
        presentation::format_environment_names(
            &package_item.environments,
            config.environment(),
            config,
        )
    });

    ListRow {
        name: package_item.name.clone(),
        result,
        status,
        environments,
    }
}

fn handle_list_event(
    event: &event::PackageEvent,
    config: &CliConfig,
    display: &DisplayManager,
    use_colors: bool,
    show_all: bool,
    state: &mut ListState,
) -> bool {
    match event {
        event::PackageEvent::PackageListReady { packages, .. } => {
            state.total_packages = packages.len();
            state.checked_count = 0;
            // Results are buffered and printed sorted once every check is done.
            // The wait is the library's waiting step, shown by the shared
            // handler.
            true
        }

        event::PackageEvent::PackageListItemCompleted { package_item, .. } => {
            state.checked_count += 1;
            // Buffered for sorted output once the list loads.
            state
                .buffered_rows
                .push(format_list_item(package_item, config, use_colors, show_all));
            true
        }

        event::PackageEvent::PackageListLoaded { package_list, .. } => {
            // One table, the answer: each package's status, and its environments
            // under `--all`, in their own columns.
            state.buffered_rows.sort_by(|a, b| a.name.cmp(&b.name));
            let mut table = SafeTable::new();
            let mut header = vec!["", "Package", "Status"];
            if show_all {
                header.push("Environments");
            }
            table.set_header(header);
            let package_dir = std::path::Path::new(&package_list.package_directory);
            let failed =
                |text: String| Cell::from(Shown::prose(&text).paint(use_colors, |s| s.red()));
            let mut add = |result: &RowResult, name: &str, status: Cell, envs: Option<Shown>| {
                let mut row = vec![row_marker(result, use_colors).into(), name.into(), status];
                if show_all {
                    row.push(envs.map_or_else(|| "-".into(), Cell::from));
                }
                table.add_row(row);
            };
            for row in &state.buffered_rows {
                add(
                    &row.result,
                    &row.name,
                    row.status.clone().into(),
                    row.environments.clone(),
                );
            }

            // Every invalid package, whatever environment is selected. A spec that
            // failed to parse has no environment list to filter on, so leaving one
            // out would report a clean run over a broken file.
            for invalid in &package_list.invalid_packages {
                let filename = invalid.display_name();
                add(
                    &RowResult::Failure,
                    &filename,
                    failed(format!("unparsable: {invalid}")),
                    None,
                );
            }

            // Whatever environment is selected, as for an unparsable spec: a refused
            // file cannot say which environments it declares. Its path is relative
            // to the package directory named above the table.
            for refused in &package_list.refused {
                let path = refused.path.as_path();
                let shown = path.strip_prefix(package_dir).unwrap_or(path);
                add(
                    &RowResult::Failure,
                    &refused.package_name,
                    failed(format!(
                        "refused ({}): {}",
                        DisplayPath::of(shown),
                        refused.reason
                    )),
                    None,
                );
            }

            let rows = state.buffered_rows.len()
                + package_list.invalid_packages.len()
                + package_list.refused.len();
            // Named before the table, whose refused rows give paths relative to
            // it, so output cut short still says where they are; worded as
            // `dotfiles list` and apply name the same directory.
            display.println(base_directory_line(BaseKind::PackageDirectory, package_dir));
            if rows > 0 {
                display.println(shown!("{}", table));
            }

            let valid = package_list.valid_packages.len();
            let invalid = package_list.invalid_packages.len();
            let refused = package_list.refused.len();

            // The hints are about what the environment filter left, so they are
            // given only when the listing found nothing at all to show.
            if valid + invalid + refused == 0 && package_list.environment_stats.is_empty() {
                display.println(lit!("No packages found."));
            } else if valid + invalid + refused == 0 {
                display.println(shown!(
                    "No packages found for environment '{}'.",
                    Shown::name(config.environment())
                ));
                display_environment_stats(&package_list.environment_stats, config, display);
            }
            // The counts are the summary line's, printed once, by the shared
            // handler.
            true
        }

        _ => false,
    }
}

fn display_environment_stats(
    environment_stats: &std::collections::HashMap<String, usize>,
    config: &CliConfig,
    display: &DisplayManager,
) {
    if environment_stats.is_empty() {
        return;
    }

    display.println(lit!(""));
    display.println(lit!("Packages by environment in this directory:"));

    // Sort environments by package count (descending), then by name
    let mut env_counts: Vec<(String, usize)> = environment_stats
        .iter()
        .map(|(env, count)| (env.clone(), *count))
        .collect();
    env_counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    let mut table = SafeTable::new();
    table.set_header(["Environment", "Package Count"]);

    let use_colors = config.use_colors();
    for (env_name, count) in env_counts {
        let env_cell = if env_name == config.environment() {
            Cell::painted(&env_name, use_colors, |s| s.green().bold())
        } else {
            env_name.into()
        };
        table.add_row([
            env_cell,
            Cell::painted(&count.to_string(), use_colors, |s| s.cyan()),
        ]);
    }

    display.println(shown!("{}", table));

    // Advice about the run, so on stderr, as one suggestion: a second line on
    // its own would read as an unrelated note.
    display.print_styled_suggestion(shown!(
        "{} to see packages for a different environment, or {} to see all packages regardless \
         of environment",
        Shown::from(lit!("--environment <env>")).paint(use_colors, |s| s.yellow()),
        Shown::from(lit!("--all")).paint(use_colors, |s| s.yellow())
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use selfie::package::event::PackageListItem;
    use test_common::{ALT_TEST_ENV, TEST_ENV};

    // Helper to call handle_list_event with test defaults (non-TTY, no spinners)
    fn test_handle_event(event: &event::PackageEvent, config: &CliConfig, show_all: bool) -> bool {
        let display = DisplayManager::new(false);
        let use_colors = config.use_colors();
        let mut state = ListState::default();
        handle_list_event(event, config, &display, use_colors, show_all, &mut state)
    }

    // Helper to call handle_list_event with a state the test inspects
    fn test_handle_event_with_state(
        event: &event::PackageEvent,
        config: &CliConfig,
        state: &mut ListState,
    ) -> bool {
        let display = DisplayManager::new(false);
        let use_colors = config.use_colors();
        handle_list_event(event, config, &display, use_colors, false, state)
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

        let result = presentation::format_environment_names(&environments, TEST_ENV, &config);

        // Just test that it doesn't panic and returns something
        assert!(!result.is_empty());
    }

    #[test]
    fn test_format_check_status_installed() {
        let result = status_style::format_list_status(&event::EnvironmentStatus::Installed, false);
        assert_eq!(result, "Installed");
    }

    #[test]
    fn test_format_check_status_not_installed() {
        let result =
            status_style::format_list_status(&event::EnvironmentStatus::NotInstalled, false);
        assert_eq!(result, "Not installed");
    }

    #[test]
    fn test_format_check_status_no_check() {
        let result = status_style::format_list_status(
            &event::EnvironmentStatus::Unknown(event::UnknownStatus::NoCheckCommand),
            false,
        );
        assert_eq!(result, "No check");
    }

    // A canceled check reads as canceled, not as a red error.
    #[test]
    fn a_canceled_check_reads_as_canceled() {
        let result = status_style::format_list_status(
            &event::EnvironmentStatus::Unknown(event::UnknownStatus::CheckFailed(
                event::CheckFailure::Canceled {
                    message: selfie::commands::BoundedText::bound(b"Command canceled: check-x"),
                },
            )),
            false,
        );
        assert_eq!(result, "Canceled");
    }

    #[test]
    fn test_format_check_status_error() {
        let result = status_style::format_list_status(
            &event::EnvironmentStatus::Unknown(event::UnknownStatus::CheckFailed(
                event::CheckFailure::ResultUnreadable {
                    message: selfie::commands::BoundedText::bound(b"test error"),
                },
            )),
            false,
        );
        assert_eq!(result, "Error: test error");
    }

    #[test]
    fn test_format_check_status_na() {
        let result = status_style::format_list_status(
            &event::EnvironmentStatus::Unknown(event::UnknownStatus::NotInEnvironment),
            false,
        );
        assert_eq!(result, "N/A");
    }

    #[test]
    fn test_format_check_status_with_colors() {
        // Just verify these don't panic and return non-empty strings
        let result = status_style::format_list_status(&event::EnvironmentStatus::Installed, true);
        assert!(result.contains("Installed"));

        let result = status_style::format_list_status(
            &event::EnvironmentStatus::Unknown(event::UnknownStatus::NotInEnvironment),
            true,
        );
        assert!(result.contains("N/A"));
    }

    #[test]
    fn test_handle_list_event_empty_packages_and_stats() {
        let config = CliConfig::wrap_for_test(test_common::test_config());
        let package_list = selfie::package::event::PackageListData {
            valid_packages: vec![],
            invalid_packages: vec![],
            refused: vec![],
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
            refused: vec![],
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
            check_command: None,
            name: "test-package".to_string(),

            environments: vec![TEST_ENV.to_string()],
            status: event::EnvironmentStatus::Installed,
        };

        let mut environment_stats = std::collections::HashMap::new();
        environment_stats.insert(TEST_ENV.to_string(), 1);

        let package_list = selfie::package::event::PackageListData {
            valid_packages: vec![package_item],
            invalid_packages: vec![],
            refused: vec![],
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
        let invalid_package = selfie::package::port::PackageParseError::new(
            "/test/invalid.yml",
            selfie::package::port::PackageParseKind::Unreadable {
                reason: "missing field `name`".to_string(),
            },
        );

        let package_list = selfie::package::event::PackageListData {
            valid_packages: vec![],
            invalid_packages: vec![invalid_package],
            refused: vec![],
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
            check_command: None,
            name: "test-package".to_string(),

            environments: vec![TEST_ENV.to_string()],
            status: event::EnvironmentStatus::Installed,
        };

        let invalid_package = selfie::package::port::PackageParseError::new(
            "/test/invalid.yml",
            selfie::package::port::PackageParseKind::Unreadable {
                reason: "missing field `name`".to_string(),
            },
        );

        let mut environment_stats = std::collections::HashMap::new();
        environment_stats.insert(TEST_ENV.to_string(), 1);
        environment_stats.insert("other-env".to_string(), 2);

        let package_list = selfie::package::event::PackageListData {
            valid_packages: vec![package_item],
            invalid_packages: vec![invalid_package],
            refused: vec![],
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
        let packages = vec!["alpha".to_string(), "beta".to_string()];

        let event = event::PackageEvent::PackageListReady {
            operation_info: test_common::create_test_operation_info("package_list", "", TEST_ENV),
            packages,
        };

        // In non-TTY mode, PackageListReady should be handled but not create spinners
        let result = test_handle_event(&event, &config, false);
        assert!(result);
    }

    // Helper: send Ready + two out-of-order Completed events and return the state.
    fn send_buffered_results() -> ListState {
        let config = CliConfig::wrap_for_test(test_common::test_config());
        let mut state = ListState::default();

        let ready_event = event::PackageEvent::PackageListReady {
            operation_info: test_common::create_test_operation_info("package_list", "", TEST_ENV),
            packages: vec!["zebra".to_string(), "alpha".to_string()],
        };
        test_handle_event_with_state(&ready_event, &config, &mut state);

        let completed_zebra = event::PackageEvent::PackageListItemCompleted {
            operation_info: test_common::create_test_operation_info("package_list", "", TEST_ENV),
            package_item: PackageListItem {
                check_command: None,
                name: "zebra".to_string(),

                environments: vec![TEST_ENV.to_string()],
                status: event::EnvironmentStatus::Installed,
            },
        };
        test_handle_event_with_state(&completed_zebra, &config, &mut state);

        let completed_alpha = event::PackageEvent::PackageListItemCompleted {
            operation_info: test_common::create_test_operation_info("package_list", "", TEST_ENV),
            package_item: PackageListItem {
                check_command: None,
                name: "alpha".to_string(),

                environments: vec![TEST_ENV.to_string()],
                status: event::EnvironmentStatus::NotInstalled,
            },
        };
        test_handle_event_with_state(&completed_alpha, &config, &mut state);

        state
    }

    #[test]
    fn results_are_buffered_until_the_list_loads() {
        let state = send_buffered_results();

        assert_eq!(state.buffered_rows.len(), 2, "Results should be buffered");
        // Buffered in arrival order (zebra first, alpha second)
        assert_eq!(state.buffered_rows[0].name, "zebra");
        assert_eq!(state.buffered_rows[1].name, "alpha");
        assert_eq!(state.checked_count, 2);
    }

    #[test]
    fn test_package_list_loaded_sorts_buffered_rows() {
        // Verify that PackageListLoaded sorts the buffered lines by name.
        let config = CliConfig::wrap_for_test(test_common::test_config());
        let mut state = send_buffered_results();

        // Confirm arrival order before PackageListLoaded
        assert_eq!(state.buffered_rows[0].name, "zebra");
        assert_eq!(state.buffered_rows[1].name, "alpha");

        let loaded_event = event::PackageEvent::PackageListLoaded {
            operation_info: test_common::create_test_operation_info("package_list", "", TEST_ENV),
            package_list: selfie::package::event::PackageListData {
                valid_packages: vec![],
                invalid_packages: vec![],
                refused: vec![],
                current_environment: TEST_ENV.to_string(),
                package_directory: "/test/path".to_string(),
                environment_stats: std::collections::HashMap::new(),
            },
        };
        test_handle_event_with_state(&loaded_event, &config, &mut state);

        // After PackageListLoaded, buffered lines must be sorted alphabetically
        assert_eq!(state.buffered_rows[0].name, "alpha");
        assert_eq!(state.buffered_rows[1].name, "zebra");
    }

    #[test]
    fn test_format_list_item_variants() {
        let config = CliConfig::wrap_for_test(test_common::test_config());

        // Success variant
        let success_item = PackageListItem {
            check_command: None,
            name: "ripgrep".to_string(),

            environments: vec![TEST_ENV.to_string()],
            status: event::EnvironmentStatus::Installed,
        };
        let row = format_list_item(&success_item, &config, false, false);
        let (result, content) = (&row.result, format!("{} {}", row.name, row.status));
        assert!(matches!(result, RowResult::Success));
        assert!(content.contains("ripgrep"));
        assert!(content.contains("Installed"));

        // Failure variant
        let failure_item = PackageListItem {
            check_command: None,
            name: "missing-pkg".to_string(),

            environments: vec![TEST_ENV.to_string()],
            status: event::EnvironmentStatus::NotInstalled,
        };
        let row = format_list_item(&failure_item, &config, false, false);
        let (result, content) = (&row.result, format!("{} {}", row.name, row.status));
        assert!(matches!(result, RowResult::Failure));
        assert!(content.contains("missing-pkg"));
        assert!(content.contains("Not installed"));

        // Warning variant (no check command)
        let warning_item = PackageListItem {
            check_command: None,
            name: "no-check".to_string(),

            environments: vec![TEST_ENV.to_string()],
            status: event::EnvironmentStatus::Unknown(event::UnknownStatus::NoCheckCommand),
        };
        let row = format_list_item(&warning_item, &config, false, false);
        let (result, content) = (&row.result, format!("{} {}", row.name, row.status));
        assert!(matches!(result, RowResult::Warning));
        assert!(content.contains("no-check"));
        assert!(content.contains("No check"));

        // N/A variant (not in this environment)
        let na_item = PackageListItem {
            check_command: None,
            name: "elsewhere".to_string(),

            environments: vec![ALT_TEST_ENV.to_string()],
            status: event::EnvironmentStatus::Unknown(event::UnknownStatus::NotInEnvironment),
        };
        let row = format_list_item(&na_item, &config, false, false);
        let result = &row.result;
        assert!(matches!(result, RowResult::Warning));
    }

    // A canceled check is a warning row, not a failed one: it says nothing
    // about the package.
    #[test]
    fn a_canceled_check_is_a_warning_row() {
        let config = CliConfig::wrap_for_test(test_common::test_config());
        let item = PackageListItem {
            check_command: None,
            name: "slow".to_string(),
            environments: vec![TEST_ENV.to_string()],
            status: event::EnvironmentStatus::Unknown(event::UnknownStatus::CheckFailed(
                event::CheckFailure::Canceled {
                    message: selfie::commands::BoundedText::bound(b"Command canceled: sleep 5"),
                },
            )),
        };

        let row = format_list_item(&item, &config, false, false);

        assert!(matches!(row.result, RowResult::Warning));
        assert_eq!(row.status, "Canceled");
    }

    #[test]
    fn test_handle_list_event_package_item_completed() {
        let config = CliConfig::wrap_for_test(test_common::test_config());
        let package_item = PackageListItem {
            check_command: None,
            name: "test-package".to_string(),

            environments: vec![TEST_ENV.to_string()],
            status: event::EnvironmentStatus::Installed,
        };

        let event = event::PackageEvent::PackageListItemCompleted {
            operation_info: test_common::create_test_operation_info("package_list", "", TEST_ENV),
            package_item,
        };

        // In non-TTY mode, should buffer the result line
        let result = test_handle_event(&event, &config, false);
        assert!(result);
    }

    // A step is left to the shared handler, which decides what shows.
    #[test]
    fn a_list_step_is_left_to_the_shared_handler() {
        let config = CliConfig::wrap_for_test(test_common::test_config());

        let event = event::PackageEvent::Progress {
            operation_info: test_common::create_test_operation_info("package_list", "", TEST_ENV),
            message: "Checking packages...".to_string(),
            step: 1,
            total_steps: 5,
            percent_complete: 0.2,
            kind: selfie::package::event::StepKind::Local,
        };

        let result = test_handle_event(&event, &config, false);
        assert!(!result);
    }
}
