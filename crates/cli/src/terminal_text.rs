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
//! What a text is decides how much is escaped, and each kind of text is a type
//! that holds its escaped form: a [`Name`] or a [`DisplayPath`] escapes
//! strictly, since an invisible character in it makes it read as another; a
//! [`Content`] line of a file keeps its tabs; [`Prose`] and a tool's
//! [`Output`] escape only what can act on the terminal or reorder the text, so
//! emoji sequences and joiners render. selfie's own text is a [`Literal`].

use std::borrow::Cow;
use std::fmt;

use icu_properties::props::{DefaultIgnorableCodePoint, GeneralCategory};
use icu_properties::{CodePointMapData, CodePointSetData};

// The guard set: what can act on the terminal or reorder the text around it.
// Controls other than a newline and a tab, the line and paragraph separators,
// the bidi controls and the tag characters. Joiners, variation selectors and
// soft hyphens are left to render, so an emoji sequence or a Persian word reads
// as written. Each policy decides for itself whether a newline and a tab show.
fn in_guard_set(c: char) -> bool {
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

// The base set: a control character (Cc), a format character (Cf), a line or
// paragraph separator (Zl, Zp), or a default-ignorable code point, which
// renders as nothing. Combining marks are none of these, so an accented name
// stored decomposed, as macOS often stores it, shows as written.
fn in_base_set(c: char) -> bool {
    matches!(
        CodePointMapData::<GeneralCategory>::new().get(c),
        GeneralCategory::Control
            | GeneralCategory::Format
            | GeneralCategory::LineSeparator
            | GeneralCategory::ParagraphSeparator
    ) || CodePointSetData::new::<DefaultIgnorableCodePoint>().contains(c)
}

// The strict set, for names and paths, where two values that read alike are
// two different things.
fn in_strict_set(c: char) -> bool {
    in_base_set(c)
}

// One variant per distinct escape. `Name` and `DisplayPath` share `Strict`.
#[derive(Clone, Copy)]
enum Policy {
    // The strict set, a newline and a tab included, so the text stays on one
    // line.
    Strict,
    // The base set except a tab: one line of file content.
    Content,
    // The guard set and a tab; a newline stays.
    Prose,
    // One line of a tool's output, cleaned as a terminal would leave it, then
    // the guard set.
    Output,
    // The guard set, a newline and a tab kept, with selfie's own SGR sequences
    // handled as the `Sgr` says.
    Guard(Sgr),
    // The base set, a newline and a tab included: what a command or a step's
    // description has always been escaped with, until a kind for each exists.
    Interim,
}

// The one escaping function; the kind constructors, `guard` and
// `interim_one_line` are its only callers.
fn escape(text: &str, policy: Policy) -> Cow<'_, str> {
    match policy {
        Policy::Strict => escape_with(text, in_strict_set, Sgr::Escape),
        Policy::Content => escape_with(text, |c| c != '\t' && in_base_set(c), Sgr::Escape),
        Policy::Prose => escape_with(text, |c| c == '\t' || in_guard_set(c), Sgr::Escape),
        Policy::Output => Cow::Owned(cleaned(text)),
        Policy::Guard(sgr) => escape_with(text, in_guard_set, sgr),
        Policy::Interim => escape_with(text, in_base_set, Sgr::Escape),
    }
}

/// Shows `text`, a command or a step's description, on one line, with every
/// control, format or invisible character written as an escape. A blank such
/// as U+00A0 shows as written, so a command quoting French or CJK text still
/// pastes as it runs.
// Neither is a name, so neither takes the strict set; the Command kind and a
// one-line prose kind replace this.
pub(crate) fn interim_one_line(text: &str) -> Shown {
    Shown(escape(text, Policy::Interim).into_owned())
}

// `line`, one line of a command's output, as a terminal would leave it: its
// colors removed, only the text after its last carriage return kept, tabs as
// spaces, and no other control character left, a line break included, with
// what could still reorder the text written as an escape. Cleaning removes
// every control character, so only the guard set's other members are left to
// escape.
fn cleaned(line: &str) -> String {
    if !line.chars().any(|c| c.is_control() || in_guard_set(c)) {
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
    escape_with(&kept, in_guard_set, Sgr::Escape).into_owned()
}

/// `block`, text a printer is about to write, as the stream gets it. Every
/// part of it from outside selfie must already have been escaped: this is a
/// second guard behind that, which escapes anything that could still act on
/// the terminal except a newline or a tab. selfie's own color and weight
/// sequences are kept when `colors` is on and removed when it is off.
pub(crate) fn guard(block: &str, colors: bool) -> Cow<'_, str> {
    escape(
        block,
        Policy::Guard(if colors { Sgr::Keep } else { Sgr::Strip }),
    )
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
// A style in and a style out, never the styled text, so a paint cannot put
// other text in place of the text it styles.
pub(crate) type Paint = fn(console::Style) -> console::Style;

// `text` styled by `paint`, the styling written whatever the stream.
fn painted(text: String, paint: Paint) -> String {
    paint(console::Style::new().force_styling(true))
        .apply_to(text)
        .to_string()
}

/// Text safe to print: every part of it from outside selfie has been escaped,
/// and only selfie styled it.
///
/// The printers that keep styling take this type, so text from outside selfie
/// reaches them only as one of the kinds, such as a [`Name`] or [`Prose`], or
/// through [`shown!`], whose arguments must each be [`Trusted`]; selfie's own
/// text reaches them as a [`Literal`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Shown(String);

impl Shown {
    /// `items` joined by `separator`, each shown as it is.
    pub(crate) fn join(
        items: impl IntoIterator<Item = impl Into<Shown>>,
        separator: Literal,
    ) -> Self {
        Self(
            items
                .into_iter()
                .map(|item| item.into().0)
                .collect::<Vec<_>>()
                .join(separator.as_str()),
        )
    }

    /// `text` with every line after the first starting with `indent`, so no
    /// line of it starts at the left margin.
    pub(crate) fn indented(text: impl Into<Shown>, indent: Literal) -> Self {
        Self(text.into().0.replace('\n', &format!("\n{indent}")))
    }

    /// `self` styled by `paint` when `use_colors`.
    ///
    /// The styling is written whatever the stream; the printer removes it when
    /// the stream it writes to has colors off.
    #[must_use]
    pub(crate) fn paint(self, use_colors: bool, paint: Paint) -> Self {
        if use_colors {
            Self(painted(self.0, paint))
        } else {
            self
        }
    }

    /// Returns the text, escapes and styling included.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    // Only `shown!` calls this, once it has checked that every argument is
    // `Trusted`. A test holds every other file to that.
    #[doc(hidden)]
    pub(crate) fn __assembled(text: String) -> Self {
        Self(text)
    }
}

impl From<Literal> for Shown {
    fn from(text: Literal) -> Self {
        Self(text.0.to_string())
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

// `Trusted` requires this trait, which only this module can name, so no other
// module can implement `Trusted` for a type of its own. Its name is its own, so
// rustc's note on a refused implementation names no other crate's trait.
mod sealed {
    #[diagnostic::on_unimplemented(
        message = "only terminal_text.rs implements Trusted, and not for `{Self}`"
    )]
    pub trait TerminalTextSeal {}
}

/// A value [`shown!`] may format: text already safe to print, or a number.
///
/// Only this module implements it.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not Trusted, so shown! cannot format it",
    label = "escape it first, or write selfie's own text with lit!"
)]
pub(crate) trait Trusted: fmt::Display + sealed::TerminalTextSeal {}

macro_rules! trusted {
    ($($trusted:ty),*) => { $(impl sealed::TerminalTextSeal for $trusted {} impl Trusted for $trusted {})* };
}
// A `SafeTable` escapes every cell not built from a `Shown`, so a rendered
// table may be printed.
trusted!(Shown, Literal, crate::tables::SafeTable);
trusted!(
    u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128, isize, f32, f64
);
impl<T: Trusted + ?Sized> sealed::TerminalTextSeal for &T {}
impl<T: Trusted + ?Sized> Trusted for &T {}

#[doc(hidden)]
pub(crate) fn trusted<T: Trusted>(value: T) -> T {
    value
}

// Whether every placeholder in `format` is `{}`, with `{{` as a literal brace.
// Any other placeholder can bring in a value `trusted` never saw: `{name}`
// captures a variable, and a width or precision argument such as `{:w$}` or
// `{:.*}` is read without the check. An index such as `{0}` is refused too,
// so the rule stays one a reader can check by eye.
#[doc(hidden)]
pub(crate) const fn only_empty_placeholders(format: &str) -> bool {
    let bytes = format.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            if i + 1 == bytes.len() || (bytes[i + 1] != b'{' && bytes[i + 1] != b'}') {
                return false;
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    true
}

/// Format a [`Shown`] from [`Trusted`] arguments, each placed by a `{}`.
///
/// A format with any other placeholder, such as `{name}`, `{0}`, `{:>4}` or
/// `{:.2}`, does not compile. `{{` and `}}` are literal braces.
macro_rules! shown {
    ($format:literal $(, $argument:expr)* $(,)?) => {{
        const _: () = assert!(
            $crate::terminal_text::only_empty_placeholders($format),
            "shown! takes only {{}} placeholders, so each argument is checked as Trusted"
        );
        $crate::terminal_text::Shown::__assembled(format!(
            $format $(, $crate::terminal_text::trusted(&$argument))*
        ))
    }};
}
pub(crate) use shown;

/// Holds selfie's own text, which is shown as it is written. Only [`lit!`]
/// makes one, from a string literal in selfie's source. The printer's guard
/// still escapes a control character or a color sequence in it that selfie's
/// own styling does not write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Literal(&'static str);

impl Literal {
    // Only `lit!` calls this, with a string-literal token: a `&'static str`
    // can also come from `String::leak`. A test holds every other file to
    // that.
    #[doc(hidden)]
    pub(crate) const fn __from_literal(text: &'static str) -> Self {
        Self(text)
    }

    /// Returns the text.
    pub(crate) const fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for Literal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// Makes a [`Literal`] from one string literal, and works in a `const`.
///
/// Anything but a string literal does not compile: a variable or a macro call
/// gets this macro's own error, and a literal of another type, such as `1` or
/// `b"x"`, gets rustc's type error.
macro_rules! lit {
    ($text:literal) => {
        $crate::terminal_text::Literal::__from_literal($text)
    };
    ($($other:tt)*) => {
        compile_error!("lit! takes one string literal")
    };
}
pub(crate) use lit;

// Each kind holds its escaped text, and only its constructors, below, build
// one. Each is `Trusted`, so `shown!` formats it, and becomes a `Shown` as it
// is.
macro_rules! kinds {
    ($($(#[$meta:meta])* $kind:ident;)*) => {$(
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
        pub(crate) struct $kind(String);

        impl $kind {
            /// Returns the escaped text.
            #[allow(dead_code, reason = "not every kind's text is read outside tests")]
            pub(crate) fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $kind {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        trusted!($kind);

        impl From<$kind> for Shown {
            fn from(text: $kind) -> Self {
                Self(text.0)
            }
        }
    )*};
}

kinds! {
    /// Holds a spec, package, environment or file-stem name, or a list item, on
    /// one line, with every invisible or controlling character shown as an
    /// escape.
    Name;
    /// Holds a path or a dotfile source, escaped as a [`Name`] is.
    // Escaped rather than dropped: dropping would show `evil\rgood` as the name
    // of a real file `evilgood`. Two names can still read alike in two ways,
    // both accepted: a backslash is not escaped, so a name holding the six
    // characters `\u{1b}` reads like one holding ESC, and `DisplayPath::of`
    // shows a path that is not UTF-8 lossily, so two such names can show the
    // same replacement characters.
    DisplayPath;
    /// Holds a library sentence, description, note or refusal reason. What
    /// could act on the terminal or reorder the text is escaped, a tab
    /// included, and its line breaks are kept.
    Prose;
    /// Holds one line of file content, as in a diff. A control, format or
    /// invisible character shows as an escape, a tab stays a tab, and a blank
    /// such as U+00A0 shows as written.
    Content;
    /// Holds what a tool printed, as a terminal would leave it: its colors are
    /// removed, a line redrawn by carriage returns shows its final state, tabs
    /// become spaces, no control character is left but the line breaks between
    /// its lines, and what could reorder the text is escaped.
    Output;
}

impl Name {
    /// Escapes `text`.
    pub(crate) fn new(text: &str) -> Self {
        Self(escape(text, Policy::Strict).into_owned())
    }

    /// Whether `text` shows as written when escaped as a name: nothing in it
    /// acts on the terminal, renders as nothing, or reads as another name.
    pub(crate) fn is_plain(text: &str) -> bool {
        matches!(escape(text, Policy::Strict), Cow::Borrowed(_))
    }
}

impl DisplayPath {
    /// Escapes `text`.
    pub(crate) fn new(text: &str) -> Self {
        Self(escape(text, Policy::Strict).into_owned())
    }

    /// Shows `path`'s text as [`DisplayPath::new`] does.
    pub(crate) fn of(path: &std::path::Path) -> Self {
        // The one place outside tests a path's text is taken for display; the
        // gate in tests/no_raw_path_display.rs holds every other one to it.
        Self::new(&path.display().to_string())
    }
}

impl Prose {
    /// Escapes `text`.
    pub(crate) fn new(text: &str) -> Self {
        Self(escape(text, Policy::Prose).into_owned())
    }
}

impl Content {
    /// Escapes `line`.
    pub(crate) fn new(line: &str) -> Self {
        Self(escape(line, Policy::Content).into_owned())
    }
}

impl Output {
    /// Cleans `text`, a tool's whole output, line by line, keeping its line
    /// breaks and trimming the blank space around it.
    pub(crate) fn new(text: &str) -> Self {
        Self(
            text.trim()
                .lines()
                .map(|line| escape(line, Policy::Output).into_owned())
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }

    /// Cleans `line`, one line of a tool's output as it streams, without
    /// trimming it. A line break inside it is removed.
    pub(crate) fn line(line: &str) -> Self {
        Self(escape(line, Policy::Output).into_owned())
    }
}

// The one escaping loop. A character `escaped` names is written as an escape;
// an SGR sequence selfie's own styling writes is kept, removed or escaped as
// `sgr` says.
fn escape_with(text: &str, escaped: impl Fn(char) -> bool, sgr: Sgr) -> Cow<'_, str> {
    if !text.chars().any(&escaped) {
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

#[cfg(test)]
mod tests {
    use super::*;

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
            ('\u{A0}', false, false, false),
            ('\u{1680}', false, false, false),
            ('\u{2000}', false, false, false),
            ('\u{200A}', false, false, false),
            ('\u{202F}', false, false, false),
            ('\u{205F}', false, false, false),
            ('\u{3000}', false, false, false),
            ('\u{2800}', false, false, false),
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
    // what a name or a line of content escapes.
    #[test]
    fn the_sets_nest() {
        for c in (0..=u32::from(char::MAX)).filter_map(char::from_u32) {
            assert!(!in_guard_set(c) || in_base_set(c), "{c:?}");
            assert!(!in_base_set(c) || in_strict_set(c), "{c:?}");
        }
    }

    // Each kind against each class of character, as (text, name, prose,
    // content, output, guarded without colors, guarded with them). A path
    // escapes as a name does.
    #[test]
    fn each_kind_escapes_each_class() {
        let own = "\u{1b}[31mx\u{1b}[0m";
        let rows = [
            (
                "a\u{1b}[2Jb",
                "a\\u{1b}[2Jb",
                "a\\u{1b}[2Jb",
                "a\\u{1b}[2Jb",
                "ab",
                "a\\u{1b}[2Jb",
                "a\\u{1b}[2Jb",
            ),
            ("a\rb", "a\\rb", "a\\rb", "a\\rb", "b", "a\\rb", "a\\rb"),
            ("a\nb", "a\\nb", "a\nb", "a\\nb", "ab", "a\nb", "a\nb"),
            ("a\tb", "a\\tb", "a\\tb", "a\tb", "a b", "a\tb", "a\tb"),
            (
                "a\u{7f}b",
                "a\\u{7f}b",
                "a\\u{7f}b",
                "a\\u{7f}b",
                "ab",
                "a\\u{7f}b",
                "a\\u{7f}b",
            ),
            (
                "a\u{85}b",
                "a\\u{85}b",
                "a\\u{85}b",
                "a\\u{85}b",
                "ab",
                "a\\u{85}b",
                "a\\u{85}b",
            ),
            (
                "a\u{202E}b",
                "a\\u{202e}b",
                "a\\u{202e}b",
                "a\\u{202e}b",
                "a\\u{202e}b",
                "a\\u{202e}b",
                "a\\u{202e}b",
            ),
            (
                "a\u{2066}b",
                "a\\u{2066}b",
                "a\\u{2066}b",
                "a\\u{2066}b",
                "a\\u{2066}b",
                "a\\u{2066}b",
                "a\\u{2066}b",
            ),
            (
                "a\u{61C}b",
                "a\\u{61c}b",
                "a\\u{61c}b",
                "a\\u{61c}b",
                "a\\u{61c}b",
                "a\\u{61c}b",
                "a\\u{61c}b",
            ),
            (
                "a\u{E0041}b",
                "a\\u{e0041}b",
                "a\\u{e0041}b",
                "a\\u{e0041}b",
                "a\\u{e0041}b",
                "a\\u{e0041}b",
                "a\\u{e0041}b",
            ),
            (
                "a\u{2028}b",
                "a\\u{2028}b",
                "a\\u{2028}b",
                "a\\u{2028}b",
                "a\\u{2028}b",
                "a\\u{2028}b",
                "a\\u{2028}b",
            ),
            (
                "a\u{200B}b",
                "a\\u{200b}b",
                "a\u{200B}b",
                "a\\u{200b}b",
                "a\u{200B}b",
                "a\u{200B}b",
                "a\u{200B}b",
            ),
            (
                "a\u{200D}b",
                "a\\u{200d}b",
                "a\u{200D}b",
                "a\\u{200d}b",
                "a\u{200D}b",
                "a\u{200D}b",
                "a\u{200D}b",
            ),
            (
                "a\u{FE0F}b",
                "a\\u{fe0f}b",
                "a\u{FE0F}b",
                "a\\u{fe0f}b",
                "a\u{FE0F}b",
                "a\u{FE0F}b",
                "a\u{FE0F}b",
            ),
            (
                "a\u{FEFF}b",
                "a\\u{feff}b",
                "a\u{FEFF}b",
                "a\\u{feff}b",
                "a\u{FEFF}b",
                "a\u{FEFF}b",
                "a\u{FEFF}b",
            ),
            (
                "a\u{AD}b",
                "a\\u{ad}b",
                "a\u{AD}b",
                "a\\u{ad}b",
                "a\u{AD}b",
                "a\u{AD}b",
                "a\u{AD}b",
            ),
            (
                "a\u{A0}b", "a\u{A0}b", "a\u{A0}b", "a\u{A0}b", "a\u{A0}b", "a\u{A0}b", "a\u{A0}b",
            ),
            (
                "a\u{2800}b",
                "a\u{2800}b",
                "a\u{2800}b",
                "a\u{2800}b",
                "a\u{2800}b",
                "a\u{2800}b",
                "a\u{2800}b",
            ),
            (
                "a\u{3000}b",
                "a\u{3000}b",
                "a\u{3000}b",
                "a\u{3000}b",
                "a\u{3000}b",
                "a\u{3000}b",
                "a\u{3000}b",
            ),
            (
                "cafe\u{301}",
                "cafe\u{301}",
                "cafe\u{301}",
                "cafe\u{301}",
                "cafe\u{301}",
                "cafe\u{301}",
                "cafe\u{301}",
            ),
            (
                own,
                "\\u{1b}[31mx\\u{1b}[0m",
                "\\u{1b}[31mx\\u{1b}[0m",
                "\\u{1b}[31mx\\u{1b}[0m",
                "x",
                "x",
                own,
            ),
        ];
        for (text, name, prose, content, output, plain, colored) in rows {
            let shown = (
                Name::new(text).as_str().to_owned(),
                DisplayPath::new(text).as_str().to_owned(),
                Prose::new(text).as_str().to_owned(),
                Content::new(text).as_str().to_owned(),
                Output::line(text).as_str().to_owned(),
                guard(text, false).into_owned(),
                guard(text, true).into_owned(),
            );
            let expected = (name, name, prose, content, output, plain, colored);
            assert_eq!(
                (
                    shown.0.as_str(),
                    shown.1.as_str(),
                    shown.2.as_str(),
                    shown.3.as_str(),
                    shown.4.as_str(),
                    shown.5.as_str(),
                    shown.6.as_str()
                ),
                expected,
                "{text:?}"
            );
        }
    }

    // A command or a step's description is not a name: a blank in it shows as
    // written, so a command pastes as it runs, and a line break or an invisible
    // character still shows as an escape.
    #[test]
    fn a_command_keeps_its_blanks_and_stays_on_one_line() {
        assert_eq!(
            interim_one_line("echo 'a\u{A0}b'\n\u{200B}x").as_str(),
            "echo 'a\u{A0}b'\\n\\u{200b}x"
        );
    }

    // A name shows as written exactly when escaping leaves it unchanged.
    #[test]
    fn a_plain_name_is_one_escaping_leaves_unchanged() {
        for text in ["plain-name", "café", "e\u{301}", "a b"] {
            assert!(Name::is_plain(text), "{text:?}");
        }
        for text in ["a\u{200B}b", "a\nb", "a\u{1b}b"] {
            assert!(!Name::is_plain(text), "{text:?}");
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

        let shown: std::collections::BTreeSet<Name> =
            names.iter().map(|name| Name::new(name)).collect();

        assert_eq!(shown.len(), names.len(), "{shown:?}");
    }

    // A tool's whole output loses the blank space around it and keeps the line
    // breaks and indentation inside it. One streamed line is not trimmed, and a
    // line break inside it is removed.
    #[test]
    fn output_trims_a_whole_output_and_not_a_line() {
        assert_eq!(Output::new("\n  1.2.3\n").as_str(), "1.2.3");
        assert_eq!(Output::new("\n\none\n  two\n\n").as_str(), "one\n  two");
        assert_eq!(Output::line("  indented").as_str(), "  indented");
        assert_eq!(Output::line("a\nb").as_str(), "ab");
    }

    // A redrawn line whose last part is blank shows its last part with text, so
    // a progress meter ending in a bare carriage return shows its final state.
    #[test]
    fn a_redrawn_line_shows_its_last_state() {
        assert_eq!(Output::line("10%\r50%\r100%\r").as_str(), "100%");
        assert_eq!(Output::line("old\rnew").as_str(), "new");
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
        let painted = Shown::from(Name::new("x")).paint(true, |s| s.red());
        assert_eq!(painted.as_str(), "\u{1b}[31mx\u{1b}[0m");
        assert_eq!(Shown::from(Name::new("x")).paint(false, |s| s.red()), "x");
    }

    // Prose the library laid out is escaped and gains no indentation.
    #[test]
    fn prose_keeps_its_own_layout() {
        assert_eq!(Prose::new("  a\n  b\u{1b}").as_str(), "  a\n  b\\u{1b}");
    }

    // `shown!` accepts only `{}`: any other placeholder can capture a value
    // that never passed the check that every argument is Trusted.
    #[test]
    fn only_empty_placeholders_are_accepted() {
        let cases = [
            ("", true),
            ("plain", true),
            ("{}", true),
            ("a {} b {}", true),
            ("{{}}", true),
            ("{{name}} {}", true),
            ("}}{{", true),
            ("{{{}}}", true),
            ("{name}", false),
            ("{_x}", false),
            ("{é}", false),
            ("{0}", false),
            ("{:>4}", false),
            ("{:w$}", false),
            ("{:.2}", false),
            ("{:?}", false),
            ("{ }", false),
            ("{{{x}}}", false),
            ("a {", false),
        ];
        for (format, accepted) in cases {
            assert_eq!(only_empty_placeholders(format), accepted, "{format:?}");
        }
    }

    // Escaped text has nothing left to escape, so text escaped as one kind and
    // again as another, or by a printer, reads the same.
    #[test]
    fn escaping_twice_is_escaping_once() {
        let once = Name::new("a\u{1b}[2J\u{202E}\u{FE0F}\tb\u{A0}\u{2800}");
        let once = once.as_str();
        assert_eq!(Name::new(once).as_str(), once);
        assert_eq!(DisplayPath::new(once).as_str(), once);
        assert_eq!(Prose::new(once).as_str(), once);
        assert_eq!(Content::new(once).as_str(), once);
        assert_eq!(Output::line(once).as_str(), once);
        assert_eq!(guard(once, false), once);
    }
}
