use std::fmt;

use comfy_table::{ContentArrangement, Table, presets::UTF8_FULL_CONDENSED};

use crate::terminal_text::{Paint, Shown};
use selfie::validation::ValidationIssue;

/// A table in the CLI's style, rounded and fitted to the terminal, whose cells
/// show text from outside selfie escaped.
///
/// A cell built from text, `"x".into()`, is a name: escaped strictly, on one
/// line. [`Cell::prose`] keeps a line break inside the cell, and a [`Shown`]
/// becomes a cell as it is, styling included.
pub(crate) struct SafeTable(Table);

/// Holds one cell of a [`SafeTable`].
pub(crate) struct Cell(Shown);

impl Cell {
    /// Builds a cell from `text`, prose, escaped as [`Shown::prose`] escapes
    /// it, so a line break stays inside the cell.
    pub(crate) fn prose(text: &str) -> Self {
        Self(Shown::prose(text))
    }

    /// Builds a cell from `text`, a name, escaped and then styled by `paint`
    /// when `use_colors`.
    pub(crate) fn painted(text: &str, use_colors: bool, paint: Paint) -> Self {
        Self(Shown::name(text).paint(use_colors, paint))
    }
}

impl From<Shown> for Cell {
    /// Builds a cell that shows `text` as it is, styling included.
    fn from(text: Shown) -> Self {
        Self(text)
    }
}

impl From<&str> for Cell {
    /// Builds a cell from `text`, a name, escaped as [`Shown::name`] escapes it.
    fn from(text: &str) -> Self {
        Self(Shown::name(text))
    }
}

impl From<String> for Cell {
    /// Builds a cell from `text`, a name, escaped as [`Shown::name`] escapes it.
    fn from(text: String) -> Self {
        Self::from(text.as_str())
    }
}

impl From<&String> for Cell {
    /// Builds a cell from `text`, a name, escaped as [`Shown::name`] escapes it.
    fn from(text: &String) -> Self {
        Self::from(text.as_str())
    }
}

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
        self.0.set_header(
            cells
                .into_iter()
                .map(|cell| cell.into().0.as_str().to_string()),
        );
        self
    }

    /// Add a row.
    pub(crate) fn add_row<C: Into<Cell>>(
        &mut self,
        cells: impl IntoIterator<Item = C>,
    ) -> &mut Self {
        self.0.add_row(
            cells
                .into_iter()
                .map(|cell| cell.into().0.as_str().to_string()),
        );
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

    pub(crate) fn setup(&mut self, header: Vec<&'static str>) -> &mut Self {
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
            Cell::painted(&issue.category().to_string(), self.use_colors, color),
            issue.field().into(),
            Cell::prose(issue.message()),
            Cell::prose(issue.suggestion().map_or("", String::as_str)),
        ]);
    }

    /// Print the table on stdout: a validator's issues are its answer.
    pub(crate) fn print(&self, display: &crate::display_manager::DisplayManager) {
        display.println(crate::terminal_text::shown!("{}", self.table));
    }
}
