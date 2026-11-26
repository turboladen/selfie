//! Static status formatting for tables and status indicators
//!
//! Pure formatting functions that produce styled text strings for use in
//! table cells and status displays. No spinner or progress bar dependency.

use crate::terminal_text::{Literal, Paint, Shown, lit, shown};

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

/// Format a "not installed" status indicator for a check that exited
/// `exit_code`.
pub(crate) fn format_not_installed(exit_code: Option<i32>, use_colors: bool) -> Shown {
    shown!(
        "{} {}",
        lit!("✗"),
        styled_text(not_installed_label(exit_code), use_colors, |s| s.cyan())
    )
}

/// "Not installed", followed by the shell's diagnosis when the check's exit
/// code names one.
fn not_installed_label(exit_code: Option<i32>) -> Shown {
    match selfie::package::event::ShellDiagnosis::of_exit_code(exit_code) {
        Some(diagnosis) => shown!(
            "{} ({})",
            lit!("Not installed"),
            crate::received::composed(&diagnosis)
        ),
        None => lit!("Not installed").into(),
    }
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
        EnvironmentStatus::Installed => styled_text(lit!("Installed"), use_colors, |s| s.green()),
        EnvironmentStatus::NotInstalled { exit_code } => {
            styled_text(not_installed_label(*exit_code), use_colors, |s| s.cyan())
        }
        EnvironmentStatus::Unknown(UnknownStatus::NoCheckCommand) => {
            styled_text(lit!("No check"), use_colors, |s| s.yellow())
        }
        EnvironmentStatus::Unknown(UnknownStatus::NotInEnvironment) => {
            styled_text(lit!("N/A"), use_colors, |s| s.dim())
        }
        EnvironmentStatus::Unknown(UnknownStatus::CheckFailed(CheckFailure::Canceled {
            ..
        })) => styled_text(lit!("Canceled"), use_colors, |s| s.yellow()),
        EnvironmentStatus::Unknown(UnknownStatus::CheckFailed(CheckFailure::TimedOut(
            timed_out,
        ))) => styled_text(crate::received::composed(timed_out), use_colors, |s| {
            s.red()
        }),
        EnvironmentStatus::Unknown(reason) => {
            let msg = shown!("Error: {}", crate::received::unknown_status(reason));
            styled_text(msg, use_colors, |s| s.red())
        }
    }
}

/// `text` styled by `paint` when `use_colors`.
pub(crate) fn styled_text(text: impl Into<Shown>, use_colors: bool, paint: Paint) -> Shown {
    text.into().paint(use_colors, paint)
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
            CheckFailure::TimedOut(TimedOut::new(
                long,
                std::time::Duration::from_secs(1),
                selfie::commands::runner::TimeLimit::Total,
            )),
        ));

        let cell = format_list_status(&status, false);

        assert!(cell.contains("bytes elided"), "{}", &cell[..80]);
        assert!(cell.matches('Z').count() < 3_000, "{}", &cell[..80]);
    }

    // A not-installed row names the shell's diagnosis for 127 and 126, and an
    // ordinary exit code, or none, adds nothing. Both renderers that show a
    // status word it the same way.
    #[test]
    fn a_not_installed_status_names_the_shells_diagnosis() {
        use selfie::package::event::EnvironmentStatus;

        let cases = [
            (
                Some(127),
                "Not installed (command not found, exit code 127)",
            ),
            (
                Some(126),
                "Not installed (command not executable, exit code 126)",
            ),
            (Some(1), "Not installed"),
            (Some(128), "Not installed"),
            (None, "Not installed"),
        ];
        for (exit_code, expected) in cases {
            let status = EnvironmentStatus::NotInstalled { exit_code };
            assert_eq!(
                format_list_status(&status, false).to_string(),
                expected,
                "{exit_code:?}"
            );
            assert_eq!(
                crate::commands::spec::info::format_status(&status, false).to_string(),
                format!("✗ {expected}"),
                "{exit_code:?}"
            );
        }
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
        let result = format_not_installed(None, false);
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
            format_not_installed(None, false),
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
