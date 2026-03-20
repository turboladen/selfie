//! Git status formatting for tables and status displays
//!
//! Provides short (for list tables) and long (for info displays) formatted
//! representations of git file status, following the pattern in `status_style.rs`.

use console::style;
use selfie::package::git::GitFileStatus;

/// Format git status as a short indicator for list table columns.
///
/// Returns single-character or short indicators:
/// - Clean → `✓`
/// - Modified → `M`
/// - Staged → `S`
/// - StagedAndModified → `SM`
/// - Untracked → `?`
/// - NotInRepo → `--`
pub(crate) fn format_git_status_short(status: &GitFileStatus, use_colors: bool) -> String {
    match status {
        GitFileStatus::Clean => styled_text("✓", use_colors, |s| s.dim().green()),
        GitFileStatus::Modified => styled_text("M", use_colors, |s| s.yellow()),
        GitFileStatus::Staged => styled_text("S", use_colors, |s| s.green()),
        GitFileStatus::StagedAndModified => styled_text("SM", use_colors, |s| s.yellow()),
        GitFileStatus::Untracked => styled_text("?", use_colors, |s| s.red()),
        GitFileStatus::NotInRepo => styled_text("--", use_colors, |s| s.dim()),
    }
}

/// Format git status as a descriptive label for info table rows.
///
/// Returns full descriptions like "Modified (unstaged)", "Staged", etc.
pub(crate) fn format_git_status_long(status: &GitFileStatus, use_colors: bool) -> String {
    match status {
        GitFileStatus::Clean => styled_text("Clean", use_colors, |s| s.dim().green()),
        GitFileStatus::Modified => styled_text("Modified (unstaged)", use_colors, |s| s.yellow()),
        GitFileStatus::Staged => styled_text("Staged", use_colors, |s| s.green()),
        GitFileStatus::StagedAndModified => {
            styled_text("Staged + Modified", use_colors, |s| s.yellow())
        }
        GitFileStatus::Untracked => styled_text("Untracked", use_colors, |s| s.red()),
        GitFileStatus::NotInRepo => styled_text("Not in repo", use_colors, |s| s.dim()),
    }
}

fn styled_text(
    text: &str,
    use_colors: bool,
    style_fn: fn(console::StyledObject<String>) -> console::StyledObject<String>,
) -> String {
    if use_colors {
        style_fn(style(text.to_string())).to_string()
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_format_all_variants() {
        assert_eq!(format_git_status_short(&GitFileStatus::Clean, false), "✓");
        assert_eq!(
            format_git_status_short(&GitFileStatus::Modified, false),
            "M"
        );
        assert_eq!(format_git_status_short(&GitFileStatus::Staged, false), "S");
        assert_eq!(
            format_git_status_short(&GitFileStatus::StagedAndModified, false),
            "SM"
        );
        assert_eq!(
            format_git_status_short(&GitFileStatus::Untracked, false),
            "?"
        );
        assert_eq!(
            format_git_status_short(&GitFileStatus::NotInRepo, false),
            "--"
        );
    }

    #[test]
    fn long_format_all_variants() {
        assert_eq!(
            format_git_status_long(&GitFileStatus::Clean, false),
            "Clean"
        );
        assert_eq!(
            format_git_status_long(&GitFileStatus::Modified, false),
            "Modified (unstaged)"
        );
        assert_eq!(
            format_git_status_long(&GitFileStatus::Staged, false),
            "Staged"
        );
        assert_eq!(
            format_git_status_long(&GitFileStatus::StagedAndModified, false),
            "Staged + Modified"
        );
        assert_eq!(
            format_git_status_long(&GitFileStatus::Untracked, false),
            "Untracked"
        );
        assert_eq!(
            format_git_status_long(&GitFileStatus::NotInRepo, false),
            "Not in repo"
        );
    }

    #[test]
    fn no_ansi_codes_without_colors() {
        let statuses = [
            GitFileStatus::Clean,
            GitFileStatus::Modified,
            GitFileStatus::Staged,
            GitFileStatus::StagedAndModified,
            GitFileStatus::Untracked,
            GitFileStatus::NotInRepo,
        ];
        for status in &statuses {
            let short = format_git_status_short(status, false);
            let long = format_git_status_long(status, false);
            assert!(!short.contains("\x1b["), "Found ANSI in short: {short}");
            assert!(!long.contains("\x1b["), "Found ANSI in long: {long}");
        }
    }

    #[test]
    fn colors_enabled_does_not_panic() {
        // Note: console crate may not emit ANSI codes without a TTY,
        // so we just verify it doesn't panic and returns non-empty text.
        let short = format_git_status_short(&GitFileStatus::Modified, true);
        assert!(!short.is_empty());
        assert!(short.contains('M'));

        let long = format_git_status_long(&GitFileStatus::Staged, true);
        assert!(!long.is_empty());
        assert!(long.contains("Staged"));
    }
}
