//! How commands lay out what they print: tables, field styling, and the
//! report for a package that does not support the current environment.

use crate::terminal_text::{Literal, Name, Paint, Shown, lit, shown};

use crate::config::CliConfig;
use crate::display_manager::{DisplayManager, INDENT};
use crate::tables::SafeTable;

/// The package command a report is about.
#[derive(Clone, Copy)]
pub(crate) enum PackageCommand {
    Install,
    Check,
    Audit,
}

impl PackageCommand {
    /// Returns the command's name, as `selfie package` takes it.
    pub(crate) fn word(self) -> Literal {
        match self {
            Self::Install => lit!("install"),
            Self::Check => lit!("check"),
            Self::Audit => lit!("audit"),
        }
    }
}

/// How a table row turned out, which decides its marker.
pub(crate) enum RowResult {
    Success,
    Failure,
    Warning,
}

impl From<selfie::package::event::Outcome> for RowResult {
    /// The marker for a row whose operation ended with `outcome`.
    fn from(outcome: selfie::package::event::Outcome) -> Self {
        use selfie::package::event::Outcome;
        match outcome {
            Outcome::Clean => Self::Success,
            Outcome::Found => Self::Warning,
            Outcome::Failed => Self::Failure,
        }
    }
}

/// The color of a row that turned out as `result`.
fn row_color(result: &RowResult) -> Paint {
    match result {
        RowResult::Success => |s| s.green(),
        RowResult::Warning => |s| s.yellow(),
        RowResult::Failure => |s| s.red(),
    }
}

/// `text` in the color of a row that turned out as `result`, or plain when
/// `use_colors` is off.
pub(crate) fn paint_row_text(text: &str, result: &RowResult, use_colors: bool) -> Shown {
    crate::status_style::styled_text(text, use_colors, row_color(result))
}

/// The marker cell for a row.
pub(crate) fn row_marker(result: &RowResult, use_colors: bool) -> Shown {
    let marker = match result {
        RowResult::Success => lit!("✓"),
        RowResult::Warning => lit!("⚠"),
        RowResult::Failure => lit!("✗"),
    };
    let color = row_color(result);
    Shown::from(marker)
        .paint(use_colors, color)
        .paint(use_colors, |s| s.bold())
}

/// Environment names in the order given, which is the order the spec file gives
/// them, with the current environment marked `*`.
pub(crate) fn format_environment_names(
    environments: &[String],
    current_environment: &str,
    config: &CliConfig,
) -> Shown {
    let names = environments.iter().map(|env_name| {
        if env_name == current_environment {
            Shown::from(Name::new(&format!("*{env_name}")))
                .paint(config.use_colors(), |s| s.bold().green())
        } else {
            Shown::from(Name::new(env_name)).paint(config.use_colors(), |s| s.dim().green())
        }
    });
    Shown::join(names, lit!(", "))
}

/// Display environment error with available environments for a specific package
pub(crate) fn display_environment_summary(
    package_name: &str,
    current_environment: &str,
    available_environments: &[String],
    config: &CliConfig,
    display: &DisplayManager,
    command: PackageCommand,
) {
    if available_environments.is_empty() {
        display_generic_environment_suggestion(
            package_name,
            current_environment,
            config,
            display,
            command,
        );
    } else {
        display.print_error(format!(
            "Package '{package_name}' doesn't support environment '{current_environment}'."
        ));
        display.print_note(shown!("{}Available environments for this package:", INDENT));

        let mut table = SafeTable::new();
        // Printed on stderr, so sized to the terminal stderr is on.
        table.use_stderr();
        table.set_header(vec!["Environment"]);

        // The spec file's order, which every other surface lists them in. The
        // current environment is never among them: its absence is the error.
        for env in available_environments {
            table.add_row([env]);
        }

        display.print_note(shown!("{}", table));

        display.print_styled_suggestion(shown!(
            "{} with one of the environments above",
            shown!(
                "selfie package {} --environment <env> <package>",
                command.word()
            )
            .paint(config.use_colors(), |s| s.yellow())
        ));
    }
}

/// Report that `package_name` has no `command` command in `environment`,
/// naming the environments that have one (stderr).
pub(crate) fn display_missing_command(
    display: &DisplayManager,
    command: PackageCommand,
    package_name: &str,
    environment: &str,
    others: &[String],
) {
    display.print_error(format!(
        "No {} command defined for '{package_name}' in environment '{environment}'.",
        command.word()
    ));
    if !others.is_empty() {
        // Sorted, so the line reads the same on every run.
        let mut others = others.to_vec();
        others.sort();
        display.print_note(shown!(
            "Environments with {} commands: {}",
            command.word(),
            Shown::join(others.iter().map(|name| Name::new(name)), lit!(", "))
        ));
    }
}

/// Display generic environment suggestion when specific environment info is not available
pub(crate) fn display_generic_environment_suggestion(
    package_name: &str,
    current_environment: &str,
    config: &CliConfig,
    display: &DisplayManager,
    command: PackageCommand,
) {
    display.print_error(format!(
        "Package '{package_name}' doesn't support environment '{current_environment}'."
    ));
    display.print_note(shown!("{}Try one of these options:", INDENT));
    let use_colors = config.use_colors();
    display.print_note(shown!(
        "{}• {} to {} with a different environment",
        INDENT,
        shown!(
            "selfie package {} --environment <env> <package>",
            command.word()
        )
        .paint(use_colors, |s| s.yellow()),
        command.word()
    ));
    display.print_note(shown!(
        "{}• {} to see which environments this package supports",
        INDENT,
        Shown::from(lit!("selfie spec info <package>")).paint(use_colors, |s| s.yellow())
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_common::test_config_with_dir;

    // Each word a report names is a `selfie package` subcommand, so the enum
    // cannot drift from the CLI. The match fails to compile when a variant is
    // added without a row here.
    #[test]
    fn each_package_command_word_is_a_subcommand() {
        use clap::CommandFactory;

        let cli = crate::cli::ClapCli::command();
        let package = cli
            .find_subcommand("package")
            .expect("selfie has a package command");
        for command in [
            PackageCommand::Install,
            PackageCommand::Check,
            PackageCommand::Audit,
        ] {
            match command {
                PackageCommand::Install | PackageCommand::Check | PackageCommand::Audit => {}
            }
            let word = command.word().as_str();
            assert!(package.find_subcommand(word).is_some(), "{word}");
        }
    }

    // The environments that have the command are listed in order, whatever
    // order the spec's map gave them.
    #[test]
    fn a_missing_command_lists_the_others_in_order() {
        let display = DisplayManager::new(false);

        display_missing_command(
            &display,
            PackageCommand::Check,
            "bat",
            "test",
            &["macos-work".to_string(), "linux".to_string()],
        );

        assert_eq!(
            display.printed().last().map(|(_, line)| line.as_str()),
            Some("Environments with check commands: linux, macos-work")
        );
    }

    // An environment the package does not support is a failure, so every line
    // explaining it, the table and the suggestion included, is on stderr.
    #[test]
    fn an_environment_summary_is_all_on_stderr() {
        use crate::display_manager::Channel;

        let config = CliConfig::wrap_for_test(test_config_with_dir(std::path::Path::new("/p")));
        let display = DisplayManager::new(false);

        display_environment_summary(
            "bat",
            "test",
            &["macos".to_string(), "linux".to_string()],
            &config,
            &display,
            PackageCommand::Check,
        );

        let printed = display.printed();
        assert!(printed.len() > 3, "{printed:?}");
        assert!(
            printed.iter().all(|(stream, _)| *stream == Channel::Stderr),
            "{printed:?}"
        );
        assert!(
            printed
                .iter()
                .any(|(_, line)| line.contains("doesn't support environment 'test'")),
            "{printed:?}"
        );
    }

    // The generic form, for a package that lists no environments, as well.
    #[test]
    fn a_generic_environment_suggestion_is_all_on_stderr() {
        use crate::display_manager::Channel;

        let config = CliConfig::wrap_for_test(test_config_with_dir(std::path::Path::new("/p")));
        let display = DisplayManager::new(false);

        display_generic_environment_suggestion(
            "bat",
            "test",
            &config,
            &display,
            PackageCommand::Check,
        );

        let printed = display.printed();
        assert!(printed.len() > 2, "{printed:?}");
        assert!(
            printed.iter().all(|(stream, _)| *stream == Channel::Stderr),
            "{printed:?}"
        );
    }

    // The environments a package supports are listed in its file's order, as
    // everywhere else, not sorted.
    #[test]
    fn the_environment_summary_lists_environments_in_file_order() {
        let config = CliConfig::wrap_for_test(test_config_with_dir("/test/packages"));
        let display = DisplayManager::new(false);

        display_environment_summary(
            "tool",
            "work",
            &["zeta".to_string(), "alpha".to_string()],
            &config,
            &display,
            PackageCommand::Install,
        );

        let printed: String = display
            .printed()
            .into_iter()
            .map(|(_, line)| line)
            .collect::<Vec<_>>()
            .join("\n");
        let zeta = printed.find("zeta").expect("zeta listed");
        let alpha = printed.find("alpha").expect("alpha listed");
        assert!(zeta < alpha, "{printed}");
    }

    #[test]
    fn test_format_environment_names() {
        // Test environment name formatting without filesystem operations
        let package_dir = std::path::PathBuf::from("/test/packages");
        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));
        let environments = vec!["test".to_string(), "production".to_string()];

        let result = format_environment_names(&environments, "test", &config);

        // Just test that it doesn't panic and returns something
        assert!(!result.is_empty());
        assert!(result.contains("test"));
    }

    // The spec file's order, on every surface: the same file must not list its
    // environments one way here and another way in the MCP server's answer. The
    // current environment is marked where it stands, not moved to the front.
    #[test]
    fn test_format_environment_names_ordering() {
        let package_dir = std::path::PathBuf::from("/test/packages");
        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));

        let environments = vec!["zeta".to_string(), "alpha".to_string(), "mid".to_string()];

        let result = format_environment_names(&environments, "mid", &config);

        assert_eq!(result, "zeta, alpha, *mid");
    }

    #[test]
    fn test_format_environment_names_single_environment() {
        let package_dir = std::path::PathBuf::from("/test/packages");
        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));

        let environments = vec!["macos-work".to_string()];
        let result = format_environment_names(&environments, "macos-work", &config);

        assert_eq!(result, "*macos-work");
    }

    #[test]
    fn test_format_environment_names_current_not_present() {
        let package_dir = std::path::PathBuf::from("/test/packages");
        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));

        let environments = vec!["arch-home".to_string(), "ubuntu-server".to_string()];
        let result = format_environment_names(&environments, "macos-work", &config);

        // Should not contain asterisk since current environment is not in the list
        assert!(!result.contains('*'));
        assert_eq!(result, "arch-home, ubuntu-server");
    }
}
