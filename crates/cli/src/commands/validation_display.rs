//! Shared validation table rendering for CLI commands.
//!
//! Used by both `spec validate` and `sync push` to display validation issues
//! in a consistent table format.

use comfy_table::{ContentArrangement, Table, presets};
use console::style;

use selfie::package::event::{ValidationIssueData, ValidationLevel};

use crate::display_manager::{Channel, DisplayManager};

/// A single validation issue row, used as a common representation for both
/// event-driven validation results and sync push validation failures.
pub(crate) struct ValidationRow<'a> {
    pub level: &'a str,
    pub category: &'a str,
    pub field: &'a str,
    pub message: &'a str,
    pub location: Option<&'a str>,
    /// How to fix it, when the issue says.
    pub suggestion: Option<&'a str>,
}

/// A group of validation issues for a single file or package.
pub(crate) struct ValidationGroup<'a> {
    /// Display label for this group (file path or package name).
    pub label: &'a str,
    pub rows: Vec<ValidationRow<'a>>,
}

/// Display one or more validation groups as formatted tables, on `channel`:
/// stdout when the issues are the answer, stderr when they explain a failure.
pub(crate) fn display_validation_groups(
    groups: &[ValidationGroup<'_>],
    use_colors: bool,
    display: &DisplayManager,
    channel: Channel,
) {
    let total_errors: usize = groups
        .iter()
        .flat_map(|g| &g.rows)
        .filter(|r| r.level == "ERROR")
        .count();
    let total_warnings: usize = groups
        .iter()
        .flat_map(|g| &g.rows)
        .filter(|r| r.level == "WARN")
        .count();
    let total_notices: usize = groups
        .iter()
        .flat_map(|g| &g.rows)
        .filter(|r| r.level == "INFO")
        .count();

    let total = groups.iter().map(|g| g.rows.len()).sum::<usize>();
    if total == 0 {
        return;
    }

    // Built from whichever levels are actually present. A fixed
    // errors-or-warnings pair would render an informational-only group as
    // "Validation Warnings (0)".
    let mut counted = Vec::new();
    if total_errors > 0 {
        counted.push(format!("{total_errors} error(s)"));
    }
    if total_warnings > 0 {
        counted.push(format!("{total_warnings} warning(s)"));
    }
    if total_notices > 0 {
        counted.push(format!("{total_notices} notice(s)"));
    }
    let header = match (total_errors, total_warnings, total_notices) {
        (e, 0, 0) if e > 0 => format!("Validation Errors ({e})"),
        (0, w, 0) if w > 0 => format!("Validation Warnings ({w})"),
        (0, 0, n) if n > 0 => format!("Validation Notices ({n})"),
        _ => format!("Validation Issues ({})", counted.join(", ")),
    };
    display.line_to(channel, "");
    display.print_section_header_to(channel, header);

    for group in groups {
        if group.rows.is_empty() {
            continue;
        }

        // File/package header. A group holding only informational notices is not
        // a failure, so it must not be marked with a red cross.
        let blocking = group.rows.iter().any(|r| r.level != "INFO");
        let (marker, marked) = if blocking {
            ("✗", style("✗").red())
        } else {
            ("ℹ", style("ℹ").blue())
        };
        if use_colors {
            display.line_to(
                channel,
                format!("  {} {}", marked, style(group.label).bold()),
            );
        } else {
            display.line_to(channel, format!("  {marker} {}", group.label));
        }

        let mut table = create_validation_table();
        // Sized to the terminal of the stream it prints on.
        if channel == Channel::Stderr {
            table.use_stderr();
        }
        // A Suggestion column only where some issue has one, so a table without
        // any keeps its width.
        let suggested = group.rows.iter().any(|r| r.suggestion.is_some());
        let mut header = vec!["Level", "Category", "Field", "Message", "Location"];
        if suggested {
            header.push("Suggestion");
        }
        table.set_header(header);

        for row in &group.rows {
            let level = if use_colors {
                match row.level {
                    "ERROR" => style(row.level).red().bold().to_string(),
                    "WARN" => style(row.level).yellow().bold().to_string(),
                    _ => row.level.to_string(),
                }
            } else {
                row.level.to_string()
            };

            let category = if use_colors {
                style(row.category).magenta().to_string()
            } else {
                row.category.to_string()
            };

            let field = if use_colors {
                style(row.field).cyan().to_string()
            } else {
                row.field.to_string()
            };

            let location = row.location.unwrap_or("-");

            let mut cells = vec![
                level,
                category,
                field,
                row.message.to_string(),
                location.to_string(),
            ];
            if suggested {
                cells.push(row.suggestion.unwrap_or("-").to_string());
            }
            table.add_row(cells);
        }

        display.line_to(channel, format!("{table}"));
    }
}

fn create_validation_table() -> Table {
    let mut table = Table::new();
    table
        .load_style(presets::UTF8_FULL_CONDENSED.with_rounded_corners())
        .set_content_arrangement(ContentArrangement::Dynamic);
    table
}

/// The word a validation table and an issue line both use for `level`.
pub(crate) fn level_label(level: &ValidationLevel) -> &'static str {
    match level {
        ValidationLevel::Error => "ERROR",
        ValidationLevel::Warning => "WARN",
        ValidationLevel::Info => "INFO",
    }
}

/// One issue as a line: `<LEVEL> <field>: <message>`, then `. <suggestion>` when
/// there is one.
pub(crate) fn issue_line(issue: &ValidationIssueData) -> String {
    let mut line = format!(
        "{} {}: {}",
        level_label(&issue.level),
        issue.field,
        issue.message
    );
    if let Some(suggestion) = &issue.suggestion {
        line.push_str(". ");
        line.push_str(suggestion);
    }
    line
}

/// Print each of `issues` on stderr, one line apiece, through the display method
/// for its level.
pub(crate) fn print_issues(display: &DisplayManager, issues: &[ValidationIssueData]) {
    for issue in issues {
        let line = issue_line(issue);
        match issue.level {
            ValidationLevel::Error => display.print_error(line),
            ValidationLevel::Warning => display.print_warning(line),
            ValidationLevel::Info => display.print_run_note(line),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display_manager::Channel;

    fn groups() -> Vec<ValidationGroup<'static>> {
        vec![ValidationGroup {
            label: "noenv",
            rows: vec![ValidationRow {
                level: "ERROR",
                category: "RequiredField",
                field: "environments",
                message: "At least one environment must be defined",
                location: None,
                suggestion: None,
            }],
        }]
    }

    // Every line of the tables goes to the channel asked for: stdout where the
    // issues are the answer, stderr where they explain a failure.
    #[test]
    fn every_line_goes_to_the_channel_asked_for() {
        for (channel, stream) in [
            (Channel::Stdout, Channel::Stdout),
            (Channel::Stderr, Channel::Stderr),
        ] {
            let display = DisplayManager::new(false);
            display_validation_groups(&groups(), false, &display, channel);

            let printed = display.printed();
            assert!(printed.len() > 2, "{printed:?}");
            assert!(printed.iter().all(|(s, _)| *s == stream), "{printed:?}");
        }
    }

    fn issue(level: ValidationLevel, suggestion: Option<&str>) -> ValidationIssueData {
        ValidationIssueData {
            category: "CommandSyntax".to_string(),
            field: "environments.work.install".to_string(),
            message: "Unmatched double quote in command".to_string(),
            level,
            suggestion: suggestion.map(str::to_string),
            location: None,
        }
    }

    #[test]
    fn issue_line_reads_level_field_message_and_suggestion() {
        assert_eq!(
            issue_line(&issue(
                ValidationLevel::Error,
                Some("Add a closing double quote (\") to the command.")
            )),
            "ERROR environments.work.install: Unmatched double quote in command. Add a closing \
             double quote (\") to the command."
        );
    }

    #[test]
    fn issue_line_without_a_suggestion_ends_at_the_message() {
        assert_eq!(
            issue_line(&issue(ValidationLevel::Warning, None)),
            "WARN environments.work.install: Unmatched double quote in command"
        );
    }

    // The issues are commentary on a run whose answer is elsewhere, so every line
    // goes to stderr, whatever its level.
    #[test]
    fn print_issues_writes_every_level_to_stderr_only() {
        use crate::display_manager::Channel;

        let display = DisplayManager::new(false);
        print_issues(
            &display,
            &[
                issue(ValidationLevel::Error, None),
                issue(ValidationLevel::Warning, None),
                issue(ValidationLevel::Info, None),
            ],
        );

        let line = |level: &str| {
            (
                Channel::Stderr,
                format!("{level} environments.work.install: Unmatched double quote in command"),
            )
        };
        assert_eq!(
            display.printed(),
            vec![line("ERROR"), line("WARN"), line("INFO")]
        );
    }

    #[test]
    fn level_label_names_each_level() {
        assert_eq!(level_label(&ValidationLevel::Error), "ERROR");
        assert_eq!(level_label(&ValidationLevel::Warning), "WARN");
        assert_eq!(level_label(&ValidationLevel::Info), "INFO");
    }
}
