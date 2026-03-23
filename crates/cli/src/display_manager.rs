//! indicatif-powered display layer for CLI output
//!
//! Provides spinners, progress tracking, styled output, and structured error
//! summaries. Replaces `TerminalProgressReporter` with a unified display system
//! that handles both event-driven and static output consistently.

use std::collections::VecDeque;
use std::fmt::Display;
use std::path::Path;
use std::sync::{Arc, Mutex};

use console::style;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

/// Shorten a path for display by replacing the home directory with `~`.
pub(crate) fn shorten_path(path: &str) -> String {
    if let Ok(home) = std::env::var("HOME")
        && let Some(rest) = path.strip_prefix(&home)
    {
        return format!("~{rest}");
    }
    path.to_string()
}

/// Shorten a path for display, accepting a `&Path`.
#[allow(dead_code)]
pub(crate) fn shorten_display_path(path: &Path) -> String {
    shorten_path(&path.display().to_string())
}

/// Standard indentation for structured CLI output (e.g., section content,
/// result cards, list suggestions, and other indented fields).
pub(crate) const INDENT: &str = "   ";

/// Structured error detail for the end-of-operation summary
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
    #[cfg(test)]
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
            lines.push(format!("✗ {} ({})", error.package_name, error.operation));
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
    pub(crate) fn add_output_line(&mut self, line: &str) {
        let prefix = "  │ ";
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
    mp: MultiProgress,
    use_colors: bool,
    is_tty: bool,
    errors: Arc<Mutex<ErrorCollector>>,
}

impl DisplayManager {
    /// Create a new display manager
    pub fn new(use_colors: bool) -> Self {
        let is_tty = console::Term::stderr().is_term() && console::Term::stdout().is_term();
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
    // Canonical symbol set:
    //   ✓  success (green)      ✗  error/failure (red)
    //   ⚠  warning (yellow)     ℹ  info (blue)
    //   ✨ suggestion (yellow)   ── title ──  section header (bold)
    //
    // All output routes through mp.suspend() to avoid interleaving with
    // active spinners. This pauses the draw target, writes to the correct
    // stream (stdout or stderr), then resumes. Safe when no spinners are active.

    /// Print an informational message (stdout)
    pub(crate) fn print_info(&self, message: impl Display) {
        if self.use_colors {
            self.mp
                .suspend(|| println!("{} {}", style("ℹ").blue(), style(message).blue()));
        } else {
            self.mp.suspend(|| println!("ℹ {message}"));
        }
    }

    /// Print an error message (stderr)
    pub(crate) fn print_error(&self, message: impl Display) {
        if self.use_colors {
            self.mp.suspend(|| {
                eprintln!(
                    "{} {}",
                    style("✗").red().bold(),
                    style(message).red().bold()
                )
            });
        } else {
            self.mp.suspend(|| eprintln!("✗ {message}"));
        }
    }

    /// Print a success message (stdout)
    pub(crate) fn print_success(&self, message: impl Display) {
        if self.use_colors {
            self.mp
                .suspend(|| println!("{} {}", style("✓").green().bold(), style(message).green()));
        } else {
            self.mp.suspend(|| println!("✓ {message}"));
        }
    }

    /// Print a warning message (stderr)
    pub(crate) fn print_warning(&self, message: impl Display) {
        if self.use_colors {
            self.mp.suspend(|| {
                eprintln!(
                    "{} {}",
                    style("⚠").yellow().bold(),
                    style(message).yellow().bold()
                )
            });
        } else {
            self.mp.suspend(|| eprintln!("⚠ {message}"));
        }
    }

    /// Print a progress/status message (stdout)
    pub(crate) fn print_progress(&self, message: impl Display) {
        if self.use_colors {
            self.mp.suspend(|| println!("  {}", style(message).dim()));
        } else {
            self.mp.suspend(|| println!("  {message}"));
        }
    }

    /// Print a suggestion message (stdout)
    pub(crate) fn print_suggestion(&self, message: impl Display) {
        if self.use_colors {
            self.mp.suspend(|| {
                println!(
                    "{} {}: {}",
                    style("✨").bold(),
                    style("Suggestion").yellow().bold(),
                    message
                )
            });
        } else {
            self.mp.suspend(|| println!("✨ Suggestion: {message}"));
        }
    }

    /// Print a section header (stdout)
    pub(crate) fn print_section_header(&self, title: impl Display) {
        if self.use_colors {
            self.mp
                .suspend(|| println!("── {} ──", style(&title).bold()));
        } else {
            self.mp.suspend(|| println!("── {title} ──"));
        }
    }

    /// Print a unified diff with per-line coloring and visual framing (stdout)
    ///
    /// Layout:
    /// ```text
    /// --- old/path
    /// +++ new/path
    /// ──────────────────────────────────────────
    ///  context line
    /// -removed line
    /// +added line
    /// ──────────────────────────────────────────
    /// ```
    ///
    /// Colors: `---` red, `+++` green, `@@` cyan, `-` red, `+` green,
    /// context dim, separator dim. Paths shortened with `~`.
    pub(crate) fn print_diff(&self, diff: &str) {
        const SEPARATOR: &str =
            "──────────────────────────────────────────────────────────────────────";

        self.mp.suspend(|| {
            let mut printed_separator = false;

            for line in diff.lines() {
                if self.use_colors {
                    if let Some(rest) = line.strip_prefix("--- ") {
                        println!("  {}", style(format!("--- {}", shorten_path(rest))).red());
                    } else if let Some(rest) = line.strip_prefix("+++ ") {
                        println!("  {}", style(format!("+++ {}", shorten_path(rest))).green());
                        println!("  {}", style(SEPARATOR).dim());
                        printed_separator = true;
                    } else if line.starts_with("@@") {
                        println!("  {}", style(line).cyan());
                    } else if line.starts_with('-') {
                        println!("  {}", style(line).red());
                    } else if line.starts_with('+') {
                        println!("  {}", style(line).green());
                    } else {
                        println!("  {}", style(line).dim());
                    }
                } else if let Some(rest) = line.strip_prefix("--- ") {
                    println!("  --- {}", shorten_path(rest));
                } else if let Some(rest) = line.strip_prefix("+++ ") {
                    println!("  +++ {}", shorten_path(rest));
                    println!("  {SEPARATOR}");
                    printed_separator = true;
                } else {
                    println!("  {line}");
                }
            }

            if printed_separator {
                if self.use_colors {
                    println!("  {}", style(SEPARATOR).dim());
                } else {
                    println!("  {SEPARATOR}");
                }
            }
        });
    }

    /// Print a plain line to stdout
    pub(crate) fn println(&self, message: impl Display) {
        self.mp.suspend(|| println!("{message}"));
    }

    /// Print a styled key-value pair (stdout, for config display, etc.)
    pub(crate) fn print_field(&self, key: impl Display, value: impl Display) {
        if self.use_colors {
            self.mp
                .suspend(|| println!("  {} {}", style(key).italic().dim(), style(value).bold()));
        } else {
            self.mp.suspend(|| println!("  {key} {value}"));
        }
    }

    /// Whether the output is a TTY (interactive terminal)
    pub(crate) fn is_tty(&self) -> bool {
        self.is_tty
    }

    // ── Result card builder ─────────────────────────────────────────────

    /// Create a structured result card (section header + key-value pairs)
    ///
    /// Usage:
    /// ```ignore
    /// display.result_card("Check Results")
    ///     .field("Package", &package_name)
    ///     .field("Environment", &environment)
    ///     .field_if("Command", check_command.as_deref())
    ///     .print();
    /// ```
    pub(crate) fn result_card(&self, title: impl Display) -> ResultCard<'_> {
        ResultCard::new(self, title)
    }

    // ── Dynamic output methods ─────────────────────────────────────────

    /// Start a new operation with a spinner
    ///
    /// Returns an `OperationHandle` that can be used to update progress,
    /// show command output, and finalize the operation display.
    pub(crate) fn start_operation(&self, message: impl Display) -> OperationHandle {
        self.create_spinner(message, 5)
    }

    /// Create a spinner for a list item (used by package list command)
    ///
    /// Unlike `start_operation()`, this creates a spinner optimized for
    /// sorted lists: the spinner resolves in-place to preserve ordering.
    pub(crate) fn start_list_spinner(&self, message: impl Display) -> OperationHandle {
        self.create_spinner(message, 0)
    }

    /// Create a spinner with the given max output lines
    fn create_spinner(&self, message: impl Display, max_output_lines: usize) -> OperationHandle {
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
            max_output_lines,
            mp: self.mp.clone(),
        }
    }

    // ── Error collection ───────────────────────────────────────────────

    /// Collect a structured error for the end-of-operation summary
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
        // Extract summary while holding the lock, then drop it before
        // doing terminal I/O to avoid blocking concurrent collect_error() calls.
        let summary = {
            if let Ok(collector) = self.errors.lock() {
                collector.format_summary()
            } else {
                None
            }
        };

        if let Some(summary) = summary {
            self.mp.suspend(|| eprintln!("{summary}"));
        }
    }

    /// Check if any errors have been collected
    #[cfg(test)]
    pub(crate) fn has_errors(&self) -> bool {
        self.errors.lock().map(|c| c.has_errors()).unwrap_or(false)
    }

    /// Return a snapshot of all collected errors (test-only)
    #[cfg(test)]
    pub(crate) fn collected_errors(&self) -> Vec<ErrorDetail> {
        self.errors
            .lock()
            .map(|c| c.errors.clone())
            .unwrap_or_default()
    }
}

/// Builder for structured result cards (section header + key-value pairs)
///
/// Created by [`DisplayManager::result_card()`]. Call `.field()` / `.field_if()`
/// to add rows, then `.print()` to render.
pub(crate) struct ResultCard<'a> {
    display: &'a DisplayManager,
    title: String,
    fields: Vec<(String, String)>,
}

impl<'a> ResultCard<'a> {
    fn new(display: &'a DisplayManager, title: impl Display) -> Self {
        Self {
            display,
            title: title.to_string(),
            fields: Vec::new(),
        }
    }

    /// Add a key-value field to the card
    pub(crate) fn field(mut self, key: &str, value: impl Display) -> Self {
        self.fields.push((key.to_string(), value.to_string()));
        self
    }

    /// Add a field only if the value is `Some`
    pub(crate) fn field_if(mut self, key: &str, value: Option<impl Display>) -> Self {
        if let Some(v) = value {
            self.fields.push((key.to_string(), v.to_string()));
        }
        self
    }

    /// Render the card to the display
    pub(crate) fn print(self) {
        use crate::formatters::format_key;

        self.display.println("");
        self.display.print_section_header(&self.title);

        let use_colors = self.display.use_colors();
        for (key, value) in &self.fields {
            self.display.println(format!(
                "{}{}: {}",
                INDENT,
                format_key(key, use_colors),
                value
            ));
        }
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
        dm.print_section_header("test section");
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
        handle.add_output_line("line 1");
        handle.add_output_line("line 2");
        assert_eq!(handle.output_lines.len(), 2);

        // Add more than max to test rolling window
        for i in 0..10 {
            handle.add_output_line(&format!("line {i}"));
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
    fn test_result_card_basic() {
        let dm = DisplayManager::new(false);
        // Verify the builder API works without panicking
        dm.result_card("Test Results")
            .field("Package", "test-pkg")
            .field("Environment", "macos")
            .print();
    }

    #[test]
    fn test_result_card_with_colors() {
        let dm = DisplayManager::new(true);
        dm.result_card("Test Results")
            .field("Package", "test-pkg")
            .field("Status", "valid")
            .print();
    }

    #[test]
    fn test_result_card_field_if() {
        let dm = DisplayManager::new(false);
        let cmd: Option<&str> = Some("brew install foo");
        let missing: Option<&str> = None;

        dm.result_card("Test Results")
            .field("Package", "test-pkg")
            .field_if("Command", cmd)
            .field_if("Missing", missing)
            .print();
    }

    #[test]
    fn test_static_output_during_active_spinner() {
        let dm = DisplayManager::new(false);
        let handle = dm.start_operation("Working...");
        // These should not panic even with an active spinner
        dm.print_info("info during spinner");
        dm.print_warning("warning during spinner");
        dm.print_error("error during spinner");
        dm.print_success("success during spinner");
        dm.println("plain during spinner");
        dm.print_section_header("header during spinner");
        dm.print_progress("progress during spinner");
        dm.print_suggestion("suggestion during spinner");
        dm.print_field("key:", "value");
        handle.finish_clear();
    }

    #[test]
    fn test_is_tty_false_when_stdout_piped() {
        // Test runners pipe stdout, so is_tty must be false.
        // This validates that we check BOTH stdout and stderr —
        // if we only checked stderr (which may still be a terminal),
        // is_tty could incorrectly return true when stdout is piped,
        // causing list output to go to stderr via MultiProgress.
        let dm = DisplayManager::new(false);
        assert!(
            !dm.is_tty(),
            "is_tty should be false when stdout is piped (as in test runners)"
        );
    }
}
