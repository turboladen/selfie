//! Text from outside selfie, made safe to show on a terminal.
//!
//! A file name, a spec's text, a library message naming either, or a file's
//! content can carry characters that act on the terminal instead of showing:
//! an escape sequence that clears the screen or retitles the window, a carriage
//! return, a bidi override that shows `txt.exe` as `exe.txt`, or an invisible
//! character that makes one name read as another. Each is written as an escape
//! a reader can see: control characters as their Rust escapes (`\t`, `\u{1b}`),
//! everything else as its code point (`\u{202e}`).
//!
//! How much is escaped depends on what the text is. A name or a path is escaped
//! strictly ([`escape`]), since an invisible character in it makes it read as
//! another. Prose and tool output ([`escape_prose`]) escape only what can act on
//! the terminal or reorder the text, so emoji sequences and joiners render.

use std::borrow::Cow;
use std::fmt;

use icu_properties::props::{DefaultIgnorableCodePoint, GeneralCategory};
use icu_properties::{CodePointMapData, CodePointSetData};

/// Whether `c` is escaped in a name or a path: a control character (general
/// category Cc), a format character (Cf), a line or paragraph separator (Zl,
/// Zp), or a default-ignorable code point, which renders as nothing.
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

// Whether `c` is escaped in prose: what can act on the terminal, break a line
// or reorder the text around it. Controls, the line and paragraph separators,
// the bidi controls and the tag characters. Joiners, variation selectors and
// soft hyphens are left to render, so an emoji sequence or a Persian word reads
// as written.
fn is_escaped_in_prose(c: char) -> bool {
    matches!(
        CodePointMapData::<GeneralCategory>::new().get(c),
        GeneralCategory::Control
            | GeneralCategory::LineSeparator
            | GeneralCategory::ParagraphSeparator
    ) || matches!(
        c,
        '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    ) || ('\u{E0000}'..='\u{E007F}').contains(&c)
}

/// `text`, a name or a path, with every character [`is_escaped`] names written
/// as an escape, a newline included, so it stays on one line.
pub(crate) fn escape(text: &str) -> Cow<'_, str> {
    escape_with(text, is_escaped, |_| false, Sgr::Escape)
}

/// `items`, each through [`escape`], joined by `separator`: a list of names
/// or values from outside selfie, on one line.
pub(crate) fn escape_joined<S: AsRef<str>>(
    items: impl IntoIterator<Item = S>,
    separator: &str,
) -> String {
    items
        .into_iter()
        .map(|item| escape(item.as_ref()).into_owned())
        .collect::<Vec<_>>()
        .join(separator)
}

/// `text`, prose or a tool's output, with what could act on the terminal or
/// reorder the text written as an escape. A newline stays a newline.
pub(crate) fn escape_prose(text: &str) -> Cow<'_, str> {
    escape_with(text, is_escaped_in_prose, |c| c == '\n', Sgr::Escape)
}

/// As [`escape`], but a tab stays a tab: for a line of file content, where a
/// tab is ordinary and an invisible character must still show.
pub(crate) fn escape_keeping_tabs(line: &str) -> Cow<'_, str> {
    escape_with(line, is_escaped, |c| c == '\t', Sgr::Escape)
}

/// As [`escape_prose`], but a control character is left for the caller to
/// drop: for command output, whose colors and carriage returns are removed
/// rather than shown.
fn escape_unrendered(text: &str) -> Cow<'_, str> {
    escape_with(text, is_escaped_in_prose, char::is_control, Sgr::Escape)
}

/// `line`, one line of a command's output, as a terminal would leave it: its
/// colors removed, only the text after its last carriage return kept, tabs as
/// spaces, and no other control character left, with what could still reorder
/// the text written as an escape.
pub(crate) fn printable(line: &str) -> String {
    if !line
        .chars()
        .any(|c| c.is_control() || is_escaped_in_prose(c))
    {
        return line.to_string();
    }
    let line = console::strip_ansi_codes(line);
    // A carriage return redraws the line in place, so only the text after the
    // last one is still on screen: a progress meter shows its final state.
    let shown = line
        .rsplit('\r')
        .find(|part| !part.trim().is_empty())
        .unwrap_or_default();
    let kept: String = shown
        .chars()
        .map(|c| if c == '\t' { ' ' } else { c })
        .filter(|c| !c.is_control())
        .collect();
    escape_unrendered(&kept).into_owned()
}

/// `text`, a command's output with its surrounding blank space trimmed, each
/// line through [`printable`], with its line breaks kept.
pub(crate) fn tool_output(text: &str) -> String {
    text.trim()
        .lines()
        .map(printable)
        .collect::<Vec<_>>()
        .join("\n")
}

/// `block`, text a printer is about to write, as the stream gets it. Every
/// part of it from outside selfie must already have been escaped: this is a
/// second guard behind that, which escapes anything that could still act on
/// the terminal except a newline or a tab. selfie's own color and weight
/// sequences are kept when `colors` is on and removed when it is off.
pub(crate) fn guard(block: &str, colors: bool) -> Cow<'_, str> {
    let sgr = if colors { Sgr::Keep } else { Sgr::Strip };
    escape_with(block, is_escaped_in_prose, |c| c == '\n' || c == '\t', sgr)
}

// What the escaping loop does with an SGR sequence selfie's own styling writes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sgr {
    Escape,
    Keep,
    Strip,
}

// Provenance is the guard: text from outside selfie is escaped before it is
// styled or joined into a line, so a block reaching a printer holds only the
// sequences selfie's styling wrote. This check stands behind that one.
//
// It keeps what `console` writes for the styles selfie uses, one parameter per
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

/// How a caller styles text: a color, a weight, or both.
pub(crate) type Paint = fn(console::StyledObject<String>) -> console::StyledObject<String>;

/// Text safe to print: every part of it from outside selfie has been escaped,
/// and only selfie styled it.
///
/// The printers that keep styling take this type, so text from outside selfie
/// reaches them only through [`Shown::name`], [`Shown::prose`], a
/// [`DisplayPath`], or [`shown!`], whose arguments must each be [`Trusted`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Shown(String);

impl Shown {
    /// `text`, a name, escaped as [`escape`] escapes it.
    pub(crate) fn name(text: &str) -> Self {
        Self(escape(text).into_owned())
    }

    /// `text`, prose or a tool's output, escaped as [`escape_prose`] escapes it,
    /// with every line after the first indented two columns, so that none of
    /// them starts at the left margin or a table cell's edge.
    pub(crate) fn prose(text: &str) -> Self {
        Self::indented(Self(escape_prose(text).into_owned()), "  ")
    }

    /// `line`, a line of file content, escaped as [`escape_keeping_tabs`]
    /// escapes it.
    pub(crate) fn content(line: &str) -> Self {
        Self(escape_keeping_tabs(line).into_owned())
    }

    /// `items`, names, each escaped as [`Shown::name`] escapes it, joined by
    /// `separator`.
    pub(crate) fn joined<S: AsRef<str>>(
        items: impl IntoIterator<Item = S>,
        separator: &'static str,
    ) -> Self {
        Self(escape_joined(items, separator))
    }

    /// `items` joined by `separator`, each shown as it is.
    pub(crate) fn join(items: impl IntoIterator<Item = Shown>, separator: &'static str) -> Self {
        Self(
            items
                .into_iter()
                .map(|item| item.0)
                .collect::<Vec<_>>()
                .join(separator),
        )
    }

    /// `text`, prose whose lines another layer lays out, escaped as
    /// [`escape_prose`] escapes it with nothing added: a block the library
    /// already indented, or text the caller indents itself.
    pub(crate) fn block(text: &str) -> Self {
        Self(escape_prose(text).into_owned())
    }

    /// `text`, a command's output, cleaned as [`tool_output`] cleans it, with
    /// every line after the first starting with `indent`.
    pub(crate) fn tool_output(text: &str, indent: &'static str) -> Self {
        Self(tool_output(text).replace('\n', &format!("\n{indent}")))
    }

    /// `text` with every line after the first starting with `indent`, so no
    /// line of it starts at the left margin.
    pub(crate) fn indented(text: Shown, indent: &'static str) -> Self {
        Self(text.0.replace('\n', &format!("\n{indent}")))
    }

    /// `self` styled by `paint` when `use_colors`.
    ///
    /// The styling is written whatever the stream; the printer removes it when
    /// the stream it writes to has colors off.
    #[must_use]
    pub(crate) fn paint(self, use_colors: bool, paint: Paint) -> Self {
        if use_colors {
            Self(paint(console::style(self.0).force_styling(true)).to_string())
        } else {
            self
        }
    }

    /// Returns the text, escapes and styling included.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    // Only `shown!` calls this, once it has checked that every argument is
    // `Trusted`.
    #[doc(hidden)]
    pub(crate) fn assembled(text: String) -> Self {
        Self(text)
    }
}

impl From<&'static str> for Shown {
    /// Shows selfie's own literal text as it is written.
    fn from(text: &'static str) -> Self {
        Self(text.to_string())
    }
}

impl From<DisplayPath> for Shown {
    fn from(path: DisplayPath) -> Self {
        Self(path.0)
    }
}

impl fmt::Display for Shown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::ops::Deref for Shown {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl PartialEq<&str> for Shown {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

/// A value [`shown!`] may format: text already safe to print, or a number.
pub(crate) trait Trusted: fmt::Display {}

impl Trusted for Shown {}
impl Trusted for DisplayPath {}
impl Trusted for &'static str {}
impl<T: Trusted + ?Sized> Trusted for &T {}
macro_rules! trusted_numbers {
    ($($number:ty),*) => { $(impl Trusted for $number {})* };
}
trusted_numbers!(u8, u16, u32, u64, usize, i32, i64, f64);

#[doc(hidden)]
pub(crate) fn trusted<T: Trusted>(value: T) -> T {
    value
}

// Whether `format` names an argument inline, as `{name}` does. Such an
// argument is captured by `format!` without passing through `trusted`, so
// `shown!` refuses the format at compile time.
#[doc(hidden)]
pub(crate) const fn has_inline_argument(format: &str) -> bool {
    let bytes = format.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'{' {
            let next = bytes[i + 1];
            if next == b'{' {
                i += 2;
                continue;
            }
            if next == b'_' || next.is_ascii_alphabetic() {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Format a [`Shown`] from [`Trusted`] arguments, given by position.
///
/// A format naming an argument inline, such as `{name}`, does not compile.
macro_rules! shown {
    ($format:literal $(, $argument:expr)* $(,)?) => {{
        const _: () = assert!(
            !$crate::terminal_text::has_inline_argument($format),
            "shown! takes its arguments by position, so each is checked as Trusted"
        );
        $crate::terminal_text::Shown::assembled(format!(
            $format $(, $crate::terminal_text::trusted(&$argument))*
        ))
    }};
}
pub(crate) use shown;

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

    /// Returns the escaped text.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DisplayPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// The one escaping loop. A character `escaped` names and `kept` does not keep
// is written as an escape; an SGR sequence selfie's own styling writes is kept,
// removed or escaped as `sgr` says.
fn escape_with(
    text: &str,
    escaped: fn(char) -> bool,
    kept: impl Fn(char) -> bool,
    sgr: Sgr,
) -> Cow<'_, str> {
    if !text.chars().any(|c| escaped(c) && !kept(c)) {
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
        if escaped(c) && !kept(c) {
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

    #[test]
    fn a_line_of_content_keeps_its_tabs() {
        assert_eq!(
            escape_keeping_tabs("key\tvalue\u{1b}[2J"),
            "key\tvalue\\u{1b}[2J"
        );
    }

    // Prose escapes only what acts on the terminal or reorders text, so an
    // emoji sequence, a Persian word and a soft hyphen render as written.
    #[test]
    fn prose_keeps_joiners_and_escapes_what_acts_on_the_terminal() {
        for text in [
            "👨\u{200D}💻 dev",
            "⚠\u{FE0F} warning",
            "می\u{200C}خواهم",
            "hy\u{AD}phen",
            "zero\u{200B}width",
            "one\ntwo",
        ] {
            assert!(matches!(escape_prose(text), Cow::Borrowed(_)), "{text:?}");
        }
        let cases = [
            ("a\u{1b}[2Jb", "a\\u{1b}[2Jb"),
            ("a\rb", "a\\rb"),
            ("a\tb", "a\\tb"),
            ("a\u{7f}b", "a\\u{7f}b"),
            ("a\u{9b}b", "a\\u{9b}b"),
            ("a\u{202E}b", "a\\u{202e}b"),
            ("a\u{2067}b", "a\\u{2067}b"),
            ("a\u{200F}b", "a\\u{200f}b"),
            ("a\u{61C}b", "a\\u{61c}b"),
            ("a\u{2028}b", "a\\u{2028}b"),
            ("a\u{E0041}b", "a\\u{e0041}b"),
        ];
        for (text, shown) in cases {
            assert_eq!(escape_prose(text), shown, "{text:?}");
        }
    }

    // The printer's guard keeps selfie's own styling only when the stream has
    // colors, removes it otherwise, and escapes every other sequence either way:
    // conceal, a background, black, white, the bright colors, and a sequence
    // with two parameters.
    #[test]
    fn the_guard_keeps_or_removes_only_selfies_own_styling() {
        let own = console::style("x")
            .red()
            .bold()
            .force_styling(true)
            .to_string();
        let foreign = "\u{1b}[2J\u{1b}[8mhidden\u{1b}[48;5;0m\u{1b}]0;t\u{7}\u{1b}[30m\u{1b}[37m\u{1b}[90m\u{1b}[31;1m";
        let escaped = "\\u{1b}[2J\\u{1b}[8mhidden\\u{1b}[48;5;0m\\u{1b}]0;t\\u{7}\\u{1b}[30m\\u{1b}[37m\\u{1b}[90m\\u{1b}[31;1m";
        let block = format!("{own}\n{foreign}");

        assert_eq!(guard(&block, true), format!("{own}\n{escaped}"));
        assert_eq!(guard(&block, false), format!("x\n{escaped}"));
    }

    // Painting writes selfie's styling whatever console thinks of stdout; the
    // printer then keeps or removes it for the stream it writes to.
    #[test]
    fn paint_writes_styling_only_when_colors_are_on() {
        let painted = Shown::name("x").paint(true, |s| s.red());
        assert_eq!(painted.as_str(), "\u{1b}[31mx\u{1b}[0m");
        assert_eq!(Shown::name("x").paint(false, |s| s.red()), "x");
    }

    // A command's output loses the blank space around it, as a trimmed string
    // does, and keeps the line breaks inside it.
    #[test]
    fn tool_output_trims_the_blank_space_around_it() {
        assert_eq!(tool_output("\n  1.2.3\n"), "1.2.3");
        assert_eq!(tool_output("\n\none\n  two\n\n"), "one\n  two");
    }

    // A block the library laid out is escaped and gains no indentation.
    #[test]
    fn a_block_keeps_its_own_layout() {
        assert_eq!(Shown::block("  a\n  b\u{1b}"), "  a\n  b\\u{1b}");
    }

    // `shown!` refuses a format that names an argument inline, which would
    // bypass the check that every argument is Trusted.
    #[test]
    fn a_format_naming_an_argument_inline_is_found() {
        assert!(has_inline_argument("{name}"));
        assert!(has_inline_argument("a {x:>4} b"));
        assert!(has_inline_argument("{_x}"));
        assert!(!has_inline_argument("{} {:>4} {0}"));
        assert!(!has_inline_argument("{{name}} {}"));
    }

    // Escaped text has nothing left to escape, so a path escaped as a
    // DisplayPath and again by a printer reads the same.
    #[test]
    fn escaping_twice_is_escaping_once() {
        let once = escape("a\u{1b}[2J\u{202E}\u{FE0F}\tb").into_owned();
        assert_eq!(escape(&once), once);
        assert_eq!(escape_prose(&once), once);
        assert_eq!(guard(&once, false), once);
    }
}
