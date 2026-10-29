//! Shared text formatting utilities for consistent styling

use crate::terminal_text::{Literal, Shown};

/// A spec name as the last argument of a command shown to the user: quoted
/// where the shell would split it, and after `--` when it starts with a dash.
pub(crate) fn name_argument(name: &str) -> String {
    let quoted = selfie::fs::shell_quote(std::path::Path::new(name));
    // Quoting does not stop a leading dash reading as an option.
    if name.starts_with('-') {
        format!("-- {quoted}")
    } else {
        quoted
    }
}

/// `text`, a field's name, in key styling: bold and cyan when `use_colors`.
pub(crate) fn format_key(text: Literal, use_colors: bool) -> Shown {
    Shown::from(text).paint(use_colors, |s| s.bold().cyan())
}

#[cfg(test)]
mod tests {
    use super::*;

    // A name is quoted where the shell would split it, and a leading dash is
    // ended as an option, so the command shown runs as shown.
    #[test]
    fn a_name_argument_runs_as_shown() {
        assert_eq!(name_argument("tool"), "tool");
        assert_eq!(name_argument("my app"), "'my app'");
        assert_eq!(name_argument("-tool"), "-- -tool");
    }
}
