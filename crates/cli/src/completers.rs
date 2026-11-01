//! Dynamic shell completion helpers
//!
//! Provides package-name completion for shell tab completion via `CompleteEnv`.
//! These run on every TAB press and must be fast — no package parsing,
//! no command execution, just config load + directory listing.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use clap_complete::engine::CompletionCandidate;
use selfie::config::{YamlLoader, loader::ConfigLoader};
use selfie::fs::{FileSystem, RealFileSystem};

/// Complete package names from the configured package directory.
///
/// The directory is the one the configuration file names, found as every
/// command finds it. A `--package-directory` on the same command line does not
/// reach completion. Returns empty vec on any error (completion errors must be
/// silent).
pub(crate) fn complete_package_names(current: &OsStr) -> Vec<CompletionCandidate> {
    let current_str = current.to_string_lossy();
    let Some(package_dir) = resolve_package_directory(&RealFileSystem) else {
        return vec![];
    };

    list_package_names(&package_dir)
        .into_iter()
        .filter(|name| name.starts_with(current_str.as_ref()))
        .map(CompletionCandidate::new)
        .collect()
}

/// The package directory the configuration file names, or `None` when there is
/// no file, it does not load, or it names none.
fn resolve_package_directory(fs: &impl FileSystem) -> Option<PathBuf> {
    let loaded = YamlLoader::new(fs).load_config().ok()?;
    // The file's value alone, not `resolve`, which also requires `environment`:
    // a user who passes `--environment` on every command would get nothing.
    loaded.config().package_directory(fs)
}

/// List package names (file stems of `.yml`/`.yaml` files) in a directory.
///
/// A name a [`Name`](crate::terminal_text::Name) would show escaped is left
/// out: one holding a character the terminal would act on, one that renders as
/// nothing, or a blank other than a space. The shell inserts a candidate as
/// written, so it cannot be shown escaped, and a newline would split it into
/// two candidates.
fn list_package_names(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return vec![];
    };

    entries
        .filter_map(|e| e.ok())
        .filter_map(|entry| {
            let path = entry.path();
            let ext = path.extension()?.to_str()?;
            if ext.eq_ignore_ascii_case("yaml") || ext.eq_ignore_ascii_case("yml") {
                path.file_stem()?
                    .to_str()
                    .filter(|name| crate::terminal_text::Name::is_plain(name))
                    .map(String::from)
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::TempDir;

    // A name the shell would insert as an escape sequence, or split on a
    // newline, is not offered; an ordinary name beside it is.
    #[test]
    fn a_name_holding_a_control_or_hidden_character_is_not_offered() {
        let dir = TempDir::new().unwrap();
        for name in [
            "plain",
            "esc\u{1b}[2J",
            "rlo\u{202E}txt",
            "split\nname",
            "zero\u{200B}width",
            "no\u{A0}break",
            "vs\u{FE0F}",
        ] {
            std::fs::write(dir.path().join(format!("{name}.yml")), "").unwrap();
        }

        assert_eq!(list_package_names(dir.path()), ["plain"]);
    }

    #[test]
    fn list_package_names_finds_yaml_files() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("node.yml"), "").unwrap();
        std::fs::write(dir.path().join("rust.yaml"), "").unwrap();
        std::fs::write(dir.path().join("readme.md"), "").unwrap();

        let mut names = list_package_names(dir.path());
        names.sort();
        assert_eq!(names, vec!["node", "rust"]);
    }

    #[test]
    fn list_package_names_returns_empty_for_nonexistent_dir() {
        let names = list_package_names(Path::new("/nonexistent/path/that/does/not/exist"));
        assert!(names.is_empty());
    }
}
