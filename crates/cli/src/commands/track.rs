//! Interactive track command handler
//!
//! This module handles the `selfie track <file>` CLI command — an interactive
//! shortcut that prompts the user to choose whether the file belongs to an
//! existing package or should become a new standalone dotfile, then delegates
//! to the appropriate tracking handler.

use dialoguer::{FuzzySelect, Input, theme::ColorfulTheme};
use selfie::{
    fs::real::RealFileSystem,
    namespace,
    package::{port::PackageRepository, repository::yaml::YamlPackageRepository},
};
use tracing::info;

use crate::{
    commands::common::{self, create_package_repository},
    config::CliConfig,
    display_manager::DisplayManager,
};

/// Sentinel item appended after real package names in the select list.
const NEW_STANDALONE: &str = "→ New standalone dotfile";
/// Sentinel item for free-text name entry.
const TYPE_A_NAME: &str = "→ Let me type a name";

/// What the user chose in the interactive prompt.
#[derive(Debug, PartialEq)]
enum TrackChoice {
    /// Add the file to an existing package
    ExistingPackage(String),
    /// Create a new standalone dotfile with the given name
    NewStandalone(String),
    /// User cancelled
    Cancelled,
}

/// Handle the `selfie track` interactive command
pub(crate) async fn handle_track(file: &str, config: &CliConfig, display: &DisplayManager) -> i32 {
    info!("Interactive track for '{}'", file);

    let repo = create_package_repository(config);

    // Check if this file is already tracked anywhere
    if let Some(pkg_name) = find_existing_tracker(file, config) {
        display.print_info(format!(
            "Already tracking '{}' in spec '{}'",
            file, pkg_name
        ));
        return 0;
    }

    // Collect available package names for the prompt
    let package_names = match load_package_names(&repo) {
        Ok(names) => names,
        Err(msg) => {
            display.print_error(msg);
            return 1;
        }
    };

    let choice = prompt_track_choice(&package_names, file);

    match choice {
        TrackChoice::ExistingPackage(ref name) => {
            common::handle_track_for_package(name, file, config, display).await
        }
        TrackChoice::NewStandalone(ref name) => {
            // Validate namespace before creating
            let dotfiles_dir = config.selfie_config().dotfiles_directory();
            let dotfiles_repo = if dotfiles_dir.is_dir() {
                Some(YamlPackageRepository::new(RealFileSystem, dotfiles_dir))
            } else {
                None
            };
            if let Err(e) = namespace::validate_unique_name(name, &repo, dotfiles_repo.as_ref()) {
                display.print_error(format!("Cannot use name '{name}': {e}"));
                return 1;
            }
            common::handle_track_standalone(name, file, config, display).await
        }
        TrackChoice::Cancelled => {
            display.print_info("Cancelled.");
            0
        }
    }
}

/// Present the interactive selection prompt and return the user's choice.
fn prompt_track_choice(package_names: &[String], file: &str) -> TrackChoice {
    // Build the selection list: existing packages + sentinel options
    let mut items: Vec<String> = package_names.to_vec();
    items.push(NEW_STANDALONE.to_string());
    items.push(TYPE_A_NAME.to_string());

    let selection = FuzzySelect::with_theme(&ColorfulTheme::default())
        .with_prompt("Where should this file be tracked?")
        .items(&items)
        .default(0)
        .interact_opt();

    let choice = match selection {
        Ok(Some(idx)) => idx,
        Ok(None) | Err(_) => return TrackChoice::Cancelled,
    };

    resolve_choice(&items, choice, file)
}

/// Pure function: given the selection list and the chosen index, determine action.
fn resolve_choice(items: &[String], choice: usize, file: &str) -> TrackChoice {
    let selected = &items[choice];

    if selected == TYPE_A_NAME {
        match prompt_for_name() {
            Some(name) => TrackChoice::NewStandalone(name),
            None => TrackChoice::Cancelled,
        }
    } else if selected == NEW_STANDALONE {
        let suggested = suggest_name(file);
        match prompt_for_name_with_default(&suggested) {
            Some(name) => TrackChoice::NewStandalone(name),
            None => TrackChoice::Cancelled,
        }
    } else {
        TrackChoice::ExistingPackage(selected.clone())
    }
}

/// Prompt the user to type a dotfile name (no default).
fn prompt_for_name() -> Option<String> {
    Input::with_theme(&ColorfulTheme::default())
        .with_prompt("Name for the new dotfile spec")
        .interact_text()
        .ok()
        .filter(|s: &String| !s.trim().is_empty())
}

/// Prompt for a name, pre-filling with a suggested default.
fn prompt_for_name_with_default(default: &str) -> Option<String> {
    Input::with_theme(&ColorfulTheme::default())
        .with_prompt("Name for the new dotfile spec")
        .default(default.to_string())
        .interact_text()
        .ok()
        .filter(|s: &String| !s.trim().is_empty())
}

/// Derive a suggested spec name from the file path.
///
/// Takes the final component, strips a leading `.` (so `.dprint.jsonc` becomes
/// `dprint.jsonc`), and drops the extension to give a short default name.
fn suggest_name(file_path: &str) -> String {
    let path = std::path::Path::new(file_path);
    let stem = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();

    // Strip leading dot
    let stem = stem.strip_prefix('.').unwrap_or(&stem);

    // Drop extension to get a terse name
    match stem.rsplit_once('.') {
        Some((base, _ext)) if !base.is_empty() => base.to_string(),
        _ => stem.to_string(),
    }
}

/// Check if a file is already tracked by any package or standalone dotfile.
///
/// Scans both the packages directory and the dotfiles directory for a dotfile
/// entry whose target matches the given file path. Returns the name of the
/// package that tracks it, or `None`.
fn find_existing_tracker(file: &str, config: &CliConfig) -> Option<String> {
    let fs = RealFileSystem;
    let expanded = selfie::dotfile_service::service::expand_target_path(&fs, file);

    let repos: Vec<YamlPackageRepository<RealFileSystem>> = [
        Some(config.selfie_config().package_directory().to_path_buf()),
        {
            let d = config.selfie_config().dotfiles_directory().to_path_buf();
            d.is_dir().then_some(d)
        },
    ]
    .into_iter()
    .flatten()
    .map(|dir| YamlPackageRepository::new(RealFileSystem, dir))
    .collect();

    for repo in &repos {
        if let Ok(output) = repo.list_packages() {
            for pkg in output.valid_packages() {
                for (_scope, entry) in pkg.dotfiles_with_scope() {
                    let entry_expanded =
                        selfie::dotfile_service::service::expand_target_path(&fs, entry.target());
                    if entry_expanded == expanded {
                        return Some(pkg.name().to_string());
                    }
                }
            }
        }
    }
    None
}

/// Load sorted package names from the repository.
fn load_package_names(repo: &impl PackageRepository) -> Result<Vec<String>, String> {
    let mut names = repo
        .available_packages()
        .map_err(|e| format!("Failed to list packages: {e}"))?;
    names.sort();
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggest_name_strips_leading_dot_and_extension() {
        assert_eq!(suggest_name("~/.dprint.jsonc"), "dprint");
        assert_eq!(suggest_name("/home/user/.config/starship.toml"), "starship");
    }

    #[test]
    fn suggest_name_handles_no_extension() {
        assert_eq!(suggest_name("/home/user/.bashrc"), "bashrc");
    }

    #[test]
    fn suggest_name_handles_no_leading_dot() {
        assert_eq!(suggest_name("/etc/alacritty.toml"), "alacritty");
    }

    #[test]
    fn suggest_name_handles_deeply_nested_path() {
        assert_eq!(suggest_name("~/.config/fish/conf.d/fnm.fish"), "fnm");
    }

    #[test]
    fn load_package_names_returns_sorted() {
        use selfie::package::port::MockPackageRepository;

        let mut repo = MockPackageRepository::new();
        repo.expect_available_packages().returning(|| {
            Ok(vec![
                "zsh".to_string(),
                "alacritty".to_string(),
                "fnm".to_string(),
            ])
        });

        let names = load_package_names(&repo).unwrap();
        assert_eq!(names, vec!["alacritty", "fnm", "zsh"]);
    }

    #[test]
    fn resolve_choice_selects_existing_package() {
        let items = vec![
            "alacritty".to_string(),
            "fnm".to_string(),
            NEW_STANDALONE.to_string(),
            TYPE_A_NAME.to_string(),
        ];
        let result = resolve_choice(&items, 0, "~/.config/test.toml");
        assert_eq!(
            result,
            TrackChoice::ExistingPackage("alacritty".to_string())
        );
    }
}
