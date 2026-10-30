//! Shared validation table rendering for CLI commands.
//!
//! Used by both `spec validate` and `sync push` to display validation issues
//! in a consistent table format.

use selfie::package::event::ValidationIssueData;
use selfie::validation::ValidationLevel;

use crate::display_manager::{Channel, DisplayManager};
use crate::formatters::comma_joined;
use crate::received::Issue;
use crate::tables::{Cell, SafeTable};
use crate::terminal_text::{Literal, Shown, lit, shown};

/// A group of validation issues for a single file or package.
pub(crate) struct ValidationGroup {
    /// Display label for this group (file path or package name).
    pub label: Shown,
    pub rows: Vec<Issue>,
}

/// Display one or more validation groups as formatted tables, on `channel`:
/// stdout when the issues are the answer, stderr when they explain a failure.
pub(crate) fn display_validation_groups(
    groups: Vec<ValidationGroup>,
    use_colors: bool,
    display: &DisplayManager,
    channel: Channel,
) {
    let total_errors: usize = groups
        .iter()
        .flat_map(|g| &g.rows)
        .filter(|r| r.level == ValidationLevel::Error)
        .count();
    let total_warnings: usize = groups
        .iter()
        .flat_map(|g| &g.rows)
        .filter(|r| r.level == ValidationLevel::Warning)
        .count();
    let total_notices: usize = groups
        .iter()
        .flat_map(|g| &g.rows)
        .filter(|r| r.level == ValidationLevel::Info)
        .count();

    let total = groups.iter().map(|g| g.rows.len()).sum::<usize>();
    if total == 0 {
        return;
    }

    // Built from whichever levels are actually present. A fixed
    // errors-or-warnings pair would render an informational-only group as
    // "Validation Warnings (0)".
    let counted = level_counts(total_errors, total_warnings, total_notices);
    let header = match (total_errors, total_warnings, total_notices) {
        (e, 0, 0) if e > 0 => shown!("Validation Errors ({})", e),
        (0, w, 0) if w > 0 => shown!("Validation Warnings ({})", w),
        (0, 0, n) if n > 0 => shown!("Validation Notices ({})", n),
        _ => shown!("Validation Issues ({})", comma_joined(counted)),
    };
    display.line_to(channel, lit!(""));
    display.print_section_header_to(channel, header);

    for group in groups {
        if group.rows.is_empty() {
            continue;
        }

        // File/package header. A group holding only informational notices is not
        // a failure, so it must not be marked with a red cross.
        let blocking = group.rows.iter().any(|r| r.level != ValidationLevel::Info);
        let marker = if blocking {
            Shown::from(lit!("✗")).paint(use_colors, |s| s.red())
        } else {
            Shown::from(lit!("ℹ")).paint(use_colors, |s| s.blue())
        };
        let label = group.label.paint(use_colors, |s| s.bold());
        display.line_to(channel, shown!("  {} {}", marker, label));

        let mut table = SafeTable::new();
        // Sized to the terminal of the stream it prints on.
        if channel == Channel::Stderr {
            table.use_stderr();
        }
        // A Suggestion column only where some issue has one, so a table without
        // any keeps its width.
        let suggested = group.rows.iter().any(|r| r.suggestion.is_some());
        let mut header = vec![
            lit!("Level"),
            lit!("Category"),
            lit!("Field"),
            lit!("Message"),
            lit!("Location"),
        ];
        if suggested {
            header.push(lit!("Suggestion"));
        }
        table.set_header(header);

        for row in group.rows {
            let level = Cell::painted(
                level_label(&row.level),
                use_colors,
                match row.level {
                    ValidationLevel::Error => |s| s.red().bold(),
                    ValidationLevel::Warning => |s| s.yellow().bold(),
                    ValidationLevel::Info => |s| s,
                },
            );
            let mut cells = vec![
                level,
                Cell::painted(row.category, use_colors, |s| s.magenta()),
                Cell::painted(row.field, use_colors, |s| s.cyan()),
                row.message.into(),
                row.location
                    .map_or_else(|| Cell::from(lit!("-")), Cell::from),
            ];
            if suggested {
                cells.push(
                    row.suggestion
                        .map_or_else(|| Cell::from(lit!("-")), Cell::from),
                );
            }
            table.add_row(cells);
        }

        display.line_to(channel, shown!("{}", table));
    }
}

/// The count at each level present, as "2 error(s)", "1 warning(s)" and
/// "3 notice(s)", in that order.
pub(crate) fn level_counts(errors: usize, warnings: usize, notices: usize) -> Vec<Shown> {
    [
        (errors, lit!("error(s)")),
        (warnings, lit!("warning(s)")),
        (notices, lit!("notice(s)")),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, noun)| shown!("{} {}", n, noun))
    .collect()
}

/// The word a validation table and an issue line both use for `level`.
pub(crate) fn level_label(level: &ValidationLevel) -> Literal {
    match level {
        ValidationLevel::Error => lit!("ERROR"),
        ValidationLevel::Warning => lit!("WARN"),
        ValidationLevel::Info => lit!("INFO"),
    }
}

/// One issue as a line: `<LEVEL> <field>: <message>`, then `. <suggestion>` when
/// there is one.
pub(crate) fn issue_line(issue: &ValidationIssueData) -> Shown {
    let level = level_label(&issue.level);
    let issue = Issue::from(issue);
    match issue.suggestion {
        Some(suggestion) => shown!(
            "{} {}: {}. {}",
            level,
            issue.field,
            issue.message,
            suggestion
        ),
        None => shown!("{} {}: {}", level, issue.field, issue.message),
    }
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

    fn groups() -> Vec<ValidationGroup> {
        vec![ValidationGroup {
            label: lit!("noenv").into(),
            rows: vec![Issue {
                level: ValidationLevel::Error,
                category: lit!("required field").into(),
                field: lit!("environments").into(),
                message: lit!("At least one environment must be defined").into(),
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
            display_validation_groups(groups(), false, &display, channel);

            let printed = display.printed();
            assert!(printed.len() > 2, "{printed:?}");
            assert!(printed.iter().all(|(s, _)| *s == stream), "{printed:?}");
        }
    }

    // A location renders as "line N, column M" in the table, and an issue with
    // none as "-".
    #[test]
    fn a_location_reads_line_and_column() {
        let mut located = groups();
        located[0].rows[0].location =
            crate::received::location(selfie::yaml::SourceLocation::new(2, 1));
        let display = DisplayManager::new(false);

        display_validation_groups(located, false, &display, Channel::Stdout);

        let printed: String = display
            .printed()
            .into_iter()
            .map(|(_, l)| l + "\n")
            .collect();
        assert!(printed.contains("line 2, column 1"), "{printed}");
    }

    fn issue(level: ValidationLevel, suggestion: Option<&str>) -> ValidationIssueData {
        ValidationIssueData {
            category: selfie::validation::ValidationErrorCategory::CommandSyntax,
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
            ))
            .as_str(),
            "ERROR environments.work.install: Unmatched double quote in command. Add a closing \
             double quote (\") to the command."
        );
    }

    #[test]
    fn issue_line_without_a_suggestion_ends_at_the_message() {
        assert_eq!(
            issue_line(&issue(ValidationLevel::Warning, None)).as_str(),
            "WARN environments.work.install: Unmatched double quote in command"
        );
    }

    // A library sentence ending in a line break, CRLF included, ends at its
    // text: no `\r` and no blank line after it.
    #[test]
    fn an_issue_ending_in_a_line_break_ends_at_its_text() {
        let display = DisplayManager::new(false);
        let mut ending = issue(ValidationLevel::Error, Some("fix it\r\n"));
        ending.message = "broken\r\n".to_string();

        print_issues(&display, &[ending]);

        assert_eq!(
            display.printed(),
            [(
                Channel::Stderr,
                "ERROR environments.work.install: broken. fix it".to_string()
            )]
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
        assert_eq!(level_label(&ValidationLevel::Error).as_str(), "ERROR");
        assert_eq!(level_label(&ValidationLevel::Warning).as_str(), "WARN");
        assert_eq!(level_label(&ValidationLevel::Info).as_str(), "INFO");
    }
}
