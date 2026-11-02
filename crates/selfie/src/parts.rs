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
}

/// A `selfie` subcommand a sentence offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subcommand {
    /// `selfie apply`.
    Apply,
}

impl Subcommand {
    // The subcommand as it is typed, such as `apply`.
    fn word(self) -> &'static str {
        match self {
            Self::Apply => "apply",
        }
    }
}

/// What a suggested command runs, before its operand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Program {
    /// `selfie` and a subcommand.
    Selfie(Subcommand),
    /// `mkdir -p --`, which creates a directory and every directory above it.
    MakeDirectory,
}

impl fmt::Display for Program {
    /// Writes the words before the operand: `selfie apply`, `mkdir -p --`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Selfie(subcommand) => write!(f, "selfie {}", subcommand.word()),
            Self::MakeDirectory => f.write_str("mkdir -p --"),
        }
    }
}

/// A command a sentence suggests the user run: a [`Program`] and one operand,
/// such as a spec name or a path, held as the shell words that pass it.
///
/// Its operand always has those words, so the command can always be written
/// out to run as shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuggestedCommand {
    program: Program,
    word: String,
}

impl SuggestedCommand {
    /// The command running `subcommand` on `operand`, quoted as one shell word
    /// after `--` when it starts with a dash, or `None` when `operand` holds
    /// NUL, which no shell word can carry.
    #[must_use]
    pub fn selfie(subcommand: Subcommand, operand: &str) -> Option<Self> {
        crate::fs::shell_operand(operand).map(|word| Self {
            program: Program::Selfie(subcommand),
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

    /// The shell words that pass the operand, as a command line shows them.
    #[must_use]
    pub fn word(&self) -> &str {
        &self.word
    }
}

impl fmt::Display for SuggestedCommand {
    /// Writes the program's words, then the operand's.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.program, self.word)
    }
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
        let text = cause.to_string();
        self.text(text.trim_end_matches(['\r', '\n']).to_string())
    }

    pub(crate) fn name(mut self, name: impl Into<Cow<'a, str>>) -> Self {
        self.0.push(Part::Name(name.into()));
        self
    }

    pub(crate) fn path(mut self, path: impl Into<Cow<'a, Path>>) -> Self {
        self.0.push(Part::Path(path.into()));
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

    pub(crate) fn append(mut self, other: Parts<'a>) -> Self {
        self.0.extend(other.0);
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
    /// Writes each part as it is, a path through [`Path::display`] and a
    /// command as [`SuggestedCommand`] writes it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for part in self {
            match part {
                Part::Text(text) => f.write_str(text)?,
                Part::Name(name) => f.write_str(name)?,
                Part::Path(path) => write!(f, "{}", path.display())?,
                Part::Command(command) => write!(f, "{command}")?,
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

#[cfg(test)]
mod tests {
    use super::*;

    // The parts of `parts`, in order, for comparing.
    fn listed<'a>(parts: &Parts<'a>) -> Vec<Part<'a>> {
        parts.into_iter().cloned().collect()
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
        let apply = |name: &str| {
            SuggestedCommand::selfie(Subcommand::Apply, name).map(|command| command.to_string())
        };
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
}
