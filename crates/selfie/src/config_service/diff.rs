use similar::TextDiff;

/// Produce a unified diff between two strings, labeled with source/target paths.
pub fn unified_diff(old: &str, new: &str, old_label: &str, new_label: &str) -> String {
    let diff = TextDiff::from_lines(old, new);
    let mut output = String::new();

    if diff.ratio() == 1.0 {
        return output; // Identical
    }

    output.push_str(&format!("--- {old_label}\n"));
    output.push_str(&format!("+++ {new_label}\n"));

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

    #[test]
    fn test_diff_includes_file_labels() {
        let diff = unified_diff("a\n", "b\n", "repo/file.txt", "~/.config/file.txt");
        assert!(diff.contains("repo/file.txt"));
        assert!(diff.contains("~/.config/file.txt"));
    }
}
