//! Static status formatting for tables and status indicators
//!
//! Pure formatting functions that produce styled text strings for use in
//! table cells and status displays. No spinner or progress bar dependency.

use console::style;

/// Format a status indicator with a marker prefix and optional color styling
fn format_status_indicator(
    marker: &str,
    label: &str,
    use_colors: bool,
    style_fn: fn(console::StyledObject<String>) -> console::StyledObject<String>,
) -> String {
    let text = if use_colors {
        style_fn(style(label.to_string())).to_string()
    } else {
        label.to_string()
    };
    format!("{marker} {text}")
}

/// Format an "installed" status indicator
pub(crate) fn format_installed(use_colors: bool) -> String {
    format_status_indicator("✓", "Installed", use_colors, |s| s.green())
}

/// Format a "valid" status indicator (for validation results)
pub(crate) fn format_valid(use_colors: bool) -> String {
    format_status_indicator("✓", "Valid", use_colors, |s| s.green())
}

/// Format a "not installed" status indicator
pub(crate) fn format_not_installed(use_colors: bool) -> String {
    format_status_indicator("✗", "Not installed", use_colors, |s| s.cyan())
}

/// Format a status check error indicator
pub(crate) fn format_status_error(use_colors: bool) -> String {
    format_status_indicator("✗", "Error", use_colors, |s| s.red())
}

/// Format a "clean" audit status indicator
pub(crate) fn format_audit_clean(use_colors: bool) -> String {
    format_status_indicator("✓", "Clean", use_colors, |s| s.green())
}

/// Format a "conflicts detected" audit status indicator
pub(crate) fn format_audit_conflicts(use_colors: bool) -> String {
    format_status_indicator("⚠", "Conflicts Detected", use_colors, |s| s.yellow())
}

/// Format a "no audit command" status indicator
pub(crate) fn format_no_audit(use_colors: bool) -> String {
    format_status_indicator("⚠", "No audit command", use_colors, |s| s.yellow())
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
    use selfie::package::event::{CheckFailure, CheckResult};
    match status {
        Some(CheckResult::Success { .. }) => styled_text("Installed", use_colors, |s| s.green()),
        Some(CheckResult::Failed { .. }) => styled_text("Not installed", use_colors, |s| s.cyan()),
        Some(CheckResult::NoCheckCommand) => styled_text("No check", use_colors, |s| s.yellow()),
        Some(CheckResult::CouldNotRun(CheckFailure::Canceled { .. })) => {
            styled_text("Canceled", use_colors, |s| s.yellow())
        }
        Some(CheckResult::CouldNotRun(CheckFailure::TimedOut(timed_out))) => {
            styled_text(&timed_out.to_string(), use_colors, |s| s.red())
        }
        Some(CheckResult::CouldNotRun(failure)) => {
            let msg = format!("Error: {failure}");
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

    // A check that timed out on a long command shows a bounded sentence in its
    // status cell, though the cell unwraps the timeout to print it.
    #[test]
    fn a_timed_out_check_status_is_bounded() {
        use selfie::commands::runner::TimedOut;
        use selfie::package::event::{CheckFailure, CheckResult};

        let long = "Z".repeat(30_000);
        let status = CheckResult::CouldNotRun(CheckFailure::TimedOut(TimedOut::new(
            long,
            std::time::Duration::from_secs(1),
        )));

        let cell = format_check_result(Some(&status), false);

        assert!(cell.contains("bytes elided"), "{}", &cell[..80]);
        assert!(cell.matches('Z').count() < 3_000, "{}", &cell[..80]);
    }

    #[test]
    fn test_format_installed_with_colors() {
        let result = format_installed(true);
        assert!(result.contains("Installed"));
        assert!(result.contains("✓"));
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
        assert!(result.contains("✗"));
    }

    #[test]
    fn test_format_status_error() {
        let result = format_status_error(false);
        assert!(result.contains("Error"));
    }

    #[test]
    fn test_format_valid() {
        let result = format_valid(false);
        assert!(result.contains("Valid"));
        assert!(result.contains("✓"));
    }

    #[test]
    fn test_format_audit_clean() {
        let result = format_audit_clean(false);
        assert!(result.contains("Clean"));
        assert!(result.contains("✓"));
    }

    #[test]
    fn test_format_audit_conflicts() {
        let result = format_audit_conflicts(false);
        assert!(result.contains("Conflicts Detected"));
        assert!(result.contains("⚠"));
    }

    #[test]
    fn test_format_no_audit() {
        let result = format_no_audit(false);
        assert!(result.contains("No audit command"));
        assert!(result.contains("⚠"));
    }

    #[test]
    fn test_all_formats_no_ansi_without_colors() {
        // Verify no ANSI escape codes when colors disabled
        let formats = vec![
            format_installed(false),
            format_valid(false),
            format_not_installed(false),
            format_no_audit(false),
            format_status_error(false),
            format_audit_clean(false),
            format_audit_conflicts(false),
        ];
        for f in formats {
            assert!(!f.contains("\x1b["), "Found ANSI codes in: {f}");
        }
    }
}
