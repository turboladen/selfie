//! A library sentence as typed parts, so a caller can tell selfie's words from
//! the values they name.
//!
//! A [`Composed`] value's sentence is its [`parts`](Composed::parts), and its
//! `Display` writes those parts as they are. A caller that shows the sentence to
//! a person can instead render each part by what it is, such as escaping a name
//! as a name wherever it appears.

use std::borrow::Cow;
use std::fmt;
use std::path::Path;

/// One piece of a library sentence: selfie's words, or a value they name.
///
/// A part says what a value is, never how to show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part<'a> {
    /// selfie's words, a count it formats, or text from outside selfie read as
    /// a sentence, such as an I/O error's message.
    Text(Cow<'a, str>),
    /// An environment, package or spec name.
    Name(Cow<'a, str>),
    /// A path, including one spelled as a spec spells it, such as `~/.bashrc`.
    Path(Cow<'a, Path>),
    /// A command for the user to run.
    Command(SuggestedCommand),
    /// Text another program or library wrote, such as git's error, which can
    /// quote a path or a name selfie cannot pull out as a part of its own.
    Foreign(ForeignText<'a>),
    /// A command a spec's author wrote, such as an install or check command.
    SpecCommand(SpecCommandText),
}

/// A command a spec's author wrote, as a sentence carries it: shell text of any
/// shape, bounded to the length a diagnostic keeps of it.
///
/// Built only by this module's builders, so every one is bounded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecCommandText(String);

impl SpecCommandText {
    // `command`, keeping both its ends and eliding its middle when it is longer
    // than `BoundedText` keeps.
    fn of(command: &str) -> Self {
        Self(crate::commands::BoundedText::bound(command.as_bytes()).into_string())
    }

    // `command`, bounded to its `share` of a sentence's bound.
    fn within(command: &str, share: usize) -> Self {
        Self(crate::commands::BoundedText::bound_within(command.as_bytes(), share).into_string())
    }

    /// `command`, bounded as selfie bounds a spec's command, for building
    /// messages in tests.
    #[cfg(any(test, feature = "with_mocks"))]
    #[must_use]
    pub fn for_tests(command: &str) -> Self {
        Self::of(command)
    }

    /// The command, as bounded.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Text another program or library wrote, as a sentence carries it.
///
/// Built only by this module's builders, each from the one source that has
/// already guarded its text, such as a redacted git message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignText<'a>(Cow<'a, str>);

impl<'a> ForeignText<'a> {
    /// `text`, as another program's text, for building messages in tests.
    #[cfg(any(test, feature = "with_mocks"))]
    #[must_use]
    pub fn for_tests(text: impl Into<Cow<'a, str>>) -> Self {
        Self(text.into())
    }

    /// The text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn into_owned(self) -> ForeignText<'static> {
        ForeignText(Cow::Owned(self.0.into_owned()))
    }

    fn borrowed(&self) -> ForeignText<'_> {
        ForeignText(Cow::Borrowed(&self.0))
    }
}

impl fmt::Display for ForeignText<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A `selfie` subcommand a sentence offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subcommand {
    /// `selfie apply`.
    Apply,
    /// `selfie package install`.
    PackageInstall,
    /// `selfie package track-dotfile`.
    PackageTrackDotfile,
}

impl Subcommand {
    // The subcommand as it is typed, such as `apply`.
    fn word(self) -> &'static str {
        match self {
            Self::Apply => "apply",
            Self::PackageInstall => "package install",
            Self::PackageTrackDotfile => "package track-dotfile",
        }
    }
}

/// What a suggested command runs, before its operand or operands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Program {
    /// `selfie` and a subcommand.
    Selfie(Subcommand),
    /// `mkdir -p --`, which creates a directory and every directory above it.
    MakeDirectory,
}

impl fmt::Display for Program {
    /// Writes the words before the operand or operands: `selfie apply`,
    /// `mkdir -p --`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Selfie(subcommand) => write!(f, "selfie {}", subcommand.word()),
            Self::MakeDirectory => f.write_str("mkdir -p --"),
        }
    }
}

/// A command a sentence suggests the user run: a [`Program`] and its one or
/// two operands, such as a spec name or a path, held as the shell words that
/// pass them.
///
/// Its operands always have those words, so the command can always be written
/// out to run as shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuggestedCommand {
    program: Program,
    word: String,
}

impl SuggestedCommand {
    /// The command running `selfie apply` on `operand`, quoted as one shell word
    /// after `--` when it starts with a dash, or `None` when `operand` holds
    /// NUL, which no shell word can carry.
    #[must_use]
    pub fn apply(operand: &str) -> Option<Self> {
        crate::fs::shell_operand(operand).map(|word| Self {
            program: Program::Selfie(Subcommand::Apply),
            word,
        })
    }

    /// The command running `selfie package install` on the package `name`,
    /// quoted as one shell word after `--` when it starts with a dash, or
    /// `None` when `name` holds NUL.
    #[must_use]
    pub fn install(name: &str) -> Option<Self> {
        crate::fs::shell_operand(name).map(|word| Self {
            program: Program::Selfie(Subcommand::PackageInstall),
            word,
        })
    }

    /// The command tracking `target` into the package `spec`, each quoted as one
    /// shell word, after one `--` when either starts with a dash, or `None` when
    /// either holds NUL.
    #[must_use]
    pub fn track_dotfile(spec: &str, target: &str) -> Option<Self> {
        crate::fs::shell_operands(&[spec, target]).map(|word| Self {
            program: Program::Selfie(Subcommand::PackageTrackDotfile),
            word,
        })
    }

    /// The command creating the directory at `path`, quoted as shell words
    /// that expand to it with a leading `~/` left outside the quotes, or `None`
    /// when `path` holds NUL.
    #[must_use]
    pub fn make_directory(path: &Path) -> Option<Self> {
        // `--` ends mkdir's option list. Without it a directory named `-p` or
        // `-foo` is read as options, so the command fails or creates something
        // the user did not ask for.
        crate::fs::shell_quote(path).map(|word| Self {
            program: Program::MakeDirectory,
            word,
        })
    }

    /// What it runs.
    #[must_use]
    pub fn program(&self) -> Program {
        self.program
    }

    /// The shell words that pass the operands, as a command line shows them.
    #[must_use]
    pub fn word(&self) -> &str {
        &self.word
    }
}

impl fmt::Display for SuggestedCommand {
    /// Writes the program's words, then the words of its operand or operands.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.program, self.word)
    }
}

// A cause's own text, without the line breaks at its end, for a sentence that
// goes on after it.
pub(crate) fn cause_words(cause: &impl fmt::Display) -> String {
    let text = cause.to_string();
    let trimmed = text.trim_end_matches(['\r', '\n']).len();
    let mut text = text;
    text.truncate(trimmed);
    text
}

// Splits `budget` bytes among values of these lengths, for a sentence that
// names several unbounded values and is bounded as a whole. A value within an
// equal share of what the shorter ones left keeps its whole length, and the
// longer ones split the rest equally.
pub(crate) fn shares<const N: usize>(lengths: [usize; N], budget: usize) -> [usize; N] {
    let mut order: [usize; N] = std::array::from_fn(|index| index);
    order.sort_by_key(|&index| lengths[index]);
    let mut shares = lengths;
    let mut left = budget;
    for (done, &index) in order.iter().enumerate() {
        shares[index] = lengths[index].min(left / (N - done));
        left -= shares[index];
    }
    shares
}

/// A sentence, as its parts in order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parts<'a>(Vec<Part<'a>>);

impl<'a> Parts<'a> {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    // An empty text adds nothing, so a clause said only when it applies can be
    // appended unconditionally.
    pub(crate) fn text(mut self, text: impl Into<Cow<'a, str>>) -> Self {
        let text = text.into();
        if !text.is_empty() {
            self.0.push(Part::Text(text));
        }
        self
    }

    // A cause's own text, embedded in a sentence, without the line breaks at
    // its end: the sentence goes on after it.
    pub(crate) fn cause(self, cause: impl fmt::Display) -> Self {
        self.text(cause_words(&cause))
    }

    pub(crate) fn name(mut self, name: impl Into<Cow<'a, str>>) -> Self {
        self.0.push(Part::Name(name.into()));
        self
    }

    pub(crate) fn path(mut self, path: impl Into<Cow<'a, Path>>) -> Self {
        self.0.push(Part::Path(path.into()));
        self
    }

    // A command a spec's author wrote. It is bounded here, so a sentence that
    // names one stays a diagnostic's length however long the command is.
    pub(crate) fn spec_command(mut self, command: &str) -> Self {
        self.0.push(Part::SpecCommand(SpecCommandText::of(command)));
        self
    }

    // `spec_command`, `path` and `cause` for a sentence that names several
    // unbounded values, each bounded to the share of the sentence's bound
    // that `shares` gave it.
    pub(crate) fn spec_command_within(mut self, command: &str, share: usize) -> Self {
        self.0
            .push(Part::SpecCommand(SpecCommandText::within(command, share)));
        self
    }

    // A path that fits its share and is valid UTF-8 stays a path. One cut to
    // its share, or decoded lossily, names no file any more, so it enters as a
    // name, which claims nothing about the file system.
    pub(crate) fn path_within(self, path: &'a Path, share: usize) -> Self {
        let bytes = path.as_os_str().as_encoded_bytes();
        if bytes.len() <= share && path.to_str().is_some() {
            return self.path(path);
        }
        // Decoded before it is cut: each byte that is not UTF-8 decodes to a
        // three-byte U+FFFD, so cutting the bytes first would show up to three
        // times the share.
        let decoded = path.to_string_lossy();
        let bounded = crate::commands::BoundedText::text_within(&decoded, share);
        self.name(bounded.into_string())
    }

    // `cause` is `cause_words`' text, measured for its share as it is.
    pub(crate) fn cause_within(self, cause: &str, share: usize) -> Self {
        let bounded = crate::commands::BoundedText::bound_within(cause.as_bytes(), share);
        self.text(bounded.into_string())
    }

    // An error the tempfile crate built, whose message ends with the temporary
    // file's path. It is the one source of a foreign part outside git, which
    // `TempfileText`'s constructor, private to the file system module, holds.
    pub(crate) fn tempfile(mut self, error: crate::fs::filesystem::TempfileText<'_>) -> Self {
        self.0
            .push(Part::Foreign(ForeignText(Cow::Owned(error.text()))));
        self
    }

    // Git's own text, which `GitMessage` has already redacted and bounded. It is
    // the one source of a foreign part from the git side, so a credential in
    // git's stderr cannot reach a part without passing through the redaction.
    pub(crate) fn git(mut self, text: &'a crate::git::GitMessage) -> Self {
        self.0
            .push(Part::Foreign(ForeignText(Cow::Borrowed(text.as_str()))));
        self
    }

    // The parts `part` gives for each of `items`, with `separator` between
    // two of them.
    pub(crate) fn joined<T>(
        items: impl IntoIterator<Item = T>,
        separator: &'a str,
        part: impl Fn(T) -> Parts<'a>,
    ) -> Self {
        items
            .into_iter()
            .enumerate()
            .fold(Self::new(), |joined, (index, item)| {
                let joined = if index == 0 {
                    joined
                } else {
                    joined.text(separator)
                };
                joined.append(part(item))
            })
    }

    pub(crate) fn command(mut self, command: SuggestedCommand) -> Self {
        self.0.push(Part::Command(command));
        self
    }

    // The same parts with one `suffix` taken off the end of the last part, when
    // that part is a text ending in it. A text left empty is dropped, as `text`
    // never adds one.
    pub(crate) fn without_final(mut self, suffix: char) -> Self {
        if let Some(Part::Text(text)) = self.0.last_mut()
            && let Some(kept) = text.strip_suffix(suffix)
        {
            if kept.is_empty() {
                self.0.pop();
            } else {
                *text = Cow::Owned(kept.to_string());
            }
        }
        self
    }

    pub(crate) fn append(mut self, other: Parts<'a>) -> Self {
        self.0.extend(other.0);
        self
    }

    // The same parts with the first letter upper case, when they start with
    // selfie's words. A sentence starting with a value keeps it as it is.
    pub(crate) fn capitalized(mut self) -> Self {
        if let Some(Part::Text(text)) = self.0.first_mut() {
            let mut chars = text.chars();
            if let Some(first) = chars.next() {
                *text = Cow::Owned(first.to_uppercase().chain(chars).collect());
            }
        }
        self
    }

    // The same parts, holding their own copies of every value, with each run
    // of adjacent texts merged into one.
    fn into_owned(self) -> Parts<'static> {
        let mut owned: Vec<Part<'static>> = Vec::with_capacity(self.0.len());
        for part in self.0 {
            match (owned.last_mut(), part) {
                (Some(Part::Text(before)), Part::Text(text)) => before.to_mut().push_str(&text),
                (_, part) => owned.push(match part {
                    Part::Text(text) => Part::Text(Cow::Owned(text.into_owned())),
                    Part::Name(name) => Part::Name(Cow::Owned(name.into_owned())),
                    Part::Path(path) => Part::Path(Cow::Owned(path.into_owned())),
                    Part::Command(command) => Part::Command(command),
                    Part::Foreign(text) => Part::Foreign(text.into_owned()),
                    Part::SpecCommand(command) => Part::SpecCommand(command),
                }),
            }
        }
        Parts(owned)
    }

    // The same parts, borrowing every value from these.
    fn borrowed(&self) -> Parts<'_> {
        Parts(
            self.0
                .iter()
                .map(|part| match part {
                    Part::Text(text) => Part::Text(Cow::Borrowed(text)),
                    Part::Name(name) => Part::Name(Cow::Borrowed(name)),
                    Part::Path(path) => Part::Path(Cow::Borrowed(path)),
                    Part::Command(command) => Part::Command(command.clone()),
                    Part::Foreign(text) => Part::Foreign(text.borrowed()),
                    Part::SpecCommand(command) => Part::SpecCommand(command.clone()),
                })
                .collect(),
        )
    }
}

/// A sentence whose parts were fixed when it was built, holding its own copy
/// of every value it names.
// It exists for a sentence an event carries, such as an entry's refusal, which
// has to outlive the values it was built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message(Parts<'static>);

impl From<Parts<'_>> for Message {
    /// Copies every value `parts` names. Adjacent texts become one, so two
    /// messages with the same words and values are equal however each was
    /// built.
    fn from(parts: Parts<'_>) -> Self {
        Self(parts.into_owned())
    }
}

impl Message {
    /// The sentence `value` words, with its own copy of every value it names.
    #[must_use]
    pub fn of(value: &(impl Composed + ?Sized)) -> Self {
        value.parts().into()
    }
}

crate::parts::display_from_parts!(Message);

impl Composed for Message {
    fn parts(&self) -> Parts<'_> {
        self.0.borrowed()
    }
}

// Only tests collect parts: the CLI renders a library value's parts and never
// composes a sentence of its own from them.
#[cfg(any(test, feature = "with_mocks"))]
impl<'a> FromIterator<Part<'a>> for Parts<'a> {
    fn from_iter<I: IntoIterator<Item = Part<'a>>>(parts: I) -> Self {
        Self(parts.into_iter().collect())
    }
}

impl<'p, 'a> IntoIterator for &'p Parts<'a> {
    type Item = &'p Part<'a>;
    type IntoIter = std::slice::Iter<'p, Part<'a>>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl fmt::Display for Parts<'_> {
    /// Writes each part as it is, a path through [`Path::display`], a
    /// command as [`SuggestedCommand`] writes it, and foreign text and a spec's
    /// command raw.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for part in self {
            match part {
                Part::Text(text) => f.write_str(text)?,
                Part::Name(name) => f.write_str(name)?,
                Part::Path(path) => write!(f, "{}", path.display())?,
                Part::Command(command) => write!(f, "{command}")?,
                Part::Foreign(text) => f.write_str(text.as_str())?,
                Part::SpecCommand(command) => f.write_str(command.as_str())?,
            }
        }
        Ok(())
    }
}

/// A library value whose sentence is built from [`Parts`].
///
/// Its `Display` writes [`parts`](Self::parts) and nothing else, so the two
/// always say the same words.
pub trait Composed: fmt::Display {
    /// This value's sentence, as parts.
    fn parts(&self) -> Parts<'_>;
}

// Implements `Display` for a `Composed` type as its parts written raw, so the
// type's sentence has no source but `parts`. A blanket
// `impl<T: Composed> Display for T` would hold every such type to it without
// a macro, but the orphan rule refuses a foreign trait implemented for an
// uncovered type parameter.
// `display_from_parts!(error T)` also implements `std::error::Error`, with no
// source, for a `Composed` type that is an error.
macro_rules! display_from_parts {
    (error $composed:ty) => {
        $crate::parts::display_from_parts!($composed);

        impl std::error::Error for $composed {}
    };
    ($composed:ty) => {
        impl std::fmt::Display for $composed {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                std::fmt::Display::fmt(&$crate::parts::Composed::parts(self), f)
            }
        }
    };
}
pub(crate) use display_from_parts;

/// The names, paths, foreign texts and spec commands `message` names, in
/// order, each as its value's text.
///
/// # Panics
///
/// Panics when a text part of `message` holds U+200B.
// A test's hostile values each hold U+200B, which no word of selfie's does, so a
// value flattened into the sentence's text is not returned and fails the
// assertion instead.
#[cfg(any(test, feature = "with_mocks"))]
#[track_caller]
#[must_use]
pub fn values_of(message: &Message) -> Vec<String> {
    let parts = message.parts();
    parts
        .into_iter()
        .filter_map(|part| match part {
            Part::Text(text) => {
                assert!(!text.contains('\u{200B}'), "{text:?} in {parts:?}");
                None
            }
            Part::Name(name) => Some(name.to_string()),
            Part::Path(path) => Some(path.display().to_string()),
            Part::Foreign(text) => Some(text.to_string()),
            Part::SpecCommand(command) => Some(command.as_str().to_string()),
            Part::Command(_) => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The parts of `parts`, in order, for comparing.
    fn listed<'a>(parts: &Parts<'a>) -> Vec<Part<'a>> {
        parts.into_iter().cloned().collect()
    }

    // Values within an equal share keep their length, and what they leave goes
    // to the longer ones, so the shares never pass the budget.
    #[test]
    fn short_values_leave_their_share_to_long_ones() {
        assert_eq!(shares([10, 5000, 20], 2000), [10, 1970, 20]);
        assert_eq!(shares([5000, 10, 5000], 2000), [995, 10, 995]);
        assert_eq!(shares([10, 20, 30], 2000), [10, 20, 30]);
    }

    // Each part is written as it is, with nothing escaped and nothing between
    // two parts.
    #[test]
    fn parts_are_written_as_they_are() {
        let path = Path::new("/a\nb/c");
        let parts = Parts::new()
            .text("in '")
            .name("a\u{A0}b")
            .text("' at ")
            .path(path);

        assert_eq!(parts.to_string(), "in 'a\u{A0}b' at /a\nb/c");
    }

    // An appended value's parts keep their kinds and their place.
    #[test]
    fn appended_parts_keep_their_kinds_in_place() {
        let inner = Parts::new().text("at ").path(Path::new("/p"));
        let parts = Parts::new().text("x ").append(inner).text(".");

        assert_eq!(
            listed(&parts),
            [
                Part::Text("x ".into()),
                Part::Text("at ".into()),
                Part::Path(Path::new("/p").into()),
                Part::Text(".".into()),
            ]
        );
    }

    // The separator falls between two items and nowhere else, and no items
    // give no parts.
    #[test]
    fn joined_parts_separate_each_pair_of_items() {
        let joined = Parts::joined(["a", "b", "c"], ", ", |name| Parts::new().name(name));
        let none = Parts::joined(Vec::<&str>::new(), ", ", |name| Parts::new().name(name));

        assert_eq!(joined.to_string(), "a, b, c");
        assert_eq!(listed(&none), []);
    }

    // A directory's command quotes the path as the shell expands it, a
    // leading `~/` outside the quotes, after `--`, and a selfie command quotes
    // a name whole. Neither exists for an operand holding NUL.
    #[test]
    fn a_suggested_command_quotes_its_operand_for_its_program() {
        let mkdir = |path: &str| {
            SuggestedCommand::make_directory(Path::new(path)).map(|command| command.to_string())
        };
        let apply = |name: &str| SuggestedCommand::apply(name).map(|command| command.to_string());
        let some = |text: &str| Some(text.to_string());

        assert_eq!(
            mkdir("/home/me/dotfiles"),
            some("mkdir -p -- /home/me/dotfiles")
        );
        assert_eq!(
            mkdir("/home/me/my dotfiles"),
            some("mkdir -p -- '/home/me/my dotfiles'")
        );
        assert_eq!(mkdir("~/my dir"), some("mkdir -p -- ~/'my dir'"));
        assert_eq!(mkdir("-foo"), some("mkdir -p -- -foo"));
        assert_eq!(mkdir("/a\0b"), None);
        assert_eq!(apply("~tool"), some("selfie apply '~tool'"));
        assert_eq!(apply("-x"), some("selfie apply -- -x"));
        assert_eq!(apply("a\0b"), None);
    }

    // A track command quotes each operand as one word, puts one `--` ahead of
    // both when either starts with a dash, and does not exist when either holds
    // NUL.
    #[test]
    fn a_track_command_quotes_each_operand_once() {
        let track = |spec: &str, target: &str| {
            SuggestedCommand::track_dotfile(spec, target).map(|command| command.to_string())
        };
        let some = |text: &str| Some(text.to_string());

        assert_eq!(
            track("bat", "/home/me/.batrc"),
            some("selfie package track-dotfile bat /home/me/.batrc")
        );
        assert_eq!(
            track("bat", "/home/me/my rc"),
            some("selfie package track-dotfile bat '/home/me/my rc'")
        );
        assert_eq!(
            track("-bat", "/x"),
            some("selfie package track-dotfile -- -bat /x")
        );
        assert_eq!(
            track("bat", "-x"),
            some("selfie package track-dotfile -- bat -x")
        );
        assert_eq!(track("b\0at", "/x"), None);
        assert_eq!(track("bat", "/x\0"), None);
    }

    // An empty text adds no part, so an absent clause leaves no trace.
    #[test]
    fn an_empty_text_adds_no_part() {
        let parts = Parts::new().text("").name("n").text(String::new());

        assert_eq!(listed(&parts), [Part::Name("n".into())]);
    }

    // A message keeps each value's kind and place, writes the same words, and
    // holds adjacent texts as one, so how a builder split its words does not
    // make two messages differ.
    #[test]
    fn a_message_owns_its_parts_and_merges_adjacent_texts() {
        let name = String::from("p\u{A0}k");
        let path = std::path::PathBuf::from("/h/t\u{200B}x\ny");
        let parts = Parts::new()
            .text("Skipping '")
            .path(path.as_path())
            .text("' in package '")
            .text(String::from(""))
            .name(name.as_str())
            .text("'")
            .text(".");
        let written = parts.to_string();

        let message = Message::from(parts);
        drop((name, path));

        assert_eq!(
            listed(&message.parts()),
            [
                Part::Text("Skipping '".into()),
                Part::Path(Path::new("/h/t\u{200B}x\ny").into()),
                Part::Text("' in package '".into()),
                Part::Name("p\u{A0}k".into()),
                Part::Text("'.".into()),
            ]
        );
        assert_eq!(message.to_string(), written);
        assert_eq!(
            message,
            Message::from(
                Parts::new()
                    .text("Skipping '")
                    .path(Path::new("/h/t\u{200B}x\ny"))
                    .text("' in package '")
                    .name("p\u{A0}k")
                    .text("'.")
            )
        );
    }

    // One final stop comes off the last text, a text that held only the stop is
    // dropped rather than left empty, and a last part that is not a text is left
    // alone.
    #[test]
    fn without_final_takes_one_stop_off_the_last_text() {
        let path = Path::new("/p.");

        assert_eq!(
            listed(&Parts::new().text("done..").without_final('.')),
            [Part::Text("done.".into())]
        );
        assert_eq!(
            listed(&Parts::new().name("n").text(".").without_final('.')),
            [Part::Name("n".into())]
        );
        assert_eq!(
            listed(&Parts::new().path(path).without_final('.')),
            [Part::Path(path.into())]
        );
    }
}
