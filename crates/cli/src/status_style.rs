//! Static status formatting for tables and status indicators
//!
//! Pure formatting functions that produce styled text strings for use in
//! table cells and status displays. No spinner or progress bar dependency.

use crate::terminal_text::{Literal, Paint, Prose, Shown, lit, shown};

/// Format a status indicator with a marker prefix and optional color styling
fn format_status_indicator(
    marker: Literal,
    label: Literal,
    use_colors: bool,
    paint: Paint,
) -> Shown {
    shown!("{} {}", marker, Shown::from(label).paint(use_colors, paint))
}

/// Format an "installed" status indicator
pub(crate) fn format_installed(use_colors: bool) -> Shown {
    format_status_indicator(lit!("✓"), lit!("Installed"), use_colors, |s| s.green())
}

/// Format a "valid" status indicator (for validation results)
pub(crate) fn format_valid(use_colors: bool) -> Shown {
    format_status_indicator(lit!("✓"), lit!("Valid"), use_colors, |s| s.green())
}

/// Format a "not installed" status indicator
pub(crate) fn format_not_installed(use_colors: bool) -> Shown {
    format_status_indicator(lit!("✗"), lit!("Not installed"), use_colors, |s| s.cyan())
}

/// Format a status check error indicator
pub(crate) fn format_status_error(use_colors: bool) -> Shown {
    format_status_indicator(lit!("✗"), lit!("Error"), use_colors, |s| s.red())
}

/// Format a "clean" audit status indicator
pub(crate) fn format_audit_clean(use_colors: bool) -> Shown {
    format_status_indicator(lit!("✓"), lit!("Clean"), use_colors, |s| s.green())
}

/// Format a "conflicts detected" audit status indicator
pub(crate) fn format_audit_conflicts(use_colors: bool) -> Shown {
    format_status_indicator(lit!("⚠"), lit!("Conflicts Detected"), use_colors, |s| {
        s.yellow()
    })
}

/// Format a "no audit command" status indicator
pub(crate) fn format_no_audit(use_colors: bool) -> Shown {
    format_status_indicator(lit!("⚠"), lit!("No audit command"), use_colors, |s| {
        s.yellow()
    })
}

/// The status cell of a `package list` row: a few words, styled when
/// `use_colors` is set, with no marker. The row's marker cell is separate.
pub(crate) fn format_list_status(
    status: &selfie::package::event::EnvironmentStatus,
    use_colors: bool,
) -> Shown {
    use selfie::package::event::{CheckFailure, EnvironmentStatus, UnknownStatus};
    match status {
        EnvironmentStatus::Installed => styled_text("Installed", use_colors, |s| s.green()),
        EnvironmentStatus::NotInstalled => styled_text("Not installed", use_colors, |s| s.cyan()),
        EnvironmentStatus::Unknown(UnknownStatus::NoCheckCommand) => {
            styled_text("No check", use_colors, |s| s.yellow())
        }
        EnvironmentStatus::Unknown(UnknownStatus::NotInEnvironment) => {
            styled_text("N/A", use_colors, |s| s.dim())
        }
        EnvironmentStatus::Unknown(UnknownStatus::CheckFailed(CheckFailure::Canceled {
            ..
        })) => styled_text("Canceled", use_colors, |s| s.yellow()),
        EnvironmentStatus::Unknown(UnknownStatus::CheckFailed(CheckFailure::TimedOut(
            timed_out,
        ))) => styled_text(&timed_out.to_string(), use_colors, |s| s.red()),
        EnvironmentStatus::Unknown(reason) => {
            let msg = format!("Error: {reason}");
            styled_text(&msg, use_colors, |s| s.red())
        }
    }
}

/// `text`, prose, escaped and then styled by `paint` when `use_colors`.
pub(crate) fn styled_text(text: &str, use_colors: bool, paint: Paint) -> Shown {
    Shown::indented(Prose::new(text), lit!("  ")).paint(use_colors, paint)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A check that timed out on a long command shows a bounded sentence in its
    // status cell, though the cell unwraps the timeout to print it.
    #[test]
    fn a_timed_out_check_status_is_bounded() {
        use selfie::commands::runner::TimedOut;
        use selfie::package::event::{CheckFailure, EnvironmentStatus, UnknownStatus};

        let long = "Z".repeat(30_000);
        let status = EnvironmentStatus::Unknown(UnknownStatus::CheckFailed(
            CheckFailure::TimedOut(TimedOut::new(long, std::time::Duration::from_secs(1))),
        ));

        let cell = format_list_status(&status, false);

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
