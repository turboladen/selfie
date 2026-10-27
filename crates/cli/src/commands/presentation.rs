//! How commands lay out what they print: tables, field styling, and the
//! report for a package that does not support the current environment.

use comfy_table::{ContentArrangement, Table, presets};
use console::style;

use crate::config::CliConfig;
use crate::display_manager::{DisplayManager, INDENT};

/// Create a formatted table with consistent styling
pub(crate) fn create_formatted_table() -> Table {
    let mut table = Table::new();
    table
        .load_style(presets::UTF8_FULL_CONDENSED.with_rounded_corners())
        .set_content_arrangement(ContentArrangement::Dynamic);
    table
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
fn row_color(
    result: &RowResult,
) -> fn(console::StyledObject<String>) -> console::StyledObject<String> {
    match result {
        RowResult::Success => |s| s.green(),
        RowResult::Warning => |s| s.yellow(),
        RowResult::Failure => |s| s.red(),
    }
}

/// `text` in the color of a row that turned out as `result`, or plain when
/// `use_colors` is off.
pub(crate) fn paint_row_text(text: &str, result: &RowResult, use_colors: bool) -> String {
    crate::status_style::styled_text(text, use_colors, row_color(result))
}

/// The marker cell for a row.
pub(crate) fn row_marker(result: &RowResult, use_colors: bool) -> String {
    let marker = match result {
        RowResult::Success => "✓",
        RowResult::Warning => "⚠",
        RowResult::Failure => "✗",
    };
    if use_colors {
        row_color(result)(style(marker.to_string()))
            .bold()
            .to_string()
    } else {
        marker.to_string()
    }
}

/// Environment names in the order given, which is the order the spec file gives
/// them, with the current environment marked `*`.
pub(crate) fn format_environment_names(
    environments: &[String],
    current_environment: &str,
    config: &CliConfig,
) -> String {
    environments
        .iter()
        .map(|env_name| {
            if env_name == current_environment {
                let env = format!("*{env_name}");
                if config.use_colors() {
                    style(env).bold().green().to_string()
                } else {
                    env
                }
            } else if config.use_colors() {
                style(env_name).dim().green().to_string()
            } else {
                env_name.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Format a key with consistent styling
pub(crate) fn format_field_key(key: &str, use_colors: bool) -> String {
    if use_colors {
        style(key).cyan().bold().to_string()
    } else {
        key.to_string()
    }
}

/// Format a value with consistent styling
pub(crate) fn format_field_value(value: &str, use_colors: bool) -> String {
    if use_colors {
        style(value).white().to_string()
    } else {
        value.to_string()
    }
}

/// Display environment error with available environments for a specific package
pub(crate) fn display_environment_summary(
    package_name: &str,
    current_environment: &str,
    available_environments: &[String],
    config: &CliConfig,
    display: &DisplayManager,
    context: &str, // "check" or "install"
) {
    if available_environments.is_empty() {
        display_generic_environment_suggestion(
            package_name,
            current_environment,
            config,
            display,
            context,
        );
    } else {
        display.print_error(format!(
            "Package '{package_name}' doesn't support environment '{current_environment}'."
        ));
        display.print_note(format!("{INDENT}Available environments for this package:"));

        let mut table = create_formatted_table();
        // Printed on stderr, so sized to the terminal stderr is on.
        table.use_stderr();
        table.set_header(vec!["Environment"]);

        // The spec file's order, which every other surface lists them in. The
        // current environment is never among them: its absence is the error.
        for env in available_environments {
            table.add_row(vec![env.as_str()]);
        }

        display.print_note(format!("{table}"));

        if config.use_colors() {
            display.print_suggestion(format!(
                "{} with one of the environments above",
                console::style(format!(
                    "selfie package {context} --environment <env> <package>"
                ))
                .yellow()
            ));
        } else {
            display.print_suggestion(format!(
                "selfie package {context} --environment <env> <package> with one of the environments above"
            ));
        }
    }
}

/// Report that `package_name` has no `kind` command ("check", "install") in
/// `environment`, naming the environments that have one (stderr).
pub(crate) fn display_missing_command(
    display: &DisplayManager,
    kind: &str,
    package_name: &str,
    environment: &str,
    others: &[String],
) {
    display.print_error(format!(
        "No {kind} command defined for '{package_name}' in environment '{environment}'."
    ));
    if !others.is_empty() {
        // Sorted, so the line reads the same on every run.
        let mut others = others.to_vec();
        others.sort();
        display.print_note(format!(
            "Environments with {kind} commands: {}",
            others.join(", ")
        ));
    }
}

/// Display generic environment suggestion when specific environment info is not available
pub(crate) fn display_generic_environment_suggestion(
    package_name: &str,
    current_environment: &str,
    config: &CliConfig,
    display: &DisplayManager,
    context: &str, // "check" or "install"
) {
    display.print_error(format!(
        "Package '{package_name}' doesn't support environment '{current_environment}'."
    ));
    display.print_note(format!("{INDENT}Try one of these options:"));
    if config.use_colors() {
        display.print_note(format!(
            "{INDENT}• {} to {} with a different environment",
            console::style(format!(
                "selfie package {context} --environment <env> <package>"
            ))
            .yellow(),
            context
        ));
        display.print_note(format!(
            "{INDENT}• {} to see which environments this package supports",
            console::style("selfie spec info <package>").yellow()
        ));
    } else {
        display.print_note(format!(
            "{INDENT}• selfie package {context} --environment <env> <package> to {context} with a different environment"
        ));
        display.print_note(format!(
            "{INDENT}• selfie spec info <package> to see which environments this package supports"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_common::test_config_with_dir;

    // The environments that have the command are listed in order, whatever
    // order the spec's map gave them.
    #[test]
    fn a_missing_command_lists_the_others_in_order() {
        let display = DisplayManager::new(false);

        display_missing_command(
            &display,
            "check",
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
            "check",
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

        display_generic_environment_suggestion("bat", "test", &config, &display, "check");

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
            "install",
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

    #[test]
    fn test_format_field_key_and_value() {
        let key = format_field_key("Test Key", false);
        assert_eq!(key, "Test Key");

        let value = format_field_value("Test Value", false);
        assert_eq!(value, "Test Value");

        // Test with colors (just ensure no panic)
        let _colored_key = format_field_key("Test Key", true);
        let _colored_value = format_field_value("Test Value", true);
    }
}
