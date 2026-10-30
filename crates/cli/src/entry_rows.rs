//! The table `apply` and `dotfiles drift` print, one row per dotfile entry:
//! what happened to it, its source and target, and a detail. Each row prints as
//! its entry is processed, in columns fixed before the first.

use console::measure_text_width;
use selfie::package::event::SkipReason;

use crate::terminal_text::{DisplayPath, Literal, Name, Paint, Shown, Trusted, lit, shown};

/// The widest a source or target column grows, in terminal columns. A longer
/// value overflows its column on its own row.
pub(crate) const COLUMN_CAP: usize = 40;

/// What happened to an entry, which its row names first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryOutcome {
    /// The run wrote the entry's target.
    Deployed,
    /// The run wrote no content to the target, for the reason the row gives.
    /// A row may still say it tightened the target's permissions.
    Skipped,
    /// The target holds content selfie did not write, and the run did not
    /// overwrite it unasked. When the user accepts at the prompt, a `Deployed`
    /// row follows once the entry is written; if the target changed again
    /// while the prompt was open, a second `Conflict` row follows instead, and
    /// a write that is refused or fails is reported as a refusal or failure,
    /// with no row.
    Conflict,
    /// The target or its source has changed since selfie last wrote it.
    Drifted,
    /// The entry's content comes from commands, which a drift check does not
    /// run, so it was not compared.
    Unverified,
}

impl EntryOutcome {
    // Every outcome, for the tests; a new one joins it.
    #[cfg(test)]
    pub(crate) const ALL: [Self; 5] = [
        Self::Deployed,
        Self::Skipped,
        Self::Conflict,
        Self::Drifted,
        Self::Unverified,
    ];

    /// The word the row starts with.
    pub(crate) fn label(self) -> Literal {
        match self {
            Self::Deployed => lit!("deployed"),
            Self::Skipped => lit!("skipped"),
            Self::Conflict => lit!("conflict"),
            Self::Drifted => lit!("drifted"),
            Self::Unverified => lit!("unverified"),
        }
    }

    // How the label is colored, if it is.
    fn paint(self) -> Option<Paint> {
        match self {
            Self::Deployed => Some(|label| label.green()),
            Self::Conflict | Self::Drifted => Some(|label| label.yellow()),
            Self::Skipped | Self::Unverified => None,
        }
    }
}

impl From<&SkipReason> for EntryOutcome {
    /// The outcome a skip's row names: unverified for an entry a drift check
    /// could not compare, skipped for every other reason.
    fn from(reason: &SkipReason) -> Self {
        // No wildcard, so a new reason has to choose its outcome here.
        match reason {
            SkipReason::Unverifiable => Self::Unverified,
            SkipReason::UpToDate
            | SkipReason::InSync
            | SkipReason::PermissionsTightened
            | SkipReason::DryRun
            | SkipReason::SecretDryRun { .. } => Self::Skipped,
        }
    }
}

// Every label pads to this. A new outcome has to fit or widen it.
const LABEL_WIDTH: usize = 10;

/// How wide a table's source and target columns are, in terminal columns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct EntryColumns {
    pub(crate) source: usize,
    pub(crate) target: usize,
}

impl EntryColumns {
    /// The columns for rows naming `entries`, each a source and target as the
    /// rows will show them. A column is as wide as its widest value, or
    /// [`COLUMN_CAP`] columns when that is wider, counted as a terminal shows
    /// them, so a CJK character counts two.
    pub(crate) fn of<'a>(entries: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let mut columns = Self::default();
        for (source, target) in entries {
            columns.source = columns.source.max(capped(source));
            columns.target = columns.target.max(capped(target));
        }
        columns
    }
}

// The width `text` asks of its column: its own, or the cap when it is wider,
// in which case it overflows its own row.
fn capped(text: &str) -> usize {
    measure_text_width(text).min(COLUMN_CAP)
}

/// One entry's row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EntryRow {
    pub(crate) outcome: EntryOutcome,
    /// The source as the row shows it: a path, or a command.
    pub(crate) source: Shown,
    /// The target as the row shows it.
    pub(crate) target: DisplayPath,
    /// What the row says after the target, such as why it was skipped.
    pub(crate) detail: Option<Shown>,
}

/// `row`'s line laid out in `columns`, its outcome's label colored when
/// `use_colors`.
///
/// The label is padded by its own width, so its color takes no room in the
/// table. A value wider than its column is never cut; it pushes the rest of its
/// own row right.
pub(crate) fn entry_line(row: &EntryRow, columns: EntryColumns, use_colors: bool) -> Shown {
    let label = row.outcome.label();
    let after_label = spaces(LABEL_WIDTH.saturating_sub(label.as_str().len()) + 2);
    let label = match row.outcome.paint() {
        Some(paint) => Shown::from(label).paint(use_colors, paint),
        None => Shown::from(label),
    };
    let source = padded(&row.source, columns.source);
    match &row.detail {
        // A detail's lines after its first start under the source column, so
        // they read as part of this row and not as the next one.
        Some(detail) => shown!(
            "{}{}{} → {}  {}",
            label,
            after_label,
            source,
            padded(&row.target, columns.target),
            detail.clone().hang(measure_text_width(&under_row()))
        ),
        // Not padded: padding with nothing after it is trailing space.
        None => shown!("{}{}{} → {}", label, after_label, source, row.target),
    }
}

/// The indent of a line under a row, which starts it under the source.
pub(crate) fn under_row() -> Shown {
    spaces(LABEL_WIDTH + 2)
}

// `text` padded with spaces to `width` terminal columns.
fn padded(text: &(impl Trusted + ?Sized), width: usize) -> Shown {
    let fill = width.saturating_sub(measure_text_width(&text.to_string()));
    shown!("{}{}", text, spaces(fill))
}

fn spaces(count: usize) -> Shown {
    Shown::from(Name::new(&" ".repeat(count)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display_manager::column;

    fn row(outcome: EntryOutcome, source: &str, target: &str, detail: Option<&str>) -> EntryRow {
        EntryRow {
            outcome,
            source: DisplayPath::new(source).into(),
            target: DisplayPath::new(target),
            detail: detail.map(|detail| Shown::from(Name::new(detail))),
        }
    }

    fn line(row: &EntryRow, columns: EntryColumns) -> String {
        entry_line(row, columns, false).to_string()
    }

    fn columns_for(rows: &[EntryRow]) -> EntryColumns {
        EntryColumns::of(
            rows.iter()
                .map(|row| (row.source.as_str(), row.target.as_str())),
        )
    }

    fn all_equal(columns: &[usize]) -> bool {
        columns.windows(2).all(|pair| pair[0] == pair[1])
    }

    // Each source differs from the next in display width, bytes and characters
    // alike: `日本/conf` is 9 columns, 13 bytes and 7 characters, so padding by
    // either of the other two measures leaves its arrow out of line.
    #[test]
    fn columns_line_up_by_display_width() {
        let rows = [
            row(
                EntryOutcome::Skipped,
                "bat/config",
                "~/.config/bat/config",
                Some("dry run"),
            ),
            row(EntryOutcome::Skipped, "日本/conf", "~/.j", Some("dry run")),
            row(
                EntryOutcome::Skipped,
                "git/ignore",
                "~/.config/git/a-longer-one",
                Some("dry run"),
            ),
        ];
        let columns = columns_for(&rows);

        let lines: Vec<String> = rows.iter().map(|r| line(r, columns)).collect();

        let arrows: Vec<usize> = lines.iter().map(|l| column(l, " → ")).collect();
        let details: Vec<usize> = lines.iter().map(|l| column(l, "dry run")).collect();
        assert!(all_equal(&arrows), "{lines:#?}");
        assert!(all_equal(&details), "{lines:#?}");
        assert_eq!(
            lines[2],
            "skipped     git/ignore → ~/.config/git/a-longer-one  dry run"
        );

        // The widest value decides the width only by its terminal columns:
        // `日本語/設定` is 11 columns, 6 characters and 16 bytes, beside a plain
        // 10-character name.
        assert_eq!(
            EntryColumns::of([("日本語/設定", "~/.a"), ("abcdefghij", "~/.b")]).source,
            11
        );
    }

    // One source past the cap overflows on its own row, whole, and the others
    // pad only to the widest source within the cap. When every value in a
    // column is past the cap, the column has no width and each row reads
    // single-spaced.
    #[test]
    fn a_value_over_the_cap_overflows_alone() {
        let long = format!("deep/{}", "x".repeat(55));
        let rows = [
            row(EntryOutcome::Skipped, "a/b", "~/.ab", Some("dry run")),
            row(EntryOutcome::Skipped, &long, "~/.long", Some("dry run")),
            row(EntryOutcome::Skipped, "abc/def", "~/.cd", Some("dry run")),
        ];
        let columns = columns_for(&rows);

        assert_eq!(columns.source, COLUMN_CAP);
        assert_eq!(
            line(&rows[0], columns),
            format!("skipped     {:<40} → ~/.ab    dry run", "a/b")
        );
        assert_eq!(
            column(&line(&rows[0], columns), " → "),
            column(&line(&rows[2], columns), " → ")
        );
        assert_eq!(
            line(&rows[1], columns),
            format!("skipped     {long} → ~/.long  dry run")
        );

        let all_long = [
            row(
                EntryOutcome::Skipped,
                &"y".repeat(45),
                "~/.a",
                Some("dry run"),
            ),
            row(
                EntryOutcome::Skipped,
                &"z".repeat(50),
                "~/.a",
                Some("dry run"),
            ),
        ];
        let columns = columns_for(&all_long);
        assert_eq!(columns.source, COLUMN_CAP);
        assert_eq!(
            line(&all_long[0], columns),
            format!("skipped     {} → ~/.a  dry run", "y".repeat(45))
        );
    }

    // A row spelled wider than its entry was announced overflows on its own row;
    // the rows announced as they print stay in line.
    #[test]
    fn a_row_spelled_wider_than_announced_overflows_alone() {
        let columns = EntryColumns::of([("a/b", "~/.ab"), ("c/d", "~/.cd")]);
        let wider = row(
            EntryOutcome::Skipped,
            "a/b-renamed",
            "~/.ab",
            Some("dry run"),
        );
        let announced = row(EntryOutcome::Skipped, "c/d", "~/.cd", Some("dry run"));

        assert_eq!(
            line(&wider, columns),
            "skipped     a/b-renamed → ~/.ab  dry run"
        );
        assert_eq!(
            line(&announced, columns),
            "skipped     c/d → ~/.cd  dry run"
        );
    }

    // A row with no detail ends at its target, whatever the column's width.
    #[test]
    fn a_row_without_detail_has_no_trailing_space() {
        let columns = EntryColumns::of([("a", "~/.a"), ("b", "~/.a-much-wider-target")]);

        assert_eq!(
            line(&row(EntryOutcome::Deployed, "a", "~/.a", None), columns),
            "deployed    a → ~/.a"
        );
    }

    // Every label fills the outcome column, so every row's source starts in one
    // place whatever happened to it.
    #[test]
    fn each_outcome_label_fills_the_outcome_column() {
        let starts: Vec<usize> = EntryOutcome::ALL
            .iter()
            .map(|outcome| {
                column(
                    &line(&row(*outcome, "src", "~/.t", None), EntryColumns::default()),
                    "src",
                )
            })
            .collect();

        assert!(all_equal(&starts), "{starts:?}");
        assert_eq!(starts[0], under_row().len());
    }

    // An entry a drift check could not compare reads `unverified`; every other
    // skip reads `skipped`.
    #[test]
    fn a_skip_reads_unverified_only_when_it_was_not_compared() {
        assert_eq!(
            EntryOutcome::from(&SkipReason::Unverifiable),
            EntryOutcome::Unverified
        );
        assert_eq!(
            EntryOutcome::from(&SkipReason::DryRun),
            EntryOutcome::Skipped
        );
        assert_eq!(
            EntryOutcome::from(&SkipReason::UpToDate),
            EntryOutcome::Skipped
        );
    }

    // A colored label takes the room its text does and no more, so a colored
    // row lines up with a plain one.
    #[test]
    fn a_colored_label_pads_by_its_text() {
        let columns = EntryColumns::of([("bat/config", "~/.b")]);
        let row = row(EntryOutcome::Deployed, "bat/config", "~/.b", None);

        let colored = entry_line(&row, columns, true);

        assert_ne!(
            colored.as_str(),
            line(&row, columns),
            "control: it is colored"
        );
        assert_eq!(
            console::strip_ansi_codes(colored.as_str()),
            line(&row, columns),
            "{colored:?}"
        );
    }

    // A detail's lines after its first start under the source column, past the
    // outcome's label, so they read as this row's and not as a row of their own.
    #[test]
    fn a_detail_continues_under_the_source_column() {
        let columns = EntryColumns::of([("bat/config", "~/.b")]);
        let row = EntryRow {
            outcome: EntryOutcome::Skipped,
            source: DisplayPath::new("bat/config").into(),
            target: DisplayPath::new("~/.b"),
            detail: Some(Shown::from(crate::terminal_text::Prose::new("a\nb"))),
        };

        let line = line(&row, columns);

        let second = line.lines().nth(1).expect("a second line");
        assert_eq!(second, format!("{}b", under_row()), "{line:?}");
        assert_eq!(column(second, "b"), column(&line, "bat/config"), "{line:?}");
    }
}
