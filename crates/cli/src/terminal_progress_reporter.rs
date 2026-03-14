//! Terminal progress reporting and output formatting
//!
//! This module provides a consistent interface for displaying progress updates,
//! status messages, and other user feedback in the terminal. It handles emoji
//! fallbacks for terminals that don't support Unicode and provides colored
//! output when appropriate.
//!
//! # Features
//!
//! - Consistent emoji prefixes for different message types
//! - Graceful fallback to text indicators when emojis aren't supported
//! - Colored output with automatic color detection
//! - Support for different message severity levels
//! - Structured formatting for various UI contexts
//!
//! # Examples
//!
//! ```rust
//! use crate::terminal_progress_reporter::TerminalProgressReporter;
//!
//! let reporter = TerminalProgressReporter::new(true); // Enable colors
//! reporter.report_success("Package installed successfully");
//! reporter.report_error("Failed to install package");
//! reporter.report_progress("Installing dependencies...");
//! ```

use std::fmt::Display;

use console::{Emoji, style};

// Define emojis with fallbacks for terminals that don't support Unicode
static ERROR_EMOJI: Emoji<'_, '_> = Emoji("❌ ", "[E] ");
static INFO_EMOJI: Emoji<'_, '_> = Emoji("ℹ️ ", "[I] ");
static PROGRESS_EMOJI: Emoji<'_, '_> = Emoji("• ", " • ");
static SUGGESTION_EJOJI: Emoji<'_, '_> = Emoji("✨", "OK ");
static SUCCESS_EMOJI: Emoji<'_, '_> = Emoji("✅ ", "OK ");
static WARN_EMOJI: Emoji<'_, '_> = Emoji("⚠️ ", "[W] ");

// Status-specific emojis for package status indicators
static INSTALLED_EMOJI: Emoji<'_, '_> = Emoji("✅ ", "[✓] ");
static NOT_INSTALLED_EMOJI: Emoji<'_, '_> = Emoji("📦 ", "[×] ");
static NO_CHECK_EMOJI: Emoji<'_, '_> = Emoji("⚠️ ", "[?] ");
static CMD_NOT_FOUND_EMOJI: Emoji<'_, '_> = Emoji("🔍 ", "[!] ");
static STATUS_ERROR_EMOJI: Emoji<'_, '_> = Emoji("💥 ", "[E] ");
static NA_EMOJI: Emoji<'_, '_> = Emoji("⚪ ", "[N/A] ");

// Output line emojis for installation display
static STDOUT_OUTPUT_EMOJI: Emoji<'_, '_> = Emoji("📦 ", "[o] ");
static STDERR_OUTPUT_EMOJI: Emoji<'_, '_> = Emoji("🔧 ", "[e] ");
static OUTPUT_HEADER_EMOJI: Emoji<'_, '_> = Emoji("📋 ", ">> ");

/// Types of status messages that can be displayed to the user
///
/// Each message type has its own visual styling, emoji/text prefix,
/// and color scheme to help users quickly understand the nature
/// of the information being presented.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum MessageType {
    /// Error messages for failures and critical issues
    Error,
    /// Informational messages for general status updates
    Info,
    /// Progress indicators for ongoing operations
    Progress,
    /// Success messages for completed operations
    Success,
    /// Helpful suggestions and recommendations
    Suggestion,
    /// Warning messages for potential issues
    Warning,
}

/// Terminal progress reporter for consistent CLI output formatting
///
/// Provides a unified interface for displaying various types of messages
/// to the user with appropriate styling, colors, and emoji indicators.
/// Automatically handles fallbacks for terminals with limited capabilities.
#[derive(Debug, Clone, Copy)]
pub struct TerminalProgressReporter {
    /// Whether to use colored output (respects user preference and terminal capabilities)
    use_colors: bool,
}

impl TerminalProgressReporter {
    /// Create a new terminal progress reporter
    ///
    /// # Arguments
    ///
    /// * `use_colors` - Whether to enable colored output formatting
    ///
    /// # Examples
    ///
    /// ```rust
    /// let reporter = TerminalProgressReporter::new(true);  // With colors
    /// let reporter = TerminalProgressReporter::new(false); // Plain text only
    /// ```
    #[must_use]
    pub fn new(use_colors: bool) -> Self {
        Self { use_colors }
    }

    /// Check if colors are enabled for this reporter
    ///
    /// # Returns
    ///
    /// `true` if colored output is enabled, `false` otherwise
    #[must_use]
    pub fn use_colors(self) -> bool {
        self.use_colors
    }
}

impl TerminalProgressReporter {
    /// Format a status line with appropriate styling and prefix
    ///
    /// Creates a formatted status line with the appropriate emoji/text prefix
    /// and color styling based on the message type and color settings.
    ///
    /// # Arguments
    ///
    /// * `message_type` - The type of message to format
    /// * `message` - The message content to display
    ///
    /// # Returns
    ///
    /// A formatted string ready for display in the terminal
    pub(crate) fn status_line(self, message_type: MessageType, message: impl Display) -> String {
        let prefix = match message_type {
            MessageType::Error => ERROR_EMOJI,
            MessageType::Info => INFO_EMOJI,
            MessageType::Progress => PROGRESS_EMOJI,
            MessageType::Success => SUCCESS_EMOJI,
            MessageType::Suggestion => SUGGESTION_EJOJI,
            MessageType::Warning => WARN_EMOJI,
        };

        let formatted_message = if self.use_colors {
            match message_type {
                MessageType::Error => style(message).for_stderr().red().bold().to_string(),
                MessageType::Info => style(message).blue().to_string(),
                MessageType::Progress => style(message).dim().to_string(),
                MessageType::Success => style(message).green().to_string(),
                MessageType::Suggestion => {
                    return format!(
                        "{prefix} {}: {}",
                        style("Suggestion").yellow().bold(),
                        &message
                    );
                }
                MessageType::Warning => style(message).for_stderr().yellow().bold().to_string(),
            }
        } else {
            message.to_string()
        };

        format!("{prefix}{formatted_message}")
    }

    /// Format a status line with custom emoji and styling
    ///
    /// Creates a formatted status line with the provided emoji and custom styling function.
    ///
    /// # Arguments
    ///
    /// * `emoji` - The emoji/text prefix to use
    /// * `message` - The message content to display
    /// * `style_fn` - Function to apply styling when colors are enabled
    ///
    /// # Returns
    ///
    /// A formatted string ready for display in the terminal
    fn status_line_with_emoji<F>(
        self,
        emoji: Emoji<'_, '_>,
        message: impl Display,
        style_fn: F,
    ) -> String
    where
        F: Fn(console::StyledObject<String>) -> console::StyledObject<String>,
    {
        let formatted_message = if self.use_colors {
            style_fn(style(message.to_string())).to_string()
        } else {
            message.to_string()
        };

        format!("{emoji}{formatted_message}")
    }

    /// Format a message with the specified indentation
    ///
    /// Adds leading whitespace to create visual hierarchy in terminal output.
    /// Useful for nested information or sub-items in lists.
    ///
    /// # Arguments
    ///
    /// * `indent` - Number of spaces to indent the message
    /// * `message` - The message content to format
    ///
    /// # Returns
    ///
    /// The message with appropriate leading whitespace
    pub(crate) fn format(indent: usize, message: impl Display) -> String {
        format!("{:indent$}{}", "", message, indent = indent)
    }

    /// Format an error message with appropriate styling
    ///
    /// Creates a formatted error message with red coloring (if enabled)
    /// and an error emoji/indicator prefix.
    pub(crate) fn format_error(self, message: impl Display) -> String {
        self.status_line(MessageType::Error, message)
    }

    /// Format an informational message with appropriate styling
    ///
    /// Creates a formatted info message with blue coloring (if enabled)
    /// and an info emoji/indicator prefix.
    pub(crate) fn format_info(self, message: impl Display) -> String {
        self.status_line(MessageType::Info, message)
    }

    /// Format a progress message with appropriate styling
    ///
    /// Creates a formatted progress message with dim coloring (if enabled)
    /// and a progress emoji/indicator prefix.
    pub(crate) fn format_progress(self, message: impl Display) -> String {
        self.status_line(MessageType::Progress, message)
    }

    /// Format a suggestion message with appropriate styling
    ///
    /// Creates a formatted suggestion message with yellow coloring (if enabled)
    /// and a suggestion emoji/indicator prefix.
    pub(crate) fn format_suggestion(self, message: impl Display) -> String {
        self.status_line(MessageType::Suggestion, message)
    }

    /// Format a success message with appropriate styling
    ///
    /// Creates a formatted success message with green coloring (if enabled)
    /// and a success emoji/indicator prefix.
    pub(crate) fn format_success(self, message: impl Display) -> String {
        self.status_line(MessageType::Success, message)
    }

    /// Format a warning message with appropriate styling
    ///
    /// Creates a formatted warning message with yellow coloring (if enabled)
    /// and a warning emoji/indicator prefix.
    pub(crate) fn format_warning(self, message: impl Display) -> String {
        self.status_line(MessageType::Warning, message)
    }

    /// Print a message with the specified indentation to stdout
    ///
    /// Convenience method for printing indented messages without specific styling.
    ///
    /// # Arguments
    ///
    /// * `indent` - Number of spaces to indent the message
    /// * `message` - The message content to print
    pub(crate) fn report(indent: usize, message: impl Display) {
        println!("{}", Self::format(indent, message));
    }

    /// Print a formatted progress message to stdout
    ///
    /// Displays a progress message with appropriate styling and prefix.
    /// Useful for showing ongoing operation status to the user.
    pub(crate) fn report_progress(self, message: impl Display) {
        println!("{}", self.format_progress(message));
    }

    /// Print a formatted success message to stdout
    ///
    /// Displays a success message with green styling and success indicator.
    /// Used to confirm successful completion of operations.
    pub(crate) fn report_success(self, message: impl Display) {
        println!("{}", self.format_success(message));
    }

    /// Print a formatted suggestion message to stdout
    ///
    /// Displays a suggestion message with yellow styling and suggestion indicator.
    /// Used to provide helpful recommendations to the user.
    pub(crate) fn report_suggestion(self, message: impl Display) {
        println!("{}", self.format_suggestion(message));
    }

    /// Print a formatted informational message to stdout
    ///
    /// Displays an info message with blue styling and info indicator.
    /// Used for general status updates and non-critical information.
    pub(crate) fn report_info(self, message: impl Display) {
        println!("{}", self.format_info(message));
    }

    /// Print a formatted warning message to stdout
    ///
    /// Displays a warning message with yellow styling and warning indicator.
    /// Used to alert users to potential issues that don't prevent operation.
    pub(crate) fn report_warning(self, message: impl Display) {
        println!("{}", self.format_warning(message));
    }

    /// Print a formatted error message to stderr
    ///
    /// Displays an error message with red styling and error indicator.
    /// Uses stderr for proper error stream handling in scripts and pipelines.
    pub(crate) fn report_error(self, message: impl Display) {
        eprintln!("{}", self.format_error(message));
    }

    // Status-specific formatting methods

    /// Format an "installed" status message
    ///
    /// Creates a formatted message indicating a package is installed,
    /// with green styling and a checkmark indicator.
    pub(crate) fn format_installed(self) -> String {
        self.status_line_with_emoji(INSTALLED_EMOJI, "Installed", |style| style.green())
    }

    /// Format a "not installed" status message
    ///
    /// Creates a formatted message indicating a package is not installed,
    /// with cyan styling and a package indicator.
    pub(crate) fn format_not_installed(self) -> String {
        self.status_line_with_emoji(NOT_INSTALLED_EMOJI, "Not installed", |style| style.cyan())
    }

    /// Format a "no check command" status message
    ///
    /// Creates a formatted message indicating no check command is configured,
    /// with yellow styling and a warning indicator.
    pub(crate) fn format_no_check(self) -> String {
        self.status_line_with_emoji(NO_CHECK_EMOJI, "No check", |style| style.yellow())
    }

    /// Format a "command not found" status message
    ///
    /// Creates a formatted message indicating the check command was not found,
    /// with red styling and a search indicator.
    pub(crate) fn format_cmd_not_found(self) -> String {
        self.status_line_with_emoji(CMD_NOT_FOUND_EMOJI, "Cmd not found", |style| style.red())
    }

    /// Format a status check error message
    ///
    /// Creates a formatted message indicating an error occurred during status check,
    /// with red styling and an error indicator.
    pub(crate) fn format_status_error(self) -> String {
        self.status_line_with_emoji(STATUS_ERROR_EMOJI, "Error", |style| style.red())
    }

    /// Format a "not available" status message
    ///
    /// Creates a formatted message indicating status is not available,
    /// with dim styling and a neutral indicator.
    pub(crate) fn format_na(self) -> String {
        self.status_line_with_emoji(NA_EMOJI, "N/A", |style| style.dim())
    }

    /// Format a stdout output line with appropriate emoji prefix
    pub(crate) fn format_stdout_output(self, line: &str) -> String {
        let trimmed = line.trim();
        let text = if self.use_colors {
            style(trimmed).dim().to_string()
        } else {
            trimmed.to_string()
        };
        format!("    {STDOUT_OUTPUT_EMOJI}{text}")
    }

    /// Format a stderr output line with appropriate emoji prefix
    pub(crate) fn format_stderr_output(self, line: &str) -> String {
        let trimmed = line.trim();
        let text = if self.use_colors {
            style(trimmed).dim().to_string()
        } else {
            trimmed.to_string()
        };
        format!("    {STDERR_OUTPUT_EMOJI}{text}")
    }

    /// Format an installation output header
    pub(crate) fn format_output_header(self) -> String {
        let label = "Installation output:";
        let text = if self.use_colors {
            style(label).bold().to_string()
        } else {
            label.to_string()
        };
        format!("\n{OUTPUT_HEADER_EMOJI}{text}")
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_terminal_reporter_formatting() {
        // Test with colors enabled
        let reporter = TerminalProgressReporter::new(true);

        // Check formatting of different message types
        let success_msg = reporter.format_success("Test successful");
        let error_msg = reporter.format_error("Test failed");
        let info_msg = reporter.format_info("Test information");

        // Verify prefixes are included
        assert!(success_msg.contains("Test successful"));
        assert!(error_msg.contains("Test failed"));
        assert!(info_msg.contains("Test information"));

        // Prefix indicators should be present
        assert!(success_msg.contains("✅") || success_msg.contains("OK"));
        assert!(error_msg.contains("❌") || error_msg.contains("[E]"));
        assert!(info_msg.contains("ℹ️") || info_msg.contains("[I]"));
    }

    #[test]
    fn test_terminal_reporter_without_colors() {
        // Test with colors disabled
        let reporter = TerminalProgressReporter::new(false);

        let success_msg = reporter.format_success("Test successful");

        // Message should be plain text without ANSI color codes
        assert!(!success_msg.contains("\x1b["));
    }

    #[test]
    fn test_status_formatting_with_colors() {
        let reporter = TerminalProgressReporter::new(true);

        // Test status-specific formatting methods
        let installed = reporter.format_installed();
        let not_installed = reporter.format_not_installed();
        let no_check = reporter.format_no_check();
        let cmd_not_found = reporter.format_cmd_not_found();
        let status_error = reporter.format_status_error();
        let na = reporter.format_na();

        // Verify text content
        assert!(installed.contains("Installed"));
        assert!(not_installed.contains("Not installed"));
        assert!(no_check.contains("No check"));
        assert!(cmd_not_found.contains("Cmd not found"));
        assert!(status_error.contains("Error"));
        assert!(na.contains("N/A"));

        // Verify emoji fallbacks are present
        assert!(installed.contains("✅") || installed.contains("[✓]"));
        assert!(not_installed.contains("📦") || not_installed.contains("[×]"));
        assert!(no_check.contains("⚠️") || no_check.contains("[?]"));
        assert!(cmd_not_found.contains("🔍") || cmd_not_found.contains("[!]"));
        assert!(status_error.contains("💥") || status_error.contains("[E]"));
        assert!(na.contains("⚪") || na.contains("[N/A]"));
    }

    #[test]
    fn test_status_formatting_without_colors() {
        let reporter = TerminalProgressReporter::new(false);

        let installed = reporter.format_installed();

        // Should not contain ANSI color codes
        assert!(!installed.contains("\x1b["));
        // But should still contain the text and emoji/fallback
        assert!(installed.contains("Installed"));
        assert!(installed.contains("✅") || installed.contains("[✓]"));
    }

    #[test]
    fn test_format_stdout_output_without_colors() {
        let reporter = TerminalProgressReporter::new(false);
        let result = reporter.format_stdout_output("hello world");
        assert!(result.contains("hello world"));
        assert!(result.contains("📦") || result.contains("[o]"));
        assert!(result.starts_with("    ")); // 4-space indent
        assert!(!result.contains("\x1b[")); // No ANSI codes
    }

    #[test]
    fn test_format_stdout_output_with_colors() {
        let reporter = TerminalProgressReporter::new(true);
        let result = reporter.format_stdout_output("hello world");
        assert!(result.contains("hello world"));
        assert!(result.contains("📦") || result.contains("[o]"));
        assert!(result.starts_with("    ")); // 4-space indent
    }

    #[test]
    fn test_format_stderr_output_without_colors() {
        let reporter = TerminalProgressReporter::new(false);
        let result = reporter.format_stderr_output("  some error  ");
        assert!(result.contains("some error"));
        assert!(result.contains("🔧") || result.contains("[e]"));
        assert!(result.starts_with("    ")); // 4-space indent
        assert!(!result.contains("\x1b[")); // No ANSI codes
    }

    #[test]
    fn test_format_stderr_output_with_colors() {
        let reporter = TerminalProgressReporter::new(true);
        let result = reporter.format_stderr_output("  some error  ");
        assert!(result.contains("some error"));
        assert!(result.contains("🔧") || result.contains("[e]"));
        assert!(result.starts_with("    ")); // 4-space indent
    }

    #[test]
    fn test_format_output_header_without_colors() {
        let reporter = TerminalProgressReporter::new(false);
        let result = reporter.format_output_header();
        assert!(result.contains("Installation output:"));
        assert!(result.contains("📋") || result.contains(">>"));
        assert!(!result.contains("\x1b[")); // No ANSI codes
    }

    #[test]
    fn test_format_output_header_with_colors() {
        let reporter = TerminalProgressReporter::new(true);
        let result = reporter.format_output_header();
        assert!(result.contains("Installation output:"));
        assert!(result.contains("📋") || result.contains(">>"));
    }

    #[test]
    fn test_format_stdout_output_trims_whitespace() {
        let reporter = TerminalProgressReporter::new(false);
        let result = reporter.format_stdout_output("  padded line  ");
        assert!(result.contains("padded line"));
        // Should not contain the original leading/trailing spaces around the text
        assert!(!result.contains("  padded"));
    }
}
