//! Shared text formatting utilities for consistent styling

use crate::terminal_text::{Command, Literal, Shown, Word, lit};

/// Returns the selfie command naming a spec: `selfie` and `words`, then `name`
/// quoted as one shell word, after `--` when it starts with a dash, then
/// `tail`. Returns `None` when `name` holds NUL, which no shell word can carry.
pub(crate) fn selfie_command(words: &[Literal], name: &str, tail: &[Literal]) -> Option<Command> {
    Command::composed(
        std::iter::once(Word::Literal(lit!("selfie")))
            .chain(words.iter().copied().map(Word::Literal))
            .chain([Word::Operand(name)])
            .chain(tail.iter().copied().map(Word::Literal)),
    )
}

/// Returns the command that creates the directory at `path` and every
/// directory above it, for a path that names nothing. A `--` ends the options
/// first, so a path starting with a dash is not read as one. Returns `None`
/// when `path` holds NUL, as the library's remedy does.
pub(crate) fn mkdir_command(path: &std::path::Path) -> Option<Command> {
    Command::composed([
        Word::Literal(lit!("mkdir")),
        Word::Literal(lit!("-p")),
        Word::Literal(lit!("--")),
        Word::Path(path),
    ])
}

/// `items`, each shown as it is, joined by commas.
pub(crate) fn comma_joined(items: impl IntoIterator<Item = impl Into<Shown>>) -> Shown {
    Shown::join(items, lit!(", "))
}

/// `one` when `count` is one, `many` otherwise.
pub(crate) fn plural(count: usize, one: Literal, many: Literal) -> Literal {
    if count == 1 { one } else { many }
}

/// `text`, a field's name, in key styling: bold and cyan when `use_colors`.
pub(crate) fn format_key(text: Literal, use_colors: bool) -> Shown {
    Shown::from(text).paint(use_colors, |s| s.bold().cyan())
}

#[cfg(test)]
mod tests {
    use super::*;

    // One takes the singular, and none or several the plural.
    #[test]
    fn plural_picks_by_count() {
        let pick = |count| plural(count, lit!("file"), lit!("files")).as_str();
        assert_eq!([pick(0), pick(1), pick(2)], ["files", "file", "files"]);
    }

    // A name is quoted where the shell would split it, and a leading dash is
    // ended as an option, so the command shown runs as shown.
    #[test]
    fn a_selfie_command_runs_as_shown() {
        let edit = |name| {
            selfie_command(&[lit!("spec"), lit!("edit")], name, &[])
                .expect("no NUL")
                .to_string()
        };
        assert_eq!(edit("tool"), "selfie spec edit tool");
        assert_eq!(edit("café"), "selfie spec edit 'café'");
        assert_eq!(edit("-tool"), "selfie spec edit -- -tool");
        assert!(selfie_command(&[lit!("spec"), lit!("edit")], "a\0b", &[]).is_none());
        assert_eq!(
            selfie_command(
                &[lit!("package"), lit!("track-dotfile")],
                "x",
                &[lit!("<file>")]
            )
            .expect("no NUL")
            .to_string(),
            "selfie package track-dotfile x <file>"
        );
    }

    // The library words the same remedy in its own sentences, so the two
    // spellings of the command cannot drift apart.
    #[test]
    fn a_composed_mkdir_matches_the_library_remedy() {
        for path in [
            "/home/me/dotfiles",
            "/home/me/my dotfiles",
            "~/my dir",
            "-foo",
        ] {
            let path = std::path::Path::new(path);
            let remedy = selfie::fs::AbsentReason::Empty
                .remedy(path)
                .expect("an empty path has a remedy");
            let (_, library) = remedy
                .split_once("with: ")
                .expect("a command after `with: `");
            assert_eq!(
                mkdir_command(path)
                    .map(|mkdir| mkdir.to_string())
                    .as_deref(),
                Some(library)
            );
        }
        // A path no shell word can carry has no remedy here or in the library.
        let nul = std::path::Path::new("/a\0b");
        assert_eq!(selfie::fs::AbsentReason::Empty.remedy(nul), None);
        assert!(mkdir_command(nul).is_none());
    }
}
