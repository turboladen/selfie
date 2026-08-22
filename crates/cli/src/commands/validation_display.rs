//! Shared validation table rendering for CLI commands.
//!
//! Used by both `spec validate` and `sync push` to display validation issues
//! in a consistent table format.

use comfy_table::{ContentArrangement, Table, presets};
use console::style;

use crate::display_manager::DisplayManager;

/// A single validation issue row, used as a common representation for both
/// event-driven validation results and sync push validation failures.
pub(crate) struct ValidationRow<'a> {
    pub level: &'a str,
    pub category: &'a str,
    pub field: &'a str,
    pub message: &'a str,
    pub location: Option<&'a str>,
}

/// A group of validation issues for a single file or package.
pub(crate) struct ValidationGroup<'a> {
    /// Display label for this group (file path or package name).
    pub label: &'a str,
    pub rows: Vec<ValidationRow<'a>>,
}

/// Display one or more validation groups as formatted tables.
pub(crate) fn display_validation_groups(
    groups: &[ValidationGroup<'_>],
    use_colors: bool,
    display: &DisplayManager,
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
    display.println("");
    display.print_section_header(header);

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
            display.println(format!("  {} {}", marked, style(group.label).bold()));
        } else {
            display.println(format!("  {marker} {}", group.label));
        }

        let mut table = create_validation_table();
        table.set_header(vec!["Level", "Category", "Field", "Message", "Location"]);

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

            table.add_row(vec![
                level,
                category,
                field,
                row.message.to_string(),
                location.to_string(),
            ]);
        }

        display.println(format!("{table}"));
    }
}

fn create_validation_table() -> Table {
    let mut table = Table::new();
    table
        .load_style(presets::UTF8_FULL_CONDENSED.with_rounded_corners())
        .set_content_arrangement(ContentArrangement::Dynamic);
    table
}
