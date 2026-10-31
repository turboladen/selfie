use std::fmt;

use comfy_table::{ContentArrangement, Table, presets::UTF8_FULL_CONDENSED};

use crate::terminal_text::{
    Command, Content, DisplayPath, Literal, Name, Output, Paint, Prose, Shown, lit, shown,
};
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
    //
    // A tab is expanded here, because comfy-table counts it as one column where
    // the terminal moves to the next tab stop, which pushes that row's right
    // border out of line.
    fn laid_out(self) -> String {
        self.0.hang(2).tabs_expanded().into_string()
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
cells_from!(
    Shown,
    Literal,
    Name,
    DisplayPath,
    Prose,
    Content,
    Output,
    Command
);

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

    /// Add a row holding `command` under the field name `key`, and, when the
    /// command reads as another, a row under it holding its escaped note, so
    /// the note's lines line up with the command's. Each key is styled as
    /// [`format_key`](crate::formatters::format_key) styles it.
    pub(crate) fn add_command_row(
        &mut self,
        key: Literal,
        command: Command,
        use_colors: bool,
    ) -> &mut Self {
        let key_cell = |key| Cell::from(crate::formatters::format_key(key, use_colors));
        let note = command.note();
        self.add_row([key_cell(key), command.into()]);
        if let Some(note) = note {
            self.add_row([key_cell(crate::terminal_text::ESCAPED), note.into()]);
        }
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
}

impl ValidationTableReporter {
    /// A table of validation issues, each category colored; the printer
    /// removes the color where the stream has none.
    pub(crate) fn new() -> Self {
        Self {
            table: SafeTable::new(),
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
        let issue = crate::received::Issue::from(issue);
        self.table.add_row([
            Cell::from(issue.category.painted(color)),
            issue.field.into(),
            issue.message.into(),
            issue
                .suggestion
                .map_or_else(|| Cell::from(lit!("")), Cell::from),
        ]);
    }

    /// Print the table on stdout: a validator's issues are its answer.
    pub(crate) fn print(&self, display: &crate::display_manager::DisplayManager) {
        display.println(shown!("{}", self.table));
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

    // A tab moves to the next tab stop on a terminal, and comfy-table counts it
    // as one column, so a cell holding one would push its row's border out of
    // line. Every line of the table is one width, wherever in its cell the tab
    // sits, a continued line's included, and no tab reaches the terminal.
    #[test]
    fn a_tab_in_a_cell_keeps_every_row_the_same_width() {
        let mut table = SafeTable::new();
        table.set_header([lit!("key"), lit!("value")]);
        for text in [
            "\tat0",
            "abc\tat3",
            "abcdefg\tat7",
            "first\n\tafter-hang",
            "x\t\ty",
        ] {
            table.add_row([Cell::from(lit!("k")), Content::new(text).into()]);
        }

        let rendered = table.to_string();

        assert!(!rendered.contains('\t'), "{rendered}");
        let widths: Vec<usize> = rendered.lines().map(console::measure_text_width).collect();
        assert!(
            widths.windows(2).all(|pair| pair[0] == pair[1]),
            "{widths:?}\n{rendered}"
        );
        let (_, at3) = find(&rendered, "at3");
        let (_, abc) = find(&rendered, "abc\u{20}");
        assert_eq!(at3 - abc, 8, "{rendered}");
    }
}
