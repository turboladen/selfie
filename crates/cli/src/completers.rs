//! Dynamic shell completion helpers
//!
//! Provides package-name completion for shell tab completion via `CompleteEnv`.
//! These run on every TAB press and must be fast — no package parsing,
//! no command execution, just config load + directory listing.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use clap_complete::engine::CompletionCandidate;
use etcetera::AppStrategy;

/// Complete package names from the configured package directory.
///
/// Loads config, lists YAML files, strips extensions, filters by prefix.
/// Returns empty vec on any error (completion errors must be silent).
pub(crate) fn complete_package_names(current: &OsStr) -> Vec<CompletionCandidate> {
    let current_str = current.to_string_lossy();
    let Some(package_dir) = resolve_package_directory() else {
        return vec![];
    };

    list_package_names(&package_dir)
        .into_iter()
        .filter(|name| name.starts_with(current_str.as_ref()))
        .map(CompletionCandidate::new)
        .collect()
}

/// Resolve the package directory from the selfie config file.
///
/// Uses the same config-dir strategy as `RealFileSystem` (`etcetera`)
/// so the completer finds the same config the app does.
fn resolve_package_directory() -> Option<PathBuf> {
    let strategy = etcetera::choose_app_strategy(etcetera::AppStrategyArgs {
        top_level_domain: "com".to_string(),
        author: "selfie".to_string(),
        app_name: "selfie".to_string(),
    })
    .ok()?;

    let config_dir = strategy.config_dir();
    let config_path = ["config.yml", "config.yaml"]
        .iter()
        .map(|name| config_dir.join(name))
        .find(|p: &PathBuf| p.exists())?;

    let contents = std::fs::read_to_string(config_path).ok()?;
    let config: HashMap<String, String> = serde_saphyr::from_str(&contents).ok()?;
    let pkg_dir_str = config.get("package_directory")?;
    let expanded = shellexpand::tilde(pkg_dir_str.as_str());
    let path = PathBuf::from(expanded.as_ref());

    if path.is_dir() { Some(path) } else { None }
}

/// List package names (file stems of `.yml`/`.yaml` files) in a directory.
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
                path.file_stem()?.to_str().map(String::from)
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::ffi::OsString;

    use tempfile::TempDir;

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

    #[test]
    fn complete_package_names_returns_empty_without_config() {
        // With no real config, this should silently return empty
        let result = complete_package_names(&OsString::from("nonexistent-prefix-xyz"));
        assert!(result.is_empty());
    }
}
