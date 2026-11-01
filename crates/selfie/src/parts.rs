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
    Name(&'a str),
    /// A path, including one spelled as a spec spells it, such as `~/.bashrc`.
    Path(&'a Path),
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

    pub(crate) fn name(mut self, name: &'a str) -> Self {
        self.0.push(Part::Name(name));
        self
    }

    pub(crate) fn path(mut self, path: &'a Path) -> Self {
        self.0.push(Part::Path(path));
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

    pub(crate) fn append(mut self, other: Parts<'a>) -> Self {
        self.0.extend(other.0);
        self
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
    /// Writes each part as it is, a path through [`Path::display`].
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for part in self {
            match part {
                Part::Text(text) => f.write_str(text)?,
                Part::Name(name) => f.write_str(name)?,
                Part::Path(path) => write!(f, "{}", path.display())?,
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
macro_rules! display_from_parts {
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
                Part::Path(Path::new("/p")),
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

    // An empty text adds no part, so an absent clause leaves no trace.
    #[test]
    fn an_empty_text_adds_no_part() {
        let parts = Parts::new().text("").name("n").text(String::new());

        assert_eq!(listed(&parts), [Part::Name("n")]);
    }
}
