//! Spotting config a user already added by hand, so a deploy can warn about
//! duplicating it.
//!
//! Someone may have put `eval "$(fnm env)"` in their `.zshrc` long before selfie
//! existed. [`find_related_lines`] finds lines mentioning a package name, and
//! [`is_shell_config_path`] recognizes the shell profiles where that is worth
//! saying.

/// A line in a file that is related to a package
pub struct RelatedLine {
    pub line_number: usize,
    pub content: String,
}

/// Scan file content for lines related to a package name.
/// Looks for: the package name itself (case-insensitive).
pub fn find_related_lines(content: &str, package_name: &str) -> Vec<RelatedLine> {
    let name_lower = package_name.to_lowercase();
    content
        .lines()
        .enumerate()
        .filter(|(_, line)| line.to_lowercase().contains(&name_lower))
        .map(|(i, line)| RelatedLine {
            line_number: i + 1,
            content: line.to_string(),
        })
        .collect()
}

/// Check if a target path looks like a shell config file.
pub fn is_shell_config_path(target: &str) -> bool {
    let shell_patterns = [
        ".bashrc",
        ".bash_profile",
        ".zshrc",
        ".zprofile",
        ".profile",
        "config.fish",
        ".zshenv",
    ];
    shell_patterns.iter().any(|p| target.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_related_lines_finds_eval_pattern() {
        let content = "# some config\neval \"$(fnm env)\"\n# more config\n";
        let matches = find_related_lines(content, "fnm");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].line_number, 2);
    }

    #[test]
    fn test_find_related_lines_finds_source_pattern() {
        let content = "source ~/.config/fnm/init.sh\n";
        let matches = find_related_lines(content, "fnm");
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn test_find_related_lines_no_matches() {
        let content = "export PATH=/usr/bin:$PATH\n";
        let matches = find_related_lines(content, "fnm");
        assert!(matches.is_empty());
    }

    #[test]
    fn test_find_related_lines_skips_comments_about_package() {
        // Comments mentioning the package name should still be found
        // (they indicate the user has config for this package)
        let content = "# fnm setup\neval \"$(fnm env)\"\n";
        let matches = find_related_lines(content, "fnm");
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn test_is_shell_config_file() {
        assert!(is_shell_config_path("~/.bashrc"));
        assert!(is_shell_config_path("~/.zshrc"));
        assert!(is_shell_config_path("~/.config/fish/config.fish"));
        assert!(!is_shell_config_path("~/.config/alacritty/alacritty.toml"));
    }

    #[test]
    fn test_is_shell_config_path_profile_files() {
        assert!(is_shell_config_path("~/.bash_profile"));
        assert!(is_shell_config_path("~/.zprofile"));
        assert!(is_shell_config_path("~/.profile"));
        assert!(is_shell_config_path("~/.zshenv"));
    }
}
