//! Unified diff generation for config conflict display.
//!
//! When a config file has diverged on both the source and target sides, selfie
//! shows the user a diff so they can understand the conflict before deciding
//! what to do. This module wraps the [`similar`] crate's line-level diff engine
//! with selfie-specific labeling conventions (source path vs target path).

use similar::TextDiff;

/// Produce a unified diff between two strings, labeled with source/target paths.
///
/// Each label is kept to its header line: a line break in it is written as
/// `\n` or `\r`.
pub fn unified_diff(old: &str, new: &str, old_label: &str, new_label: &str) -> String {
    let diff = TextDiff::from_lines(old, new);
    let mut output = String::new();

    if diff.ratio() == 1.0 {
        return output; // Identical
    }

    // A path may hold a line break. Written raw, the rest of the label would
    // become a line of its own, which a reader of the diff takes for a hunk or
    // content line.
    let one_line = |label: &str| label.replace('\r', "\\r").replace('\n', "\\n");
    output.push_str(&format!("--- {}\n", one_line(old_label)));
    output.push_str(&format!("+++ {}\n", one_line(new_label)));

    for hunk in diff.unified_diff().context_radius(3).iter_hunks() {
        output.push_str(&hunk.to_string());
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_identical_files_produce_no_diff() {
        let diff = unified_diff("hello\nworld\n", "hello\nworld\n", "source", "target");
        assert!(diff.is_empty() || diff.trim().is_empty());
    }

    #[test]
    fn test_different_files_produce_diff() {
        let diff = unified_diff("hello\nworld\n", "hello\nearth\n", "source", "target");
        assert!(diff.contains("-world"));
        assert!(diff.contains("+earth"));
    }

    // A label holding a line break stays on its header line.
    #[test]
    fn a_label_holding_a_line_break_stays_on_its_line() {
        let diff = unified_diff("a\n", "b\n", "~/.a\n@@ -9 +9 @@", "src\r\nx");

        let lines: Vec<&str> = diff.lines().collect();
        assert_eq!(lines[0], "--- ~/.a\\n@@ -9 +9 @@");
        assert_eq!(lines[1], "+++ src\\r\\nx");
        assert_eq!(lines[2], "@@ -1 +1 @@");
    }

    #[test]
    fn test_diff_includes_file_labels() {
        let diff = unified_diff("a\n", "b\n", "repo/file.txt", "~/.config/file.txt");
        assert!(diff.contains("repo/file.txt"));
        assert!(diff.contains("~/.config/file.txt"));
    }
}
