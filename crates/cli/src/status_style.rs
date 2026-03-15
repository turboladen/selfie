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
static NA_EMOJI: Emoji<'_, '_> = Emoji("⚪ ", "[N/A] ");

// Output line emojis for installation display
#[allow(dead_code)]
static STDOUT_OUTPUT_EMOJI: Emoji<'_, '_> = Emoji("📦 ", "[o] ");
#[allow(dead_code)]
static STDERR_OUTPUT_EMOJI: Emoji<'_, '_> = Emoji("🔧 ", "[e] ");
#[allow(dead_code)]
static OUTPUT_HEADER_EMOJI: Emoji<'_, '_> = Emoji("📋 ", ">> ");

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

/// Format a "not available" status indicator
pub(crate) fn format_na(use_colors: bool) -> String {
    format_status_indicator(NA_EMOJI, "N/A", use_colors, |s| s.dim())
}

/// Format a stdout output line with appropriate prefix
#[allow(dead_code)]
pub(crate) fn format_stdout_output(line: &str, use_colors: bool) -> String {
    let trimmed = line.trim();
    let text = if use_colors {
        style(trimmed).dim().to_string()
    } else {
        trimmed.to_string()
    };
    format!("    {STDOUT_OUTPUT_EMOJI}{text}")
}

/// Format a stderr output line with appropriate prefix
#[allow(dead_code)]
pub(crate) fn format_stderr_output(line: &str, use_colors: bool) -> String {
    let trimmed = line.trim();
    let text = if use_colors {
        style(trimmed).dim().to_string()
    } else {
        trimmed.to_string()
    };
    format!("    {STDERR_OUTPUT_EMOJI}{text}")
}

/// Format an installation output header
#[allow(dead_code)]
pub(crate) fn format_output_header(use_colors: bool) -> String {
    let label = "Installation output:";
    let text = if use_colors {
        style(label).bold().to_string()
    } else {
        label.to_string()
    };
    format!("\n{OUTPUT_HEADER_EMOJI}{text}")
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
    fn test_format_na() {
        let result = format_na(false);
        assert!(result.contains("N/A"));
    }

    #[test]
    fn test_format_stdout_output() {
        let result = format_stdout_output("hello world", false);
        assert!(result.contains("hello world"));
        assert!(result.starts_with("    "));
    }

    #[test]
    fn test_format_stdout_output_trims_whitespace() {
        let result = format_stdout_output("  padded line  ", false);
        assert!(result.contains("padded line"));
    }

    #[test]
    fn test_format_stderr_output() {
        let result = format_stderr_output("some error", false);
        assert!(result.contains("some error"));
        assert!(result.starts_with("    "));
    }

    #[test]
    fn test_format_output_header() {
        let result = format_output_header(false);
        assert!(result.contains("Installation output:"));
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
            format_na(false),
            format_stdout_output("test", false),
            format_stderr_output("test", false),
            format_output_header(false),
        ];
        for f in formats {
            assert!(!f.contains("\x1b["), "Found ANSI codes in: {f}");
        }
    }
}
