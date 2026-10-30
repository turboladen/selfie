//! Shared text formatting utilities for consistent styling

use crate::terminal_text::{Literal, Prose, Shown, lit, shown};

/// A spec name as the last argument of a command shown to the user: quoted
/// where the shell would split it, and after `--` when it starts with a dash.
pub(crate) fn name_argument(name: &str) -> Shown {
    let quoted = selfie::fs::shell_quote(std::path::Path::new(name));
    // Quoting does not stop a leading dash reading as an option. The command is
    // escaped as prose until slice 5's Command kind takes it.
    if name.starts_with('-') {
        shown!("-- {}", Prose::new(&quoted))
    } else {
        Prose::new(&quoted).into()
    }
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
    fn a_name_argument_runs_as_shown() {
        assert_eq!(name_argument("tool"), "tool");
        assert_eq!(name_argument("my app"), "'my app'");
        assert_eq!(name_argument("-tool"), "-- -tool");
    }
}
