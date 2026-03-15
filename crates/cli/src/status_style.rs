//! Static status formatting for tables and status indicators
//!
//! Pure formatting functions that produce styled text strings for use in
//! table cells and status displays. No spinner or progress bar dependency.

use console::{Emoji, style};

// Status-specific emojis for package status indicators
static INSTALLED_EMOJI: Emoji<'_, '_> = Emoji("✅ ", "[✓] ");
static NOT_INSTALLED_EMOJI: Emoji<'_, '_> = Emoji("📦 ", "[×] ");
static NO_CHECK_EMOJI: Emoji<'_, '_> = Emoji("⚠️ ", "[?] ");
static CMD_NOT_FOUND_EMOJI: Emoji<'_, '_> = Emoji("🔍 ", "[!] ");
static STATUS_ERROR_EMOJI: Emoji<'_, '_> = Emoji("💥 ", "[E] ");

/// Format a status indicator with emoji and optional color styling
fn format_status_indicator(
    emoji: Emoji<'_, '_>,
    label: &str,
    use_colors: bool,
    style_fn: fn(console::StyledObject<String>) -> console::StyledObject<String>,
) -> String {
    let text = if use_colors {
        style_fn(style(label.to_string())).to_string()
    } else {
        label.to_string()
    };
    format!("{emoji}{text}")
}

/// Format an "installed" status indicator
pub(crate) fn format_installed(use_colors: bool) -> String {
    format_status_indicator(INSTALLED_EMOJI, "Installed", use_colors, |s| s.green())
}

/// Format a "not installed" status indicator
pub(crate) fn format_not_installed(use_colors: bool) -> String {
    format_status_indicator(NOT_INSTALLED_EMOJI, "Not installed", use_colors, |s| {
        s.cyan()
    })
}

/// Format a "no check command" status indicator
pub(crate) fn format_no_check(use_colors: bool) -> String {
    format_status_indicator(NO_CHECK_EMOJI, "No check", use_colors, |s| s.yellow())
}

/// Format a "command not found" status indicator
pub(crate) fn format_cmd_not_found(use_colors: bool) -> String {
    format_status_indicator(CMD_NOT_FOUND_EMOJI, "Cmd not found", use_colors, |s| {
        s.red()
    })
}

/// Format a status check error indicator
pub(crate) fn format_status_error(use_colors: bool) -> String {
    format_status_indicator(STATUS_ERROR_EMOJI, "Error", use_colors, |s| s.red())
}

/// Format a check result as plain text status (no emoji prefix).
///
/// Used in contexts where emoji prefixes are inappropriate, such as
/// inline status within spinner lines. For emoji-prefixed status
/// indicators (used in tables), use `format_installed()` etc.
pub(crate) fn format_check_result(
    status: Option<&selfie::package::event::CheckResult>,
    use_colors: bool,
) -> String {
    use selfie::package::event::CheckResult;
    match status {
        Some(CheckResult::Success { .. }) => styled_text("Installed", use_colors, |s| s.green()),
        Some(CheckResult::Failed { .. }) => styled_text("Not installed", use_colors, |s| s.cyan()),
        Some(CheckResult::NoCheckCommand) => styled_text("No check", use_colors, |s| s.yellow()),
        Some(CheckResult::CommandNotFound) => styled_text("Cmd not found", use_colors, |s| s.red()),
        Some(CheckResult::Error(e)) => {
            let msg = format!("Error: {e}");
            styled_text(&msg, use_colors, |s| s.red())
        }
        None => styled_text("N/A", use_colors, |s| s.dim()),
    }
}

/// Apply color styling to text, or return plain text if colors disabled
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
    fn test_format_installed_with_colors() {
        let result = format_installed(true);
        assert!(result.contains("Installed"));
        assert!(result.contains("✅") || result.contains("[✓]"));
    }

    #[test]
    fn test_format_installed_without_colors() {
        let result = format_installed(false);
        assert!(result.contains("Installed"));
        assert!(!result.contains("\x1b["));
    }

    #[test]
    fn test_format_not_installed() {
        let result = format_not_installed(false);
        assert!(result.contains("Not installed"));
        assert!(result.contains("📦") || result.contains("[×]"));
    }

    #[test]
    fn test_format_no_check() {
        let result = format_no_check(false);
        assert!(result.contains("No check"));
    }

    #[test]
    fn test_format_cmd_not_found() {
        let result = format_cmd_not_found(false);
        assert!(result.contains("Cmd not found"));
    }

    #[test]
    fn test_format_status_error() {
        let result = format_status_error(false);
        assert!(result.contains("Error"));
    }

    #[test]
    fn test_all_formats_no_ansi_without_colors() {
        // Verify no ANSI escape codes when colors disabled
        let formats = vec![
            format_installed(false),
            format_not_installed(false),
            format_no_check(false),
            format_cmd_not_found(false),
            format_status_error(false),
        ];
        for f in formats {
            assert!(!f.contains("\x1b["), "Found ANSI codes in: {f}");
        }
    }
}
