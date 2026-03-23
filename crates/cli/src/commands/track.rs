//! Interactive track command handler
//!
//! This module handles the `selfie track <file>` CLI command — an interactive
//! shortcut that prompts the user to choose whether the file belongs to an
//! existing package or should become a new standalone dotfile, then delegates
//! to the appropriate tracking handler.

use dialoguer::{Input, Select, theme::ColorfulTheme};
use selfie::{
    dotfile_service::{port::DotfileService, service::DotfileServiceImpl},
    fs::real::RealFileSystem,
    package::{port::PackageRepository, repository::yaml::YamlPackageRepository},
};
use tracing::info;

use crate::{
    commands::common::create_package_repository, config::CliConfig,
    display_manager::DisplayManager, event_processor::EventProcessor,
};

/// Sentinel item appended after real package names in the select list.
const NEW_STANDALONE: &str = "→ New standalone dotfile";
/// Sentinel item for free-text name entry.
const TYPE_A_NAME: &str = "→ Let me type a name";

/// Handle the `selfie track` interactive command
pub(crate) async fn handle_track(file: &str, config: &CliConfig, display: &DisplayManager) -> i32 {
    info!("Interactive track for '{}'", file);

    let repo = create_package_repository(config);

    // Collect available package names for the prompt
    let package_names = match load_package_names(&repo) {
        Ok(names) => names,
        Err(msg) => {
            display.print_error(msg);
            return 1;
        }
    };

    // Build the selection list: existing packages + sentinel options
    let mut items: Vec<String> = package_names.clone();
    items.push(NEW_STANDALONE.to_string());
    items.push(TYPE_A_NAME.to_string());

    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Where should this file be tracked?")
        .items(&items)
        .default(0)
        .interact_opt();

    let choice = match selection {
        Ok(Some(idx)) => idx,
        Ok(None) | Err(_) => {
            display.print_info("Cancelled.");
            return 0;
        }
    };

    let selected = &items[choice];

    if selected == TYPE_A_NAME {
        // Free-text: prompt for a standalone dotfile name
        let name = match prompt_for_name() {
            Some(n) => n,
            None => {
                display.print_info("Cancelled.");
                return 0;
            }
        };
        track_standalone(&name, file, config, display).await
    } else if selected == NEW_STANDALONE {
        // Derive a default name from the filename (strip leading dot)
        let suggested = suggest_name(file);
        let name = match prompt_for_name_with_default(&suggested) {
            Some(n) => n,
            None => {
                display.print_info("Cancelled.");
                return 0;
            }
        };
        track_standalone(&name, file, config, display).await
    } else {
        // An existing package was chosen
        track_for_package(selected, file, config, display).await
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

/// Load sorted package names from the repository.
fn load_package_names(repo: &impl PackageRepository) -> Result<Vec<String>, String> {
    let mut names = repo
        .available_packages()
        .map_err(|e| format!("Failed to list packages: {e}"))?;
    names.sort();
    Ok(names)
}

/// Track as a standalone dotfile via `DotfileServiceImpl::track_standalone`.
async fn track_standalone(
    name: &str,
    file: &str,
    config: &CliConfig,
    display: &DisplayManager,
) -> i32 {
    let repo = create_package_repository(config);
    let fs = RealFileSystem;
    let mut service = DotfileServiceImpl::new(repo, fs, config.selfie_config().clone());

    let dotfiles_dir = config.selfie_config().dotfiles_directory();
    if !dotfiles_dir.is_dir() {
        display.print_error(format!(
            "Dotfiles directory does not exist: {}",
            dotfiles_dir.display()
        ));
        display.print_suggestion(format!(
            "Create it with: mkdir -p {}",
            dotfiles_dir.display()
        ));
        return 1;
    }

    let dotfiles_repo = YamlPackageRepository::new(RealFileSystem, dotfiles_dir);
    service = service.with_dotfiles_repository(dotfiles_repo);

    let event_stream = service.track_standalone(name, file).await;

    let processor = EventProcessor::new(display.clone());
    let result = processor.process_events(event_stream, |_| false).await;
    result.exit_code
}

/// Track for an existing package via `DotfileServiceImpl::track_for_package`.
async fn track_for_package(
    package_name: &str,
    file: &str,
    config: &CliConfig,
    display: &DisplayManager,
) -> i32 {
    let repo = create_package_repository(config);
    let fs = RealFileSystem;
    let service = DotfileServiceImpl::new(repo, fs, config.selfie_config().clone());

    let event_stream = service.track_for_package(package_name, file).await;

    let processor = EventProcessor::new(display.clone());
    let result = processor.process_events(event_stream, |_| false).await;
    result.exit_code
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
}
