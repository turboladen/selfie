//! Text from outside selfie, made safe to show on a terminal.
//!
//! A file name, a spec's text, a library message naming either, or a file's
//! content can carry characters that act on the terminal instead of showing:
//! an escape sequence that clears the screen or retitles the window, a carriage
//! return, a bidi override that shows `txt.exe` as `exe.txt`, or an invisible
//! character that makes one name read as another. Each is written as an escape
//! a reader can see: control characters as their Rust escapes (`\t`, `\u{1b}`),
//! everything else as its code point (`\u{202e}`).

use std::borrow::Cow;

use icu_properties::props::{DefaultIgnorableCodePoint, GeneralCategory};
use icu_properties::{CodePointMapData, CodePointSetData};

/// Whether `c` is shown as an escape: a control character (general category
/// Cc), a format character (Cf), a line or paragraph separator (Zl, Zp), or a
/// default-ignorable code point, which renders as nothing.
///
/// Combining marks are none of these, so an accented name stored decomposed, as
/// macOS often stores it, shows as written.
pub(crate) fn is_escaped(c: char) -> bool {
    matches!(
        CodePointMapData::<GeneralCategory>::new().get(c),
        GeneralCategory::Control
            | GeneralCategory::Format
            | GeneralCategory::LineSeparator
            | GeneralCategory::ParagraphSeparator
    ) || CodePointSetData::new::<DefaultIgnorableCodePoint>().contains(c)
}

/// `text` with every character [`is_escaped`] names written as an escape.
///
/// For a path, a name, or a message on one line: a newline in it is escaped
/// too, so it cannot start a line of its own.
pub(crate) fn escape(text: &str) -> Cow<'_, str> {
    escape_except(text, |_| false)
}

/// A path or source as the terminal shows it, made only by
/// [`DisplayPath::new`], so text of this type has been through [`escape`].
// Escaped rather than dropped: dropping would show `evil\rgood` as the name of
// a real file `evilgood`. Two names can still read alike in two ways, both
// accepted: a backslash is not escaped, so a name holding the six characters
// `\u{1b}` reads like one holding ESC, and `DisplayPath::of` shows a path that
// is not UTF-8 lossily, so two such names can show the same replacement
// characters.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct DisplayPath(String);

impl DisplayPath {
    /// `text` with its escaped characters written as escapes.
    pub(crate) fn new(text: &str) -> Self {
        Self(escape(text).into_owned())
    }

    /// `path` as [`DisplayPath::new`] shows its text.
    pub(crate) fn of(path: &std::path::Path) -> Self {
        // The one place outside tests a path's text is taken for display; the
        // gate in tests/no_raw_path_display.rs holds every other one to it.
        Self::new(&path.display().to_string())
    }
}

impl std::fmt::Display for DisplayPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn escape_except(text: &str, kept: impl Fn(char) -> bool) -> Cow<'_, str> {
    if !text.chars().any(|c| is_escaped(c) && !kept(c)) {
        return Cow::Borrowed(text);
    }
    let mut shown = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        if is_escaped(c) && !kept(c) {
            push_escaped(&mut shown, c);
        } else {
            shown.push(c);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    // Each kind of character that acts on a terminal or hides in a name shows
    // as its escape, with the visible text around it kept.
    #[test]
    fn each_kind_shows_as_its_escape() {
        let cases = [
            ("a\u{1b}[2Jb", "a\\u{1b}[2Jb"),
            ("a\u{1b}]0;pwned\u{7}b", "a\\u{1b}]0;pwned\\u{7}b"),
            ("a\u{9b}b", "a\\u{9b}b"),
            ("evil\rgood", "evil\\rgood"),
            ("evil\ngood", "evil\\ngood"),
            ("a\tb", "a\\tb"),
            ("a\u{7f}b", "a\\u{7f}b"),
            ("a\u{202E}txt.exe", "a\\u{202e}txt.exe"),
            ("a\u{2066}b", "a\\u{2066}b"),
            ("pay\u{200B}pal", "pay\\u{200b}pal"),
            ("a\u{200D}b", "a\\u{200d}b"),
            ("\u{FEFF}a", "\\u{feff}a"),
            ("a\u{AD}b", "a\\u{ad}b"),
            ("a\u{61C}b", "a\\u{61c}b"),
            ("a\u{180E}b", "a\\u{180e}b"),
            ("a\u{2028}b", "a\\u{2028}b"),
            ("a\u{2029}b", "a\\u{2029}b"),
            ("evil\u{E0001}\u{E0041}", "evil\\u{e0001}\\u{e0041}"),
            ("config\u{FE0F}", "config\\u{fe0f}"),
            ("a\u{34F}b", "a\\u{34f}b"),
            ("a\u{3164}b", "a\\u{3164}b"),
            ("a\u{FFF9}b", "a\\u{fff9}b"),
            ("a\u{600}b", "a\\u{600}b"),
        ];

        for (text, shown) in cases {
            assert_eq!(escape(text), shown, "{text:?}");
        }
    }

    // Combining marks, other scripts and emoji show as written: an accented
    // name stored decomposed must read as the name it is.
    #[test]
    fn ordinary_text_is_unchanged() {
        for text in ["cafe\u{301}", "/tmp/日本/conf", "🦀 crab", "plain-name.yml"] {
            assert!(matches!(escape(text), Cow::Borrowed(_)), "{text:?}");
        }
    }

    // Two names that differ only by such a character never read as one name.
    #[test]
    fn names_differing_by_one_such_character_read_differently() {
        let names = [
            "evilgood",
            "evil\rgood",
            "evil\ngood",
            "evil\u{1b}good",
            "evil\u{202E}good",
            "evil\u{200B}good",
            "evil\u{FEFF}good",
            "evil\u{E0041}good",
            "evil\u{FE0F}good",
        ];

        let shown: std::collections::BTreeSet<String> =
            names.iter().map(|name| escape(name).into_owned()).collect();

        assert_eq!(shown.len(), names.len(), "{shown:?}");
    }

    // Escaped text has nothing left to escape, so a path escaped as a
    // DisplayPath and again by a printer reads the same.
    #[test]
    fn escaping_twice_is_escaping_once() {
        let once = escape("a\u{1b}[2J\u{202E}\u{FE0F}\tb").into_owned();
        assert_eq!(escape(&once), once);
    }
}
