//! Shared text formatting utilities for consistent styling

use console::style;
use std::fmt::Display;

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

/// Format text with key field styling (bold and cyan when colors enabled)
pub(crate) fn format_key<T: Display>(text: T, use_colors: bool) -> String {
    let text = text.to_string();
    let styled = style(text).bold();

    if use_colors {
        styled.cyan().to_string()
    } else {
        styled.to_string()
    }
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
