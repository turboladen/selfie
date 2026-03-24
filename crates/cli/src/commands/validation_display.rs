//! Shared validation table rendering for CLI commands.
//!
//! Used by both `spec validate` and `sync push` to display validation issues
//! in a consistent table format.

use comfy_table::{ContentArrangement, Table, modifiers, presets};
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

    let total = groups.iter().map(|g| g.rows.len()).sum::<usize>();
    if total == 0 {
        return;
    }

    let header = match (total_errors, total_warnings) {
        (e, w) if e > 0 && w > 0 => {
            format!("Validation Issues ({e} error(s), {w} warning(s))")
        }
        (e, _) if e > 0 => format!("Validation Errors ({e})"),
        (_, w) => format!("Validation Warnings ({w})"),
    };
    display.println("");
    display.print_section_header(header);

    for group in groups {
        if group.rows.is_empty() {
            continue;
        }

        // File/package header
        if use_colors {
            display.println(format!(
                "  {} {}",
                style("✗").red(),
                style(group.label).bold()
            ));
        } else {
            display.println(format!("  ✗ {}", group.label));
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
        .load_preset(presets::UTF8_FULL_CONDENSED)
        .apply_modifier(modifiers::UTF8_ROUND_CORNERS)
        .set_content_arrangement(ContentArrangement::Dynamic);
    table
}
