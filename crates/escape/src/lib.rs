//! Escaping for text from outside selfie, shared by selfie's adapters.
//!
//! A file name, a spec's text or a logged value can carry characters that act
//! on a terminal instead of showing: an escape sequence, a carriage return, a
//! bidi override, or an invisible character that makes one name read as
//! another. This crate holds the character sets that name them, the one loop
//! that writes each as a visible escape, and a log writer built on both.

use std::borrow::Cow;

use icu_properties::props::{DefaultIgnorableCodePoint, GeneralCategory};
use icu_properties::{CodePointMapData, CodePointSetData};

/// Whether `c` is in the guard set, which holds what can act on the terminal or
/// reorder the text around it: controls other than a newline and a tab, the
/// line and paragraph separators, the bidi controls and the tag characters.
// Joiners, variation selectors and soft hyphens are left to render, so an emoji
// sequence or a Persian word reads as written. A caller decides for itself
// whether a newline and a tab show.
pub fn in_guard_set(c: char) -> bool {
    (c.is_control() && c != '\n' && c != '\t')
        || matches!(
            CodePointMapData::<GeneralCategory>::new().get(c),
            GeneralCategory::LineSeparator | GeneralCategory::ParagraphSeparator
        )
        || matches!(
            c,
            '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
        )
        || ('\u{E0000}'..='\u{E007F}').contains(&c)
}

/// Whether `c` is in the base set, which holds the guard set: a control
/// character (Cc), a format character (Cf), a line or paragraph separator (Zl,
/// Zp), or a default-ignorable code point, which renders as nothing.
// Combining marks are none of these, so an accented name stored decomposed, as
// macOS often stores it, shows as written.
pub fn in_base_set(c: char) -> bool {
    matches!(
        CodePointMapData::<GeneralCategory>::new().get(c),
        GeneralCategory::Control
            | GeneralCategory::Format
            | GeneralCategory::LineSeparator
            | GeneralCategory::ParagraphSeparator
    ) || CodePointSetData::new::<DefaultIgnorableCodePoint>().contains(c)
}

/// Whether `c` is in the strict set, for names and paths, where two values that
/// read alike are two different things: the base set, every space separator
/// other than U+0020, and U+2800, the braille blank.
// Those blanks are not added to the base set because in file content U+00A0
// and U+3000 are ordinary French and CJK text.
pub fn in_strict_set(c: char) -> bool {
    in_base_set(c)
        || c == '\u{2800}'
        || (c != ' '
            && CodePointMapData::<GeneralCategory>::new().get(c) == GeneralCategory::SpaceSeparator)
}

/// Returns `record`, one record of a log, with the strict set escaped, every
/// escape sequence, line break and tab included, so it shows uncolored on one
/// line. A final newline, which ends the record, is kept.
pub fn log_record(record: &str) -> Cow<'_, str> {
    let line = record.strip_suffix('\n').unwrap_or(record);
    match escape_with(line, in_strict_set, Sgr::Escape) {
        Cow::Borrowed(_) => Cow::Borrowed(record),
        Cow::Owned(mut shown) => {
            shown.push_str(&record[line.len()..]);
            Cow::Owned(shown)
        }
    }
}

/// What [`escape_with`] does with an SGR sequence selfie's own styling writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sgr {
    /// Escape it, as any other sequence is.
    Escape,
    /// Write it as it is.
    Keep,
    /// Leave it out.
    Strip,
}

// Provenance is the guard: the CLI escapes text from outside selfie before it
// styles it or joins it into a line, so a block reaching the CLI's printer
// holds only the sequences selfie's styling wrote. This check stands behind
// that one.
//
// It keeps what the `console` crate writes for the styles the CLI's `Paint`
// uses, one parameter per
// sequence: reset, bold, dim, italic, underline, and red through cyan. Black,
// white and the bright colors could hide text on one background or another, so
// they are escaped. Dim is selfie's own too, so only provenance stops a
// repository's.
fn is_own_sgr_parameter(parameter: u16) -> bool {
    matches!(parameter, 0..=4 | 31..=36)
}

// The length of the SGR sequence `text` starts with, `ESC [ n m`, when its one
// parameter is one of selfie's own.
fn own_sgr_length(text: &str) -> Option<usize> {
    let body = text.strip_prefix("\u{1b}[")?;
    let end = body.find(|c: char| !c.is_ascii_digit())?;
    if body.as_bytes()[end] != b'm' {
        return None;
    }
    let own = body[..end].parse().is_ok_and(is_own_sgr_parameter);
    own.then_some(2 + end + 1)
}

/// Returns `text` with each character `escaped` names written as an escape a
/// reader can see: a control character as its Rust escape (`\t`, `\u{1b}`),
/// anything else as its code point (`\u{202e}`). An SGR sequence selfie's own
/// styling writes is kept, removed or escaped as `sgr` says. A backslash is
/// never escaped, so escaping twice is escaping once.
pub fn escape_with(text: &str, escaped: impl Fn(char) -> bool, sgr: Sgr) -> Cow<'_, str> {
    // An own sequence is removed even when nothing else in `text` is escaped.
    if !text
        .chars()
        .any(|c| escaped(c) || (sgr != Sgr::Escape && c == '\u{1b}'))
    {
        return Cow::Borrowed(text);
    }
    let mut shown = String::with_capacity(text.len() + 8);
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if sgr != Sgr::Escape
            && c == '\u{1b}'
            && let Some(length) = own_sgr_length(rest)
        {
            if sgr == Sgr::Keep {
                shown.push_str(&rest[..length]);
            }
            rest = &rest[length..];
            continue;
        }
        if escaped(c) {
            push_escaped(&mut shown, c);
        } else {
            shown.push(c);
        }
        rest = &rest[c.len_utf8()..];
    }
    Cow::Owned(shown)
}

// A control character as its Rust escape, which names the common ones (`\t`,
// `\n`, `\r`); anything else as its code point. A backslash is left as it is,
// so a name holding the six characters `\u{1b}` reads like one holding ESC.
// Escaping it too would double every backslash in a command source, and leaving
// it is what makes escaping twice the same as escaping once.
fn push_escaped(shown: &mut String, c: char) {
    if c.is_control() {
        shown.extend(c.escape_debug());
    } else {
        shown.extend(c.escape_unicode());
    }
}

/// One record of a log, for a `tracing-subscriber` writer. Nothing written to
/// it reaches its sink until it is dropped. Then the whole record is escaped
/// with [`log_record`] and written at once, on one line.
///
/// A record ends in one line feed, and every other line break in it is
/// escaped. That holds only while the subscriber makes one writer per record
/// and its formatter adds no line break but the last, which is true of the
/// default single-line format and not of `.pretty()`.
pub struct LogEvent<W: std::io::Write> {
    sink: W,
    text: Vec<u8>,
}

impl<W: std::io::Write> LogEvent<W> {
    /// Returns a record that is written to `sink` when it is dropped.
    pub fn new(sink: W) -> Self {
        Self {
            sink,
            text: Vec::new(),
        }
    }

    // tracing-subscriber writes each event with one `write_all` today. The
    // buffer keeps the event whole if a writer splits it, since a split inside
    // an escape sequence or a character would have each half escaped alone.
    fn emit(&mut self) {
        let text = std::mem::take(&mut self.text);
        let text = String::from_utf8_lossy(&text);
        // This runs in `drop`, so a failed write has nowhere to be reported.
        let shown = log_record(&text);
        let _ = self.sink.write_all(shown.as_bytes());
    }
}

impl<W: std::io::Write> std::io::Write for LogEvent<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.text.extend_from_slice(buf);
        Ok(buf.len())
    }

    // Nothing is written before the event ends, so there is nothing to flush.
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<W: std::io::Write> Drop for LogEvent<W> {
    fn drop(&mut self) {
        self.emit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // An own sequence is kept or removed as `sgr` says even when nothing else
    // in the text is escaped, and a foreign one is escaped either way.
    #[test]
    fn an_own_sequence_follows_sgr_whatever_else_is_escaped() {
        let not_esc = |c: char| c == '\u{202e}';
        let own = "\u{1b}[31mred";
        assert_eq!(escape_with(own, not_esc, Sgr::Strip), "red");
        assert_eq!(escape_with(own, not_esc, Sgr::Keep), own);
        assert_eq!(escape_with(own, not_esc, Sgr::Escape), own);
        assert_eq!(
            escape_with(own, in_guard_set, Sgr::Escape),
            "\\u{1b}[31mred"
        );
    }

    // A record with nothing to escape is returned as it is, final line feed
    // included, and one without a final line feed gains none.
    #[test]
    fn a_plain_record_is_returned_as_it_is() {
        assert!(matches!(log_record("WARN a\n"), Cow::Borrowed("WARN a\n")));
        assert_eq!(log_record("WARN a\u{200b}\n"), "WARN a\\u{200b}\n");
        assert_eq!(log_record("WARN \n b"), "WARN \\n b");
    }

    // Each set's members, one row per class: (character, guard, base, strict).
    // The rows sit on the edges of each hand-written range, so a range one code
    // point too wide or too narrow fails here.
    #[test]
    fn the_sets_are_pinned() {
        let rows = [
            ('\0', true, true, true),
            ('\t', false, true, true),
            ('\n', false, true, true),
            ('\r', true, true, true),
            ('\u{1b}', true, true, true),
            ('\u{7f}', true, true, true),
            ('\u{85}', true, true, true),
            ('\u{9b}', true, true, true),
            ('\u{61B}', false, false, false),
            ('\u{61C}', true, true, true),
            ('\u{61D}', false, false, false),
            ('\u{200E}', true, true, true),
            ('\u{200F}', true, true, true),
            ('\u{2010}', false, false, false),
            ('\u{2029}', true, true, true),
            ('\u{2028}', true, true, true),
            ('\u{202A}', true, true, true),
            ('\u{202E}', true, true, true),
            ('\u{2060}', false, true, true),
            ('\u{2065}', false, true, true),
            ('\u{2066}', true, true, true),
            ('\u{2069}', true, true, true),
            ('\u{206A}', false, true, true),
            ('\u{DFFFF}', false, false, false),
            ('\u{E0000}', true, true, true),
            ('\u{E0041}', true, true, true),
            ('\u{E007F}', true, true, true),
            ('\u{E0080}', false, true, true),
            ('\u{E0100}', false, true, true),
            ('\u{200B}', false, true, true),
            ('\u{200C}', false, true, true),
            ('\u{200D}', false, true, true),
            ('\u{FEFF}', false, true, true),
            ('\u{AD}', false, true, true),
            ('\u{180E}', false, true, true),
            ('\u{FE0F}', false, true, true),
            ('\u{34F}', false, true, true),
            ('\u{3164}', false, true, true),
            ('\u{FFF9}', false, true, true),
            ('\u{600}', false, true, true),
            ('\u{A0}', false, false, true),
            ('\u{1680}', false, false, true),
            ('\u{2000}', false, false, true),
            ('\u{200A}', false, false, true),
            ('\u{202F}', false, false, true),
            ('\u{205F}', false, false, true),
            ('\u{3000}', false, false, true),
            ('\u{2800}', false, false, true),
            (' ', false, false, false),
            ('a', false, false, false),
            ('\u{301}', false, false, false),
            ('日', false, false, false),
            ('\u{1F980}', false, false, false),
        ];
        for (c, guard, base, strict) in rows {
            assert_eq!(
                (in_guard_set(c), in_base_set(c), in_strict_set(c)),
                (guard, base, strict),
                "{c:?}"
            );
        }
    }

    // Each set holds the one before it: what can act on the terminal is also
    // what a name or a line of content escapes. The strict set adds exactly
    // the 16 space separators other than U+0020, and U+2800.
    #[test]
    fn the_sets_nest() {
        let mut blanks = Vec::new();
        for c in (0..=u32::from(char::MAX)).filter_map(char::from_u32) {
            assert!(!in_guard_set(c) || in_base_set(c), "{c:?}");
            assert!(!in_base_set(c) || in_strict_set(c), "{c:?}");
            if in_strict_set(c) && !in_base_set(c) {
                blanks.push(c);
            }
        }
        assert_eq!(blanks.len(), 17, "{blanks:?}");
        assert!(
            blanks.contains(&'\u{2800}') && !blanks.contains(&' '),
            "{blanks:?}"
        );
    }

    // What a log event written in `pieces` leaves on its sink.
    fn logged(pieces: &[&[u8]]) -> String {
        let mut sink = Vec::new();
        {
            let mut event = LogEvent::new(&mut sink);
            for piece in pieces {
                std::io::Write::write_all(&mut event, piece).unwrap();
            }
        }
        String::from_utf8(sink).unwrap()
    }

    // An escape sequence in a log event shows as text, even one in selfie's own
    // palette, and an event split inside it, inside a character and at a line
    // break is written as it is when written whole, on one line.
    #[test]
    fn a_log_event_is_escaped_whole_however_it_is_written() {
        let event = "DEBUG \u{1b}[31ma\u{202e}b\nnext\n".as_bytes();
        let sgr = 8;
        let character = event.iter().position(|&b| b == 0xe2).unwrap() + 1;
        let line_break = event.iter().position(|&b| b == b'\n').unwrap();
        let pieces = [
            &event[..sgr],
            &event[sgr..character],
            &event[character..line_break],
            &event[line_break..],
        ];
        let whole = logged(&[event]);
        assert_eq!(whole, "DEBUG \\u{1b}[31ma\\u{202e}b\\nnext\n");
        assert_eq!(logged(&pieces), whole);
    }
}
