use std::fmt;

use comfy_table::{ContentArrangement, Table, presets::UTF8_FULL_CONDENSED};

use crate::terminal_text::{Content, DisplayPath, Literal, Name, Output, Paint, Prose, Shown};
use selfie::validation::ValidationIssue;

/// A table in the CLI's style, rounded and fitted to the terminal, whose cells
/// show text from outside selfie escaped.
///
/// Each cell is built from a kind or a [`Shown`], which keeps its text and
/// styling. Every multi-line cell has its lines after the first indented two
/// columns inside the cell.
// The indent is what tells a continued line from the next row, since the
// table draws no line between rows.
pub(crate) struct SafeTable(Table);

/// Holds one cell of a [`SafeTable`].
pub(crate) struct Cell(Shown);

impl Cell {
    /// Builds a cell from `text`, styled by `paint` when `use_colors`.
    pub(crate) fn painted(text: impl Into<Shown>, use_colors: bool, paint: Paint) -> Self {
        Self(text.into().paint(use_colors, paint))
    }

    // The cell's text as the table lays it out, its lines after the first two
    // columns in. Only the text's own line breaks get the indent: a line that
    // comfy-table wraps to fit the terminal starts at the cell's edge, and so
    // does a continued line wider than its cell, whose leading spaces the wrap
    // drops (selfie-bh34).
    fn laid_out(self) -> String {
        self.0.hang(2).into_string()
    }
}

// A cell shows a kind as it is, styling included.
macro_rules! cells_from {
    ($($kind:ty),*) => {$(
        impl From<$kind> for Cell {
            fn from(text: $kind) -> Self {
                Self(Shown::from(text))
            }
        }
    )*};
}
cells_from!(Shown, Literal, Name, DisplayPath, Prose, Content, Output);

impl SafeTable {
    /// Returns an empty table.
    pub(crate) fn new() -> Self {
        let mut table = Table::new();
        table
            .load_style(UTF8_FULL_CONDENSED.with_rounded_corners())
            .set_content_arrangement(ContentArrangement::Dynamic);
        Self(table)
    }

    /// Fit the table to the terminal stderr is on, for a table printed there.
    pub(crate) fn use_stderr(&mut self) -> &mut Self {
        self.0.use_stderr();
        self
    }

    /// Set the header row.
    pub(crate) fn set_header<C: Into<Cell>>(
        &mut self,
        cells: impl IntoIterator<Item = C>,
    ) -> &mut Self {
        self.0
            .set_header(cells.into_iter().map(|cell| cell.into().laid_out()));
        self
    }

    /// Add a row.
    pub(crate) fn add_row<C: Into<Cell>>(
        &mut self,
        cells: impl IntoIterator<Item = C>,
    ) -> &mut Self {
        self.0
            .add_row(cells.into_iter().map(|cell| cell.into().laid_out()));
        self
    }
}

impl fmt::Display for SafeTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

pub(crate) struct ValidationTableReporter {
    table: SafeTable,
    use_colors: bool,
}

impl ValidationTableReporter {
    pub(crate) fn new(use_colors: bool) -> Self {
        Self {
            table: SafeTable::new(),
            use_colors,
        }
    }

    pub(crate) fn setup(&mut self, header: [Literal; 4]) -> &mut Self {
        self.table.set_header(header);
        self
    }

    pub(crate) fn add_validation_errors(&mut self, error_issues: &[&ValidationIssue]) -> &mut Self {
        for error in error_issues {
            self.add_issue(error, |s| s.red().bold());
        }
        self
    }

    pub(crate) fn add_validation_warnings(
        &mut self,
        warning_issues: &[&ValidationIssue],
    ) -> &mut Self {
        for warning in warning_issues {
            self.add_issue(warning, |s| s.yellow().bold());
        }
        self
    }

    // One issue's row, its category painted with `color`.
    fn add_issue(&mut self, issue: &ValidationIssue, color: Paint) {
        self.table.add_row([
            Cell::painted(
                Name::new(&issue.category().to_string()),
                self.use_colors,
                color,
            ),
            Name::new(issue.field()).into(),
            Prose::new(issue.message()).into(),
            Prose::new(issue.suggestion().map_or("", String::as_str)).into(),
        ]);
    }

    /// Print the table on stdout: a validator's issues are its answer.
    pub(crate) fn print(&self, display: &crate::display_manager::DisplayManager) {
        display.println(crate::terminal_text::shown!("{}", self.table));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal_text::{lit, shown};

    // The terminal column at which `needle` starts in `table`, and the line it
    // is on.
    fn find(table: &str, needle: &str) -> (usize, usize) {
        table
            .lines()
            .enumerate()
            .find_map(|(number, line)| {
                line.find(needle)
                    .map(|at| (number, console::measure_text_width(&line[..at])))
            })
            .unwrap_or_else(|| panic!("{needle:?} not in\n{table}"))
    }

    // Every cell's lines after its first start two columns inside it, once,
    // whatever the cell was built from, the header's included; and no line
    // separates two rows.
    #[test]
    fn a_multi_line_cell_continues_two_columns_inside_it() {
        let mut table = SafeTable::new();
        table.set_header([Cell::from(Prose::new("h1\nh2")), Name::new("k").into()]);
        table.add_row([
            Cell::from(Prose::new("a1\na2")),
            crate::status_style::styled_text(Prose::new("b1\nb2"), true, |s| s.red()).into(),
        ]);
        table.add_row([
            Cell::from(shown!("{}\n{}", lit!("c1"), lit!("c2"))),
            Name::new("d1").into(),
        ]);

        let rendered = console::strip_ansi_codes(&table.to_string()).into_owned();

        for (first, second) in [("h1", "h2"), ("a1", "a2"), ("b1", "b2"), ("c1", "c2")] {
            let (first_line, first_column) = find(&rendered, first);
            let (second_line, second_column) = find(&rendered, second);
            assert_eq!(second_line, first_line + 1, "{rendered}");
            assert_eq!(second_column, first_column + 2, "{first}:\n{rendered}");
        }
        // The top border, two header lines, the header's separator, two lines
        // for each row and the bottom border.
        assert_eq!(
            rendered.lines().count(),
            1 + 2 + 1 + 2 + 2 + 1,
            "{rendered}"
        );
    }
}
