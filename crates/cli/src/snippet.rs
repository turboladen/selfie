//! The lines around a spec's parse failure, drawn from the file on disk.
//!
//! Only the CLI does this. A parse failure travels as a line and a column, and
//! this module is what turns those numbers back into the text they point at —
//! here, in the adapter that knows it is writing to a terminal, and never in a
//! value that also reaches the MCP server.
//!
//! A window is a span of lines, so it prints whatever the neighbors of the
//! failing line hold — a `command:` naming a credential store, a `vars:` value.
//! That is accepted on a terminal whose reader owns the file and is being sent to
//! open it, and it is why the text stops here: the same failure reaching the MCP
//! server carries a class, a line and a column and no file content at all.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

/// Lines either side of the failing one.
const CONTEXT: u64 = 2;

/// Longest line printed before it is cut short.
const MAX_LINE: usize = 200;

/// The lines around `line` in `path`, with a caret under `column`.
///
/// `line` and `column` are 1-indexed and count **characters**, which is what
/// serde-saphyr reports.
///
/// Answers `None` when `path` is not a regular file, cannot be opened, is not
/// valid UTF-8, or ends before `line` — each a case where the caller prints its
/// sentence alone.
pub(crate) fn window(path: &Path, line: u64, column: u64) -> Option<String> {
    // Both bounds saturate. `line` arrives as an unvalidated `u64` from the
    // parser, and a failure on line 1 or 2 makes `line - CONTEXT` underflow --
    // the same defect as `column - 1`, in the other axis. A stray bracket on the
    // first line is an ordinary way to reach here, not an exotic one.
    let first = line.saturating_sub(CONTEXT).max(1);
    let last = line.saturating_add(CONTEXT);

    // A second read of a file selfie does not control: the repository read it,
    // the parser rejected it, and the failure arrives here carrying the path.
    // Opening a fifo to read blocks until a writer arrives and a device node never
    // reaches an end, so ask what the path holds first. `metadata` stats rather
    // than opens, so asking cannot itself block, and it resolves symlinks, so a
    // link to a fifo answers as a fifo. Only a swap between the two reads reaches
    // this, which is why no test constructs it.
    //
    // Not `FileSystem::irregular_target_refusal`, which is the guard the library
    // uses for the same hazard: it takes a `TargetPath`, and the constructor for a
    // path already resolved -- `repository_path` -- is crate-private. The two
    // public ones expand a dotfile *target* against a home directory, which would
    // reinterpret a spec path rather than describe it. Reaching the port from here
    // means widening that constructor, and this answers the same question without
    // a library change.
    if !std::fs::metadata(path).ok()?.is_file() {
        return None;
    }

    let reader = BufReader::new(File::open(path).ok()?);
    let mut kept: Vec<(u64, String)> = Vec::new();

    for (number, text) in (1u64..).zip(reader.lines()) {
        if number > last {
            break;
        }
        // Not valid UTF-8, so there is no text to point at.
        let mut text = text.ok()?;
        if number < first {
            continue;
        }
        if number == 1 {
            // `skip_bom` advances the scanner's byte and character counts but not
            // its column, so a BOM would otherwise shift the caret one place.
            if let Some(rest) = text.strip_prefix('\u{feff}') {
                text = rest.to_string();
            }
        }
        // A file written on Windows keeps its `\r` after splitting on `\n`.
        if text.ends_with('\r') {
            text.pop();
        }

        kept.push((number, text));
    }

    // The file ended before the failing line. Normal rather than exceptional: an
    // error that runs out of input reports the line after the last one.
    let target = kept
        .iter()
        .find(|(number, _)| *number == line)
        .map(|(_, text)| text.as_str())?;

    // Sized from the last line the file actually has, not from `last`, which runs
    // past the end whenever the failure is within `CONTEXT` lines of it. A gutter
    // sized for a line number nothing prints indents every row that does.
    let gutter = kept
        .last()
        .map_or(1, |(number, _)| number.to_string().len());
    let mut out = String::new();
    for (number, text) in &kept {
        let _ = writeln!(out, "{number:>gutter$} | {}", clamp(text));
        if *number == line {
            let _ = writeln!(out, "{:>gutter$} | {}^", "", pad(target, column));
        }
    }

    Some(out)
}

/// `text`, cut short if it is longer than a terminal wants.
fn clamp(text: &str) -> String {
    if text.chars().count() <= MAX_LINE {
        return text.to_string();
    }
    let head: String = text.chars().take(MAX_LINE).collect();
    format!("{head}…")
}

/// Spaces to place a caret under `column`, replaying the prefix's own tabs.
// A tab is one column to the parser and one *stop* to the terminal, so emitting
// `column - 1` spaces misplaces the caret on an indented line. Copying the
// prefix's tabs lands it correctly under any tab width.
//
// One character means one column, which is right only for characters one cell
// wide. A wide CJK character or an emoji is one column and two cells, so the
// caret lands a cell left of the mark; a combining mark is a second character
// with no cell of its own, so it lands one right. Both are accepted: counting
// cells needs a width table, and the failing line sits directly above.
fn pad(text: &str, column: u64) -> String {
    let take = usize::try_from(column.saturating_sub(1)).unwrap_or(usize::MAX);
    // Bounded by what `clamp` leaves on screen as well as by the line's own
    // length. A column past `MAX_LINE` names a character the printed row ends in
    // an ellipsis instead of, so padding to it puts the caret in empty space and
    // wraps the terminal.
    let available = text.chars().count().min(MAX_LINE);
    text.chars()
        .take(take.min(available))
        .map(|c| if c == '\t' { '\t' } else { ' ' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn file(contents: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    // The caret pad, character by character. `pad` decides where the caret lands,
    // so these are the cases that decide whether it lands right.
    #[test]
    fn a_caret_pad_counts_characters_not_bytes() {
        // Three characters, six bytes. A byte count would put the caret at 6.
        assert_eq!(pad("héllo", 4).chars().count(), 3);
    }

    #[test]
    fn a_caret_pad_replays_a_tab_rather_than_spacing_past_it() {
        // The parser counts a tab as one column; a terminal advances it to a stop.
        // Emitting a space here would land the caret wherever the tab width is not
        // one, which is every terminal.
        assert_eq!(pad("\t\tkey: value", 3), "\t\t");
    }

    #[test]
    fn a_caret_pad_stops_at_the_end_of_a_short_line() {
        assert_eq!(pad("ab", 99), "  ");
    }

    #[test]
    fn a_caret_pad_survives_column_zero() {
        // `located` filters only line 0, so column 0 crosses the boundary as an
        // ordinary `u64`. `column - 1` would underflow here.
        assert_eq!(pad("abc", 0), "");
    }

    // Both directions of the accepted width drift, together. Fixture only the wide
    // case and the next reader concludes characters under-count cells, adds a
    // compensation, and makes combining marks worse.
    #[test]
    fn the_caret_drifts_in_both_directions_on_characters_that_are_not_one_cell() {
        // Two cells, one character: the caret lands one cell left of the mark.
        assert_eq!(pad("世界: x", 3).chars().count(), 2);
        // Two characters, one cell: `e` then U+0301. The caret lands one right.
        assert_eq!(pad("e\u{301}x: y", 3).chars().count(), 2);
    }

    #[test]
    fn a_failure_on_the_first_line_has_no_preceding_context() {
        // `line - CONTEXT` underflows a `u64` here. A stray bracket on line 1 is an
        // ordinary way to reach this, not an exotic one.
        let f = file("environments: {oops\nname: x\n");
        let out = window(f.path(), 1, 15).expect("a window");

        assert!(out.starts_with("1 | environments:"), "got: {out}");
        assert!(out.contains("^"), "got: {out}");
    }

    #[test]
    fn a_failure_on_the_last_line_has_no_following_context() {
        let f = file("a: 1\nb: 2\nc: {oops\n");
        let out = window(f.path(), 3, 5).expect("a window");

        assert!(out.contains("3 | c: {oops"), "got: {out}");
        assert!(!out.contains("4 |"), "got: {out}");
    }

    #[test]
    fn a_window_names_the_lines_either_side() {
        let f = file("a: 1\nb: 2\nc: 3\nd: 4\ne: 5\nf: 6\ng: 7\n");
        let out = window(f.path(), 4, 1).expect("a window");

        for expected in ["2 | b: 2", "3 | c: 3", "4 | d: 4", "5 | e: 5", "6 | f: 6"] {
            assert!(out.contains(expected), "missing {expected} in: {out}");
        }
        assert!(!out.contains("1 | a: 1"), "got: {out}");
        assert!(!out.contains("7 | g: 7"), "got: {out}");
    }

    #[test]
    fn a_byte_order_mark_does_not_shift_the_caret() {
        // The scanner's `skip_bom` advances bytes and characters but not the
        // column, so leaving the mark in place would push the caret one right.
        let f = file("\u{feff}environments: {oops\n");
        let out = window(f.path(), 1, 15).expect("a window");

        assert!(out.contains("1 | environments: {oops"), "got: {out}");
        let caret = out.lines().nth(1).expect("a caret line");
        assert_eq!(caret.find('^'), Some(18), "got: {caret}");
    }

    #[test]
    fn a_very_long_line_is_cut_short() {
        let long = "x".repeat(MAX_LINE * 2);
        let f = file(&format!("a: {long}\n"));
        let out = window(f.path(), 1, 1).expect("a window");

        assert!(out.contains('…'), "got: {out}");
        assert!(
            out.lines().next().unwrap().chars().count() < MAX_LINE + 20,
            "the line was not clamped"
        );
    }

    // A failure past the cut. The row ends in an ellipsis, so a caret placed at the
    // real column sits in empty space well past it and wraps the terminal.
    #[test]
    fn a_caret_beyond_the_cut_stops_at_it() {
        let long = "x".repeat(MAX_LINE * 2);
        let f = file(&format!("a: {long}\n"));
        let column = u64::try_from(MAX_LINE + 50).unwrap();
        let out = window(f.path(), 1, column).expect("a window");

        let caret = out.lines().nth(1).expect("a caret line");
        let content = out.lines().next().expect("a content line");
        assert!(
            caret.chars().count() <= content.chars().count(),
            "the caret ran past the line it points at: {out}"
        );
    }

    // The gutter is as wide as the widest number printed. A failure within
    // `CONTEXT` lines of the end pushes `last` past the file, and sizing on it
    // indents every row by a column no number occupies.
    #[test]
    fn a_failure_near_the_end_of_a_long_file_keeps_a_tight_gutter() {
        let mut contents = String::new();
        for number in 1..=9 {
            contents.push_str(&format!("k{number}: {number}\n"));
        }
        let f = file(&contents);
        let out = window(f.path(), 9, 1).expect("a window");

        assert!(out.starts_with("7 | k7: 7"), "got: {out}");
    }

    #[test]
    fn a_file_that_ends_before_the_failing_line_has_no_window() {
        // Normal rather than exceptional: an error that runs out of input reports
        // the line after the last one.
        let f = file("a: 1\n");
        assert!(window(f.path(), 9, 1).is_none());
    }

    #[test]
    fn a_file_that_is_not_text_has_no_window() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(&[b'a', b':', b' ', 0xff, 0xfe, b'\n']).unwrap();
        f.flush().unwrap();

        assert!(window(f.path(), 1, 1).is_none());
    }

    // The guard, watched firing. Reading a fifo blocks until a writer arrives, so
    // without the check above this does not fail -- it hangs, after the CLI has
    // already printed the sentence and with nothing to interrupt it. The deadline
    // is what turns that into a failure rather than a wedged suite; a real fifo
    // because `MockFileSystem` cannot block and would prove nothing.
    #[cfg(unix)]
    #[test]
    fn a_fifo_at_the_failing_path_has_no_window_and_does_not_block() {
        use std::sync::mpsc;

        let temp = tempfile::TempDir::new().unwrap();
        let ghost = temp.path().join("ghost.yml");
        nix::unistd::mkfifo(&ghost, nix::sys::stat::Mode::S_IRWXU).unwrap();

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(window(&ghost, 1, 1));
        });

        let answer = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("reading a fifo must not block the CLI");

        assert!(answer.is_none(), "a fifo has no lines to show");
    }

    #[test]
    fn a_file_that_is_not_there_has_no_window() {
        assert!(window(Path::new("/nonexistent/ghost.yml"), 1, 1).is_none());
    }

    #[test]
    fn a_windows_line_ending_is_not_printed() {
        let f = file("a: 1\r\nb: {oops\r\n");
        let out = window(f.path(), 2, 4).expect("a window");

        assert!(!out.contains('\r'), "got: {out:?}");
    }
}
