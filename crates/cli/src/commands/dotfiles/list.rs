//! List command handler for dotfiles
//!
//! This module handles the `selfie dotfiles list` CLI command, which shows
//! all dotfile mappings defined across packages and the standalone dotfiles
//! directory. This is a fast, file-only operation — no commands are executed.

use selfie::{
    fs::real::RealFileSystem,
    package::{Package, port::PackageRepository, repository::yaml::YamlPackageRepository},
};
use tracing::info;

use crate::{
    commands::common::{create_formatted_table, create_package_repository},
    config::CliConfig,
    display_manager::{DisplayManager, shorten_path},
};

/// Handle the `selfie dotfiles list` command
///
/// Reads all package YAML files from both the packages directory and the
/// standalone dotfiles directory, then displays every dotfile mapping in
/// a table. This bypasses the service layer for fast file reads (same
/// pattern as `spec list` and the MCP bulk tools).
pub(crate) fn handle_list(config: &CliConfig, display: &DisplayManager) -> i32 {
    info!("Listing dotfiles");

    let packages = match collect_packages_with_dotfiles(config, display) {
        Ok(pkgs) => pkgs,
        Err(code) => return code,
    };

    if packages.is_empty() {
        display.print_info("No dotfiles found in any packages.");
        return 0;
    }

    let mut table = create_formatted_table();
    table.set_header(vec!["Package", "Source", "Target"]);

    let mut total = 0;
    for pkg in &packages {
        let base_dir = pkg
            .path()
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));

        for entry in pkg.dotfiles() {
            let source_display = base_dir.join(entry.source());
            table.add_row(vec![
                pkg.name().to_string(),
                shorten_path(&source_display.display().to_string()),
                shorten_path(entry.target()),
            ]);
            total += 1;
        }
    }

    display.println(table.to_string());
    display.print_info(format!(
        "{total} dotfile{} across {} package{}",
        if total == 1 { "" } else { "s" },
        packages.len(),
        if packages.len() == 1 { "" } else { "s" },
    ));

    0
}

/// Load packages from both repos, keeping only those with dotfile entries.
fn collect_packages_with_dotfiles(
    config: &CliConfig,
    display: &DisplayManager,
) -> Result<Vec<Package>, i32> {
    let repo = create_package_repository(config);
    let mut packages = load_dotfile_packages(&repo, display, "packages")?;

    // Add standalone dotfiles repository if the directory exists
    let dotfiles_dir = config.selfie_config().dotfiles_directory();
    if dotfiles_dir.is_dir() {
        let dotfiles_repo = YamlPackageRepository::new(RealFileSystem, dotfiles_dir);
        match load_dotfile_packages(&dotfiles_repo, display, "dotfiles") {
            Ok(dotfile_pkgs) => packages.extend(dotfile_pkgs),
            Err(_) => {
                // Non-fatal — standalone dotfiles dir is optional
            }
        }
    }

    Ok(packages)
}

/// Load packages from a single repository, filtering to those with dotfiles.
fn load_dotfile_packages(
    repo: &impl PackageRepository,
    display: &DisplayManager,
    label: &str,
) -> Result<Vec<Package>, i32> {
    match repo.list_packages() {
        Ok(output) => Ok(output
            .valid_packages()
            .filter(|p| !p.dotfiles().is_empty())
            .cloned()
            .collect()),
        Err(e) => {
            display.print_error(format!("Failed to load {label}: {e}"));
            Err(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use selfie::package::{
        DotfileEntry, PackageBuilder,
        port::{ListPackagesOutput, MockPackageRepository, PackageListError},
    };

    fn make_package_with_dotfiles(name: &str) -> Package {
        PackageBuilder::default()
            .name(name)
            .dotfiles(vec![DotfileEntry::new(
                format!("{name}.conf"),
                format!("~/.config/{name}.conf"),
            )])
            .build()
    }

    fn make_package_without_dotfiles(name: &str) -> Package {
        PackageBuilder::default().name(name).build()
    }

    #[test]
    fn test_load_filters_to_packages_with_dotfiles() {
        let mut repo = MockPackageRepository::new();
        repo.expect_list_packages().returning(|| {
            Ok(ListPackagesOutput::from_packages(vec![
                make_package_with_dotfiles("starship"),
                make_package_without_dotfiles("ripgrep"),
                make_package_with_dotfiles("alacritty"),
            ]))
        });

        let display = DisplayManager::new(false);
        let result = load_dotfile_packages(&repo, &display, "test").unwrap();

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].name(), "starship");
        assert_eq!(result[1].name(), "alacritty");
    }

    #[test]
    fn test_load_returns_empty_when_no_dotfiles() {
        let mut repo = MockPackageRepository::new();
        repo.expect_list_packages().returning(|| {
            Ok(ListPackagesOutput::from_packages(vec![
                make_package_without_dotfiles("ripgrep"),
                make_package_without_dotfiles("fd"),
            ]))
        });

        let display = DisplayManager::new(false);
        let result = load_dotfile_packages(&repo, &display, "test").unwrap();

        assert!(result.is_empty());
    }

    #[test]
    fn test_load_returns_error_on_repo_failure() {
        let mut repo = MockPackageRepository::new();
        repo.expect_list_packages().returning(|| {
            Err(PackageListError::PackageDirectoryNotFound(
                "/missing".into(),
            ))
        });

        let display = DisplayManager::new(false);
        let result = load_dotfile_packages(&repo, &display, "packages");

        assert_eq!(result.unwrap_err(), 1);
    }
}
