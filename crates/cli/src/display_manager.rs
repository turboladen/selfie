//! indicatif-powered display layer for CLI output
//!
//! Provides spinners, progress tracking, styled output, and structured error
//! summaries. Replaces `TerminalProgressReporter` with a unified display system
//! that handles both event-driven and static output consistently.

use std::collections::VecDeque;
use std::fmt::Display;
use std::sync::{Arc, Mutex};

use console::style;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

/// Structured error detail for the end-of-operation summary
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct ErrorDetail {
    pub package_name: String,
    pub operation: String,
    pub command: Option<String>,
    pub exit_code: Option<i32>,
    pub stderr: Option<String>,
    pub stdout: Option<String>,
    pub message: String,
}

/// Collects errors during an operation for summary display at the end
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub(crate) struct ErrorCollector {
    errors: Vec<ErrorDetail>,
}

impl ErrorCollector {
    /// Add an error to the collection
    pub(crate) fn collect(&mut self, error: ErrorDetail) {
        self.errors.push(error);
    }

    /// Check if any errors have been collected
    pub(crate) fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// Format and return the error summary as a string
    pub(crate) fn format_summary(&self) -> Option<String> {
        if self.errors.is_empty() {
            return None;
        }

        let mut lines = Vec::new();
        lines.push(String::new());
        lines.push("── Errors ─────────────────────────────────────".to_string());

        for error in &self.errors {
            lines.push(format!("✗ {}", error.package_name));
            if !error.message.is_empty() {
                lines.push(format!("  {}", error.message));
            }
            if let Some(cmd) = &error.command {
                lines.push(format!("  Command: {cmd}"));
            }
            if let Some(code) = error.exit_code {
                lines.push(format!("  Exit code: {code}"));
            }
            if let Some(stderr) = &error.stderr {
                let stderr = stderr.trim();
                if !stderr.is_empty() {
                    lines.push("  stderr:".to_string());
                    for line in stderr.lines() {
                        lines.push(format!("    {line}"));
                    }
                }
            }
            if let Some(stdout) = &error.stdout {
                let stdout = stdout.trim();
                if !stdout.is_empty() {
                    lines.push("  stdout:".to_string());
                    for line in stdout.lines() {
                        lines.push(format!("    {line}"));
                    }
                }
            }
            lines.push(String::new());
        }

        lines.push("───────────────────────────────────────────────".to_string());
        Some(lines.join("\n"))
    }
}

/// Handle to an active operation's spinner/progress bar
///
/// Created by `DisplayManager::start_operation()`. Used to update progress,
/// show command output, and finalize the operation display.
#[allow(dead_code)]
pub(crate) struct OperationHandle {
    bar: ProgressBar,
    use_colors: bool,
    output_lines: VecDeque<String>,
    max_output_lines: usize,
    mp: MultiProgress,
}

#[allow(dead_code)]
impl OperationHandle {
    /// Update the spinner message with progress information
    pub(crate) fn update_progress(&self, step: usize, total_steps: usize, message: &str) {
        self.bar
            .set_message(format!("{message} ({step}/{total_steps})"));
    }

    /// Update the spinner message without step counts
    pub(crate) fn set_message(&self, message: impl Display) {
        self.bar.set_message(message.to_string());
    }

    /// Print a line above the spinner (safe during active spinner)
    pub(crate) fn println(&self, message: &str) {
        let _ = self.mp.println(message);
    }

    /// Add a line of command output below the spinner
    pub(crate) fn add_output_line(&mut self, line: &str, is_stderr: bool) {
        let prefix = if is_stderr { "  │ err: " } else { "  │ " };
        let formatted = if self.use_colors {
            format!("{}{}", prefix, style(line.trim()).dim())
        } else {
            format!("{}{}", prefix, line.trim())
        };

        self.output_lines.push_back(formatted.clone());
        while self.output_lines.len() > self.max_output_lines {
            self.output_lines.pop_front();
        }

        // Print through MultiProgress to avoid clobbering the spinner
        let _ = self.mp.println(&formatted);
    }

    /// Complete the operation with success
    pub(crate) fn finish_success(&self, message: impl Display) {
        let msg = if self.use_colors {
            format!("{} {}", style("✓").green().bold(), style(message).green())
        } else {
            format!("✓ {message}")
        };
        self.bar.finish_and_clear();
        let _ = self.mp.println(&msg);
    }

    /// Complete the operation with failure
    pub(crate) fn finish_failure(&self, message: impl Display) {
        let msg = if self.use_colors {
            format!("{} {}", style("✗").red().bold(), style(message).red())
        } else {
            format!("✗ {message}")
        };
        self.bar.finish_and_clear();
        let _ = self.mp.println(&msg);
    }

    /// Complete the operation with a warning
    pub(crate) fn finish_warning(&self, message: impl Display) {
        let msg = if self.use_colors {
            format!("{} {}", style("⚠").yellow().bold(), style(message).yellow())
        } else {
            format!("⚠ {message}")
        };
        self.bar.finish_and_clear();
        let _ = self.mp.println(&msg);
    }

    /// Clear the spinner without printing a final message
    pub(crate) fn finish_clear(&self) {
        self.bar.finish_and_clear();
    }

    /// Complete the operation in-place (preserves position in MultiProgress)
    pub(crate) fn finish_success_in_place(&self, message: impl Display) {
        let msg = if self.use_colors {
            format!("{} {}", style("✓").green().bold(), style(message).green())
        } else {
            format!("✓ {message}")
        };
        self.bar
            .set_style(ProgressStyle::with_template("{msg}").unwrap());
        self.bar.finish_with_message(msg);
    }

    /// Complete the operation in-place with failure
    pub(crate) fn finish_failure_in_place(&self, message: impl Display) {
        let msg = if self.use_colors {
            format!("{} {}", style("✗").red().bold(), style(message).red())
        } else {
            format!("✗ {message}")
        };
        self.bar
            .set_style(ProgressStyle::with_template("{msg}").unwrap());
        self.bar.finish_with_message(msg);
    }

    /// Complete the operation in-place with warning
    pub(crate) fn finish_warning_in_place(&self, message: impl Display) {
        let msg = if self.use_colors {
            format!("{} {}", style("⚠").yellow().bold(), style(message).yellow())
        } else {
            format!("⚠ {message}")
        };
        self.bar
            .set_style(ProgressStyle::with_template("{msg}").unwrap());
        self.bar.finish_with_message(msg);
    }
}

/// Central display manager for all CLI output
///
/// Provides two API layers:
/// 1. **Static output**: `print_info()`, `print_error()`, etc. for simple styled messages
/// 2. **Dynamic output**: `start_operation()` returns an `OperationHandle` for spinners/progress
///
/// All output goes through `MultiProgress` when spinners are active, preventing
/// interleaving and visual corruption.
#[derive(Clone)]
pub struct DisplayManager {
    #[allow(dead_code)]
    mp: MultiProgress,
    use_colors: bool,
    is_tty: bool,
    errors: Arc<Mutex<ErrorCollector>>,
}

impl DisplayManager {
    /// Create a new display manager
    pub fn new(use_colors: bool) -> Self {
        let is_tty = console::Term::stderr().is_term();
        let mp = if is_tty {
            MultiProgress::new()
        } else {
            let mp = MultiProgress::new();
            mp.set_draw_target(ProgressDrawTarget::hidden());
            mp
        };

        Self {
            mp,
            use_colors,
            is_tty,
            errors: Arc::new(Mutex::new(ErrorCollector::default())),
        }
    }

    /// Whether colors are enabled
    pub fn use_colors(&self) -> bool {
        self.use_colors
    }

    // ── Static output methods ──────────────────────────────────────────
    //
    // Phase 1: These use println!/eprintln! directly to match expected
    // stdout/stderr conventions. Info, success, progress, and suggestion
    // go to stdout; errors and warnings go to stderr.
    //
    // Phase 2 (when spinners are wired into event handlers): These should
    // route through self.mp.println() / self.mp.eprintln() to avoid
    // interleaving with active spinner output.

    /// Print an informational message (stdout)
    pub(crate) fn print_info(&self, message: impl Display) {
        if self.use_colors {
            println!("{} {}", style("ℹ").blue(), style(message).blue());
        } else {
            println!("ℹ {message}");
        }
    }

    /// Print an error message (stderr)
    pub(crate) fn print_error(&self, message: impl Display) {
        if self.use_colors {
            eprintln!(
                "{} {}",
                style("✗").red().bold(),
                style(message).red().bold()
            );
        } else {
            eprintln!("✗ {message}");
        }
    }

    /// Print a success message (stdout)
    pub(crate) fn print_success(&self, message: impl Display) {
        if self.use_colors {
            println!("{} {}", style("✓").green().bold(), style(message).green());
        } else {
            println!("✓ {message}");
        }
    }

    /// Print a warning message (stderr)
    pub(crate) fn print_warning(&self, message: impl Display) {
        if self.use_colors {
            eprintln!(
                "{} {}",
                style("⚠").yellow().bold(),
                style(message).yellow().bold()
            );
        } else {
            eprintln!("⚠ {message}");
        }
    }

    /// Print a progress/status message (stdout)
    pub(crate) fn print_progress(&self, message: impl Display) {
        if self.use_colors {
            println!("  {}", style(message).dim());
        } else {
            println!("  {message}");
        }
    }

    /// Print a suggestion message (stdout)
    pub(crate) fn print_suggestion(&self, message: impl Display) {
        if self.use_colors {
            println!(
                "{} {}: {}",
                style("✨").bold(),
                style("Suggestion").yellow().bold(),
                message
            );
        } else {
            println!("✨ Suggestion: {message}");
        }
    }

    /// Print a plain line to stdout
    ///
    /// Note: When spinners are active (Phase 2), this should route through
    /// `self.mp.println()` to avoid visual corruption. Currently spinners
    /// are not wired into event handlers, so direct println is safe.
    pub(crate) fn println(&self, message: impl Display) {
        println!("{message}");
    }

    /// Print a styled key-value pair (stdout, for config display, etc.)
    pub(crate) fn print_field(&self, key: impl Display, value: impl Display) {
        if self.use_colors {
            println!("  {} {}", style(key).italic().dim(), style(value).bold());
        } else {
            println!("  {key} {value}");
        }
    }

    /// Whether the output is a TTY (interactive terminal)
    pub(crate) fn is_tty(&self) -> bool {
        self.is_tty
    }

    // ── Dynamic output methods ─────────────────────────────────────────

    /// Start a new operation with a spinner
    ///
    /// Returns an `OperationHandle` that can be used to update progress,
    /// show command output, and finalize the operation display.
    #[allow(dead_code)]
    pub(crate) fn start_operation(&self, message: impl Display) -> OperationHandle {
        let spinner_style = if self.use_colors {
            ProgressStyle::with_template("{spinner:.cyan} {msg}")
                .unwrap()
                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"])
        } else {
            ProgressStyle::with_template("{spinner} {msg}")
                .unwrap()
                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"])
        };

        let bar = self.mp.add(ProgressBar::new_spinner());
        bar.set_style(spinner_style);
        bar.set_message(message.to_string());
        bar.enable_steady_tick(std::time::Duration::from_millis(80));

        OperationHandle {
            bar,
            use_colors: self.use_colors,
            output_lines: VecDeque::new(),
            max_output_lines: 5,
            mp: self.mp.clone(),
        }
    }

    /// Create a spinner for a list item (used by package list command)
    ///
    /// Unlike `start_operation()`, this creates a spinner optimized for
    /// sorted lists: the spinner resolves in-place to preserve ordering.
    pub(crate) fn start_list_spinner(&self, message: impl Display) -> OperationHandle {
        let spinner_style = if self.use_colors {
            ProgressStyle::with_template("{spinner:.cyan} {msg}")
                .unwrap()
                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"])
        } else {
            ProgressStyle::with_template("{spinner} {msg}")
                .unwrap()
                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"])
        };

        let bar = self.mp.add(ProgressBar::new_spinner());
        bar.set_style(spinner_style);
        bar.set_message(message.to_string());
        bar.enable_steady_tick(std::time::Duration::from_millis(80));

        OperationHandle {
            bar,
            use_colors: self.use_colors,
            output_lines: VecDeque::new(),
            max_output_lines: 0,
            mp: self.mp.clone(),
        }
    }

    // ── Error collection ───────────────────────────────────────────────

    /// Collect a structured error for the end-of-operation summary
    #[allow(dead_code)]
    pub(crate) fn collect_error(&self, error: ErrorDetail) {
        if let Ok(mut collector) = self.errors.lock() {
            collector.collect(error);
        }
    }

    /// Print the error summary if any errors were collected
    ///
    /// Call this after all operations are complete (e.g., at the end of
    /// `EventProcessor::process_events`).
    pub(crate) fn finish(&self) {
        if let Ok(collector) = self.errors.lock()
            && let Some(summary) = collector.format_summary()
        {
            eprintln!("{summary}");
        }
    }

    /// Check if any errors have been collected
    #[allow(dead_code)]
    pub(crate) fn has_errors(&self) -> bool {
        self.errors.lock().map(|c| c.has_errors()).unwrap_or(false)
    }
}

impl std::fmt::Debug for DisplayManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DisplayManager")
            .field("use_colors", &self.use_colors)
            .field("is_tty", &self.is_tty)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_display_manager_creation() {
        let dm = DisplayManager::new(false);
        assert!(!dm.use_colors());
    }

    #[test]
    fn test_display_manager_clone() {
        let dm = DisplayManager::new(false);
        let _dm2 = dm.clone();
    }

    #[test]
    fn test_display_manager_debug() {
        let dm = DisplayManager::new(false);
        let debug = format!("{dm:?}");
        assert!(debug.contains("DisplayManager"));
    }

    #[test]
    fn test_static_output_methods_dont_panic() {
        let dm = DisplayManager::new(false);
        dm.print_info("test info");
        dm.print_error("test error");
        dm.print_success("test success");
        dm.print_warning("test warning");
        dm.print_progress("test progress");
        dm.print_suggestion("test suggestion");
        dm.println("test println");
        dm.print_field("key:", "value");
    }

    #[test]
    fn test_static_output_with_colors() {
        let dm = DisplayManager::new(true);
        dm.print_info("test info");
        dm.print_error("test error");
        dm.print_success("test success");
        dm.print_warning("test warning");
    }

    #[test]
    fn test_start_operation() {
        let dm = DisplayManager::new(false);
        let handle = dm.start_operation("Testing...");
        handle.update_progress(1, 3, "Step 1");
        handle.set_message("Updated message");
        handle.finish_success("Done");
    }

    #[test]
    fn test_operation_handle_output_lines() {
        let dm = DisplayManager::new(false);
        let mut handle = dm.start_operation("Testing...");
        handle.add_output_line("line 1", false);
        handle.add_output_line("line 2", true);
        assert_eq!(handle.output_lines.len(), 2);

        // Add more than max to test rolling window
        for i in 0..10 {
            handle.add_output_line(&format!("line {i}"), false);
        }
        assert_eq!(handle.output_lines.len(), handle.max_output_lines);
        handle.finish_clear();
    }

    #[test]
    fn test_operation_handle_finish_variants() {
        let dm = DisplayManager::new(false);

        let h1 = dm.start_operation("Test 1");
        h1.finish_success("Success");

        let h2 = dm.start_operation("Test 2");
        h2.finish_failure("Failed");

        let h3 = dm.start_operation("Test 3");
        h3.finish_warning("Warning");

        let h4 = dm.start_operation("Test 4");
        h4.finish_clear();
    }

    #[test]
    fn test_error_collector_empty() {
        let collector = ErrorCollector::default();
        assert!(!collector.has_errors());
        assert!(collector.format_summary().is_none());
    }

    #[test]
    fn test_error_collector_with_errors() {
        let mut collector = ErrorCollector::default();
        collector.collect(ErrorDetail {
            package_name: "test-pkg".to_string(),
            operation: "install".to_string(),
            command: Some("brew install test-pkg".to_string()),
            exit_code: Some(1),
            stderr: Some("Error: not found".to_string()),
            stdout: None,
            message: "Installation failed".to_string(),
        });

        assert!(collector.has_errors());
        let summary = collector.format_summary().unwrap();
        assert!(summary.contains("test-pkg"));
        assert!(summary.contains("brew install test-pkg"));
        assert!(summary.contains("Exit code: 1"));
        assert!(summary.contains("Error: not found"));
        assert!(summary.contains("── Errors"));
    }

    #[test]
    fn test_display_manager_error_collection() {
        let dm = DisplayManager::new(false);
        assert!(!dm.has_errors());

        dm.collect_error(ErrorDetail {
            package_name: "test".to_string(),
            operation: "check".to_string(),
            command: None,
            exit_code: None,
            stderr: None,
            stdout: None,
            message: "test error".to_string(),
        });

        assert!(dm.has_errors());
        // finish() should print to stderr (just verify it doesn't panic)
        dm.finish();
    }

    #[test]
    fn test_display_manager_error_collection_across_clones() {
        let dm = DisplayManager::new(false);
        let dm2 = dm.clone();

        dm.collect_error(ErrorDetail {
            package_name: "test".to_string(),
            operation: "install".to_string(),
            command: None,
            exit_code: None,
            stderr: None,
            stdout: None,
            message: "shared error".to_string(),
        });

        // Clone should see the same errors (Arc<Mutex<>>)
        assert!(dm2.has_errors());
    }

    #[test]
    fn test_in_place_finish_variants() {
        let dm = DisplayManager::new(false);

        let h1 = dm.start_list_spinner("Test 1");
        h1.finish_success_in_place("Success result");

        let h2 = dm.start_list_spinner("Test 2");
        h2.finish_failure_in_place("Failed result");

        let h3 = dm.start_list_spinner("Test 3");
        h3.finish_warning_in_place("Warning result");
    }

    #[test]
    fn test_is_tty() {
        let dm = DisplayManager::new(false);
        // Just verify it returns a bool without panicking
        let _is_tty = dm.is_tty();
    }
}
