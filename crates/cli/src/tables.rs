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
/// columns inside the cell, except a [`Cell::list`], each of whose entries
/// starts at the cell's left edge.
// The indent is what tells a continued line from the next row, since the
// table draws no line between rows.
pub(crate) struct SafeTable {
    // The table's style and the terminal it fits, holding no rows: each render
    // builds its own from the cells below.
    style: Table,
    header: Option<Vec<String>>,
    rows: Vec<Vec<String>>,
}

// The indent of every line after a cell's first.
const HANG: &str = "  ";

/// Holds one cell of a [`SafeTable`].
// The text is held with its later lines already indented, so a list can
// indent each entry's lines and leave the entries themselves at the edge.
pub(crate) struct Cell(Shown);

impl Cell {
    /// Builds a cell from `text`, styled by `paint` when `use_colors`.
    pub(crate) fn painted(text: impl Into<Shown>, use_colors: bool, paint: Paint) -> Self {
        Self(hung(text.into().paint(use_colors, paint)))
    }

    /// Builds a cell listing `entries` one to a line, each starting at the
    /// cell's left edge and its own later lines two columns inside it.
    pub(crate) fn list(entries: impl IntoIterator<Item = impl Into<Shown>>) -> Self {
        Self(Shown::join(
            entries.into_iter().map(|entry| hung(entry.into())),
            lit!("\n"),
        ))
    }

    // The cell's text as the table lays it out. A line too wide for its column
    // is wrapped when the table is rendered, which indents its pieces.
    //
    // A tab is expanded here, because comfy-table counts it as one column where
    // the terminal moves to the next tab stop, which pushes that row's right
    // border out of line.
    fn laid_out(self) -> String {
        self.0.tabs_expanded().into_string()
    }
}

// Returns `text` with its lines after the first indented two columns.
fn hung(text: Shown) -> Shown {
    text.hang(HANG.len())
}

// A cell shows a kind as it is, styling included.
macro_rules! cells_from {
    ($($kind:ty),*) => {$(
        impl From<$kind> for Cell {
            fn from(text: $kind) -> Self {
                Self(hung(Shown::from(text)))
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
        let mut style = Table::new();
        style
            // `column_widths` reads this style's top border, so a change of
            // style changes it too.
            .load_style(UTF8_FULL_CONDENSED.with_rounded_corners())
            .set_content_arrangement(ContentArrangement::Dynamic);
        Self {
            style,
            header: None,
            rows: Vec::new(),
        }
    }

    // A table fitted to `width` columns rather than to a terminal.
    #[cfg(test)]
    fn with_width(mut self, width: u16) -> Self {
        self.style.set_width(width);
        self
    }

    /// Fit the table to the terminal stderr is on, for a table printed there.
    pub(crate) fn use_stderr(&mut self) -> &mut Self {
        self.style.use_stderr();
        self
    }

    /// Set the header row.
    pub(crate) fn set_header<C: Into<Cell>>(
        &mut self,
        cells: impl IntoIterator<Item = C>,
    ) -> &mut Self {
        self.header = Some(
            cells
                .into_iter()
                .map(|cell| cell.into().laid_out())
                .collect(),
        );
        self
    }

    /// Add a row.
    pub(crate) fn add_row<C: Into<Cell>>(
        &mut self,
        cells: impl IntoIterator<Item = C>,
    ) -> &mut Self {
        self.rows.push(
            cells
                .into_iter()
                .map(|cell| cell.into().laid_out())
                .collect(),
        );
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

impl SafeTable {
    // The table holding every cell as `cell` makes it from its laid-out text and
    // its column's index.
    fn built(&self, cell: impl Fn(&str, usize) -> String) -> Table {
        let row = |cells: &[String]| -> Vec<String> {
            cells
                .iter()
                .enumerate()
                .map(|(column, text)| cell(text, column))
                .collect()
        };
        let mut table = self.style.clone();
        if let Some(header) = &self.header {
            table.set_header(row(header));
        }
        table.add_rows(self.rows.iter().map(|cells| row(cells)));
        table
    }
}

impl fmt::Display for SafeTable {
    // comfy-table wraps a line too wide for its column at the cell's edge, and
    // splits on spaces, so a continued line's indent is dropped with it. The
    // table is laid out once to learn each column's width, and when a line is
    // too wide for its column, every such line is wrapped here with the indent
    // and the table laid out again with comfy-table's wrapping off. A table
    // whose lines all fit is the first layout, unchanged.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let first = self.built(|text, _| text.to_string()).to_string();
        let Some(widths) = column_widths(&first) else {
            return f.write_str(&first);
        };
        // A cell in a column the border does not show is left as it is.
        let fits = |text: &str, column: usize| {
            widths.get(column).is_none_or(|&width| {
                text.lines()
                    .all(|line| console::measure_text_width(line) <= width)
            })
        };
        if self.header.iter().chain(&self.rows).all(|cells| {
            cells
                .iter()
                .enumerate()
                .all(|(column, text)| fits(text, column))
        }) {
            return f.write_str(&first);
        }
        let mut wrapped = self.built(|text, column| match widths.get(column) {
            Some(&width) => wrapped_cell(text, width),
            None => text.to_string(),
        });
        wrapped.set_content_arrangement(ContentArrangement::Disabled);
        wrapped.fmt(f)
    }
}

// Each column's content width, read from the top border of a table rendered
// in the rounded style with comfy-table's default padding of one column on
// each side. `None` when the border does not read that way.
fn column_widths(rendered: &str) -> Option<Vec<usize>> {
    let top = rendered.lines().next()?;
    let inner = top.strip_prefix('╭')?.strip_suffix('╮')?;
    inner
        .split('┬')
        .map(|segment| {
            let width = segment.chars().count();
            (segment.chars().all(|c| c == '─') && width >= 2).then(|| width - 2)
        })
        .collect()
}

// `cell` with each line wider than `width` wrapped, every piece after a line's
// first indented by `HANG`, each piece keeping the styling it had.
fn wrapped_cell(cell: &str, width: usize) -> String {
    use ansi_str::AnsiStr;

    // A style opened on one line runs on into the next, so each line is split
    // out carrying the style it is shown in.
    cell.ansi_split("\n")
        .enumerate()
        .flat_map(|(index, line)| wrapped_line(&line, index > 0, width))
        .map(reset_closed)
        .collect::<Vec<_>>()
        .join("\n")
}

fn wrapped_line(line: &str, continued: bool, width: usize) -> Vec<String> {
    use ansi_str::AnsiStr;

    if console::measure_text_width(line) <= width {
        return vec![line.to_string()];
    }
    // A column no wider than the indent has no room for a piece after it, so
    // its pieces are not indented.
    let hang = if width > HANG.len() { HANG } else { "" };
    let plain = line.ansi_strip();
    // A continued line already starts with the indent, which its first piece
    // keeps.
    let indent = if continued && plain.starts_with(HANG) {
        hang
    } else {
        ""
    };
    // A word is broken at the cell's edge only, never at a hyphen of its own,
    // so a flag or a package name is not shown as two words.
    let options = textwrap::Options::new(width)
        .initial_indent(indent)
        .subsequent_indent(hang)
        .word_splitter(textwrap::WordSplitter::NoHyphenation)
        .break_words(true);
    // Each piece is found in the plain text after the last, so the styled text
    // can be cut at the same place.
    let mut cursor = indent.len();
    textwrap::wrap(&plain[indent.len()..], &options)
        .iter()
        .map(|piece| {
            // The indent textwrap put in front of the piece, and the words after it.
            let text = piece.trim_start_matches(' ');
            let prefix = &piece[..piece.len() - text.len()];
            let start = plain[cursor..].find(text).map_or(cursor, |at| cursor + at);
            let end = start + text.len();
            cursor = end;
            // Two cuts, each open on one side: ansi-str 0.9 measures a range's
            // end from its start when both fall inside one run of text, so
            // `start..end` there returns the wrong piece.
            let styled = line
                .ansi_get(start..)
                .and_then(|tail| tail.ansi_get(..end - start).map(|piece| piece.into_owned()))
                .unwrap_or_else(|| text.to_string());
            format!("{prefix}{styled}")
        })
        .collect()
}

// Returns `piece` with the styling sequences that end it replaced by one reset.
// ansi-str closes a line it splits or cuts with a sequence per style, such as
// `ESC[23m` for italic, and the printer's guard escapes those as text. Every
// style is closed at a piece's end, so a reset there leaves it shown the same.
fn reset_closed(piece: String) -> String {
    const RESET: &str = "\u{1b}[0m";
    let mut end = piece.len();
    while let Some(start) = piece[..end].rfind("\u{1b}[") {
        let sequence = &piece[start + 2..end];
        let Some(parameters) = sequence.strip_suffix('m') else {
            break;
        };
        if !parameters.chars().all(|c| c.is_ascii_digit() || c == ';') {
            break;
        }
        end = start;
    }
    if end == piece.len() || piece[end..] == *RESET {
        return piece;
    }
    format!("{}{RESET}", &piece[..end])
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

    // The second of two cells on a row line of a rendered table, padding
    // included, or `None` for a border line.
    fn value_cell(line: &str) -> Option<&str> {
        let cells: Vec<&str> = line.split(['│', '┆']).collect();
        (cells.len() == 4).then(|| cells[2])
    }

    // The text of each line of the value column in a table rendered without
    // styling.
    fn values(rendered: &str) -> Vec<String> {
        rendered
            .lines()
            .filter_map(value_cell)
            .map(|cell| {
                cell.strip_prefix(' ')
                    .unwrap_or(cell)
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    // Every entry of a list cell starts at the cell's left edge, and an entry's
    // own later lines and wrapped pieces start two columns inside it.
    #[test]
    fn a_list_cell_starts_each_entry_at_the_left_edge() {
        let mut table = SafeTable::new().with_width(40);
        table.add_row([
            Cell::from(lit!("k")),
            Cell::list([
                Prose::new("first: ok\nits note"),
                Prose::new("second: a status long enough to wrap in its column"),
            ]),
        ]);

        let rendered = table.to_string();
        let lines = values(&rendered);

        assert_eq!(lines[0], "first: ok", "{rendered}");
        assert_eq!(lines[1], "  its note", "{rendered}");
        assert!(lines[2].starts_with("second:"), "{rendered}");
        assert!(lines.len() > 3, "nothing wrapped:\n{rendered}");
        for line in &lines[3..] {
            assert!(line.starts_with(HANG), "{line:?}:\n{rendered}");
            assert!(
                !line[HANG.len()..].starts_with(' '),
                "{line:?}:\n{rendered}"
            );
        }
    }

    // A line too wide for its column is wrapped so that every piece after the
    // cell's first line starts two columns in, a continued line's pieces
    // included, and no word is lost or split between pieces.
    #[test]
    fn a_wrapped_line_keeps_the_continuation_indent() {
        let first = "brew install some-package --with-a-long-option";
        let continued = "and then a continued line that is much wider than its column";
        let mut table = SafeTable::new().with_width(40);
        table.add_row([
            Cell::from(lit!("install")),
            Prose::new(&format!("{first}\n{continued}")).into(),
        ]);

        let rendered = table.to_string();
        let lines = values(&rendered);

        assert!(lines.len() > 3, "nothing wrapped:\n{rendered}");
        assert!(!lines[0].starts_with(' '), "{rendered}");
        for line in &lines[1..] {
            assert!(
                line.starts_with(HANG),
                "{line:?} lost the indent:\n{rendered}"
            );
            assert!(
                !line[HANG.len()..].starts_with(' '),
                "{line:?}:\n{rendered}"
            );
        }
        let words: Vec<&str> = lines
            .iter()
            .flat_map(|line| line.split_whitespace())
            .collect();
        assert_eq!(
            words.join(" "),
            format!("{first} {continued}"),
            "{rendered}"
        );
        let widths: Vec<usize> = rendered.lines().map(console::measure_text_width).collect();
        assert!(
            widths
                .iter()
                .all(|&width| width == widths[0] && width <= 40),
            "{widths:?}\n{rendered}"
        );
    }

    // Each wrapped piece of a styled line keeps its style, so no piece reaches
    // the terminal unstyled and no style runs into the border.
    #[test]
    fn a_wrapped_styled_line_keeps_its_style_on_every_piece() {
        let text = "a styled line with enough words in it to need wrapping twice over";
        let mut table = SafeTable::new().with_width(30);
        table.add_row([
            Cell::from(lit!("k")),
            crate::status_style::styled_text(Prose::new(text), true, |s| s.red()).into(),
        ]);

        let rendered = table.to_string();
        let styled: Vec<&str> = rendered.lines().filter_map(value_cell).collect();

        assert!(styled.len() > 2, "nothing wrapped:\n{rendered}");
        for value in styled {
            assert!(value.contains("\u{1b}[31m"), "{value:?} lost its style");
            assert!(
                value.trim_end().ends_with("\u{1b}[0m"),
                "{value:?} runs its style on"
            );
        }
    }

    // Every line of a styled cell in a wrapped table closes its style with a
    // reset, the one closing sequence the printer's guard keeps, so no line
    // shows an escape as text: a wrapped piece, and a line of a multi-line
    // cell that fits. Bold, italic and underline each close with a sequence of
    // their own when a line is cut.
    #[test]
    fn a_wrapped_styled_piece_closes_with_what_the_guard_keeps() {
        let text = "a styled line with enough words in it to need wrapping twice over";
        let paints: [Paint; 3] = [
            |s| s.yellow().italic(),
            |s| s.bold().green(),
            |s| s.underlined().blue(),
        ];
        for paint in paints {
            let mut table = SafeTable::new().with_width(30);
            table.add_row([
                Cell::from(lit!("k")),
                crate::status_style::styled_text(Prose::new(text), true, paint).into(),
            ]);
            table.add_row([
                Cell::from(lit!("k")),
                crate::status_style::styled_text(Prose::new("fits\nthis too"), true, paint).into(),
            ]);

            let rendered = table.to_string();

            assert!(
                values(&console::strip_ansi_codes(&rendered)).len() > 4,
                "{rendered}"
            );
            assert_eq!(
                crate::terminal_text::guard(&rendered, true),
                rendered,
                "{rendered:?}"
            );
        }
    }

    // A style opened on a cell's first line covers its later lines too, so
    // every wrapped piece of a styled continued line is styled, whether or not
    // the first line wrapped.
    #[test]
    fn a_wrapped_continued_line_keeps_the_style_opened_before_it() {
        for first in ["short", "a first styled line that is long enough to wrap"] {
            let text = format!("{first}\na continued styled line with enough words to wrap");
            let mut table = SafeTable::new().with_width(30);
            table.add_row([
                Cell::from(lit!("k")),
                crate::status_style::styled_text(Prose::new(&text), true, |s| s.red()).into(),
            ]);

            let rendered = table.to_string();
            let styled: Vec<&str> = rendered.lines().filter_map(value_cell).collect();

            assert!(styled.len() > 3, "nothing wrapped:\n{rendered}");
            // Every word the line shows lies in the run its last opening of red
            // starts, before the next escape closes it.
            for value in styled {
                let red = value.rfind("\u{1b}[31m").unwrap_or_else(|| {
                    panic!("{value:?} lost its style");
                });
                let after = &value[red + "\u{1b}[31m".len()..];
                let run = &after[..after.find('\u{1b}').unwrap_or(after.len())];
                assert_eq!(
                    run.trim(),
                    console::strip_ansi_codes(value).trim(),
                    "{value:?} lost its style"
                );
            }
        }
    }

    // A column too narrow for the indent still wraps its lines, so the table
    // stays inside the width it was fitted to.
    #[test]
    fn a_column_narrower_than_the_indent_still_wraps() {
        let mut table = SafeTable::new().with_width(26);
        table.add_row(["a", "b", "c", "d", "e"].map(|letter| Prose::new(&letter.repeat(30))));

        let rendered = table.to_string();

        assert!(
            rendered
                .lines()
                .all(|line| console::measure_text_width(line) <= 26),
            "{rendered}"
        );
    }

    // A word is broken only at the cell's edge, not at its own hyphens, so a
    // flag that fits on the next line moves there whole.
    #[test]
    fn a_wrapped_line_keeps_a_hyphenated_word_whole() {
        let mut table = SafeTable::new().with_width(23);
        table.add_row([
            Cell::from(lit!("k")),
            Prose::new("brew --with-flag-x").into(),
        ]);

        let rendered = table.to_string();

        assert_eq!(values(&rendered), ["brew", "  --with-flag-x"], "{rendered}");
    }

    // A table whose lines all fit is the one comfy-table lays out, untouched.
    #[test]
    fn a_table_whose_lines_fit_is_laid_out_once() {
        let mut table = SafeTable::new().with_width(80);
        table.set_header([lit!("key"), lit!("value")]);
        table.add_row([Cell::from(lit!("k")), Content::new("short\nlines").into()]);

        assert_eq!(
            table.to_string(),
            table.built(|text, _| text.to_string()).to_string()
        );
    }

    // The widths come from a border of the rounded style and nothing else, and
    // a narrow column is read as it is.
    #[test]
    fn column_widths_read_only_a_rounded_top_border() {
        assert_eq!(column_widths("╭──────┬───╮\n│"), Some(vec![4, 1]));
        assert_eq!(column_widths("┌──────┬───┐"), None);
        assert_eq!(column_widths("╭──x───╮"), None);
        assert_eq!(column_widths(""), None);
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
