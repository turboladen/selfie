//! List command handler for dotfiles
//!
//! This module handles the `selfie dotfiles list` CLI command, which shows
//! all dotfile mappings defined across packages and the standalone dotfiles
//! directory. This is a fast, file-only operation — no commands are executed.

use selfie::package::{Package, port::PackageRepository};
use tracing::info;

use crate::{
    commands::common::{create_formatted_table, create_package_repository, dotfiles_repository},
    config::CliConfig,
    display_manager::{DisplayManager, shorten_path},
};

/// Which directory a package was loaded from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DotfileOrigin {
    Packages,
    Dotfiles,
}

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

    // Print the base directories so relative source paths have context
    print_base_directories(config, display, &packages);

    let mut table = create_formatted_table();
    table.set_header(vec!["Package", "Environment", "Source", "Target"]);

    let mut total = 0;
    for (pkg, _origin) in &packages {
        for (scope, entry) in pkg.dotfiles_with_scope() {
            table.add_row(vec![
                pkg.name().to_string(),
                scope.unwrap_or("(shared)").to_string(),
                // Renders var names and command strings, never a resolved value:
                // listing runs nothing, so it cannot leak a secret or raise an
                // authentication prompt.
                //
                // A refused entry is shown as the reason it was refused rather
                // than omitted: it is in the package file, `selfie apply` will
                // report skipping it, and a listing that hid it would leave the
                // user looking for a dotfile the table says does not exist.
                entry
                    .content_source()
                    .map_or_else(|invalid| invalid.to_string(), |source| source.to_string()),
                shorten_path(entry.target()),
            ]);
            total += 1;
        }
    }

    display.println(table.to_string());
    display.print_info(format!(
        "{total} {} across {} {}",
        selfie::pluralize(total, "dotfile", "dotfiles"),
        packages.len(),
        selfie::pluralize(packages.len(), "package", "packages"),
    ));

    0
}

/// Load packages from both repos, keeping only those with dotfile entries.
/// Each package is tagged with its origin so we know which base directory to display.
fn collect_packages_with_dotfiles(
    config: &CliConfig,
    display: &DisplayManager,
) -> Result<Vec<(Package, DotfileOrigin)>, i32> {
    let repo = create_package_repository(config);
    let (raw, skipped) = load_dotfile_packages(&repo, display, "packages")?;
    for warning in skipped {
        display.print_warning(warning);
    }
    let mut packages: Vec<(Package, DotfileOrigin)> = raw
        .into_iter()
        .map(|p| (p, DotfileOrigin::Packages))
        .collect();

    // Add standalone dotfiles repository if the directory exists
    if let Some(dotfiles_repo) = dotfiles_repository(config, display) {
        match load_dotfile_packages(&dotfiles_repo, display, "dotfiles") {
            Ok((dotfile_pkgs, dotfile_skipped)) => {
                for warning in dotfile_skipped {
                    display.print_warning(warning);
                }
                packages.extend(
                    dotfile_pkgs
                        .into_iter()
                        .map(|p| (p, DotfileOrigin::Dotfiles)),
                );
            }
            Err(_) => {
                // Non-fatal — standalone dotfiles dir is optional
            }
        }
    }

    Ok(packages)
}

/// Print the base directories above the table so relative source paths have context.
fn print_base_directories(
    config: &CliConfig,
    display: &DisplayManager,
    packages: &[(Package, DotfileOrigin)],
) {
    let has_packages = packages.iter().any(|(_, o)| *o == DotfileOrigin::Packages);
    let has_dotfiles = packages.iter().any(|(_, o)| *o == DotfileOrigin::Dotfiles);

    if has_packages {
        display.print_info(format!(
            "Packages: {}",
            shorten_path(&config.package_directory().display().to_string()),
        ));
    }
    if has_dotfiles {
        display.print_info(format!(
            "Dotfiles: {}",
            shorten_path(
                &config
                    .selfie_config()
                    .dotfiles_directory()
                    .display()
                    .to_string()
            ),
        ));
    }
}

/// Load packages from a single repository, filtering to those with dotfiles,
/// along with a warning for every spec file that could not be loaded.
// The warnings are returned rather than printed so they can be asserted:
// `print_warning` goes to stderr, which a unit test cannot observe, and the
// dropped-silently case is the whole reason this reports anything at all.
fn load_dotfile_packages(
    repo: &impl PackageRepository,
    display: &DisplayManager,
    label: &str,
) -> Result<(Vec<Package>, Vec<String>), i32> {
    match repo.list_packages() {
        Ok(output) => {
            let skipped = output
                .invalid_packages()
                .map(selfie::package::service::skipped_spec_warning)
                .collect();

            let packages = output
                .valid_packages()
                .filter(|p| !p.dotfiles_with_scope().is_empty())
                .cloned()
                .collect();

            Ok((packages, skipped))
        }
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
        let (packages, skipped) = load_dotfile_packages(&repo, &display, "test").unwrap();

        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name(), "starship");
        assert_eq!(packages[1].name(), "alacritty");
        assert!(skipped.is_empty(), "got: {skipped:?}");
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
        let (packages, skipped) = load_dotfile_packages(&repo, &display, "test").unwrap();

        assert!(packages.is_empty());
        assert!(skipped.is_empty(), "got: {skipped:?}");
    }

    // `valid_packages` drops a spec file that could not be loaded, so before the
    // shared warning this command listed the dotfiles it could read and said
    // nothing about the file it could not -- indistinguishable from a package
    // that genuinely declares no dotfiles.
    #[test]
    fn test_load_names_a_package_file_it_could_not_read() {
        use selfie::package::port::PackageParseError;

        let mut repo = MockPackageRepository::new();
        repo.expect_list_packages().returning(|| {
            Ok(ListPackagesOutput::from_results(vec![
                Ok(make_package_with_dotfiles("starship")),
                Err(PackageParseError::IrregularFile {
                    package_path: "/test/packages/ghost.yml".into(),
                    kind: "named pipe (fifo)",
                }),
            ]))
        });

        let display = DisplayManager::new(false);
        let (packages, skipped) = load_dotfile_packages(&repo, &display, "packages").unwrap();

        // The readable package still comes back: reporting the skipped file must
        // not cost the caller the rest of the listing.
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name(), "starship");

        assert_eq!(skipped.len(), 1, "the unreadable file must be reported");
        assert!(skipped[0].contains("ghost.yml"), "got: {}", skipped[0]);
        assert!(
            skipped[0].contains("named pipe (fifo)"),
            "got: {}",
            skipped[0]
        );
    }

    // The control for the test above: an ordinary listing reports nothing
    // skipped, so a `skipped` that is never empty would fail here.
    #[test]
    fn test_load_reports_nothing_skipped_for_a_clean_listing() {
        let mut repo = MockPackageRepository::new();
        repo.expect_list_packages().returning(|| {
            Ok(ListPackagesOutput::from_packages(vec![
                make_package_with_dotfiles("starship"),
            ]))
        });

        let display = DisplayManager::new(false);
        let (packages, skipped) = load_dotfile_packages(&repo, &display, "packages").unwrap();

        assert_eq!(packages.len(), 1);
        assert!(skipped.is_empty(), "got: {skipped:?}");
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
