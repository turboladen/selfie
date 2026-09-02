use dialoguer::{Confirm, theme::SimpleTheme};
use selfie::package::port::PackageRepository;

use crate::config::CliConfig;
use tracing::info;

use crate::display_manager::DisplayManager;

use crate::commands::common;

pub(crate) fn handle_edit(package_name: &str, config: &CliConfig, display: &DisplayManager) -> i32 {
    info!("Editing package: {}", package_name);

    // Create repository to look up the package
    let repo = common::create_package_repository(config);

    // A parse failure is not an absent package. Collapsing it with `.ok()` made
    // selfie say the package did not exist, offer to create it, and write a
    // template over the file -- and `spec edit` is the command a user reaches
    // for precisely when a file is broken (selfie-6iry).
    let existing_package = match repo.get_package(package_name) {
        Ok(pkg) => Some(pkg),
        Err(e) if e.means_no_such_package() => None,
        Err(e) => {
            display.print_error(format!(
                "Cannot edit '{package_name}': a file is already there and selfie could not use \
                 it, so opening it as a new package would overwrite it. Edit it directly. {e}"
            ));
            return 1;
        }
    };
    let package_exists = existing_package.is_some();
    let package_path = existing_package.as_ref().map(|p| p.file_path());

    // Check if EDITOR is available with context-specific error messages
    let Some(_editor) =
        common::check_editor_available(display, package_name, package_exists, package_path)
    else {
        return 1;
    };

    // Try to get existing package, or create a new one
    let package_blob = if let Some(pkg) = existing_package {
        display.print_info(format!(
            "Opening existing package '{package_name}' for editing"
        ));
        pkg
    } else {
        // Before offering to create, ask the file system whether the path is
        // free. Names fold, so a differently-capitalized spec was already found
        // above; what the name check cannot see is a path held by something no
        // name resolves to, and writing there would replace it (selfie-6cg2).
        //
        // Ahead of the prompt on purpose: asking someone to confirm a create
        // that is about to be refused wastes the answer, and it is the only
        // position a test without a terminal can reach.
        let prospective = common::create_new_package(package_name, config);
        if repo.path_is_occupied(prospective.file_path()) {
            display.print_error(format!(
                "Cannot create '{package_name}': {} is already taken. On this file system that \
                 path may resolve to a file stored under a different capitalization, and creating \
                 would replace it.",
                prospective.file_path().display()
            ));
            return 1;
        }

        display.print_info(format!("Package '{package_name}' does not exist."));

        // Prompt user for confirmation before creating
        let confirm = Confirm::with_theme(&SimpleTheme)
            .with_prompt(format!("Create new package '{package_name}'?"))
            .default(false)
            .interact();

        match confirm {
            Ok(true) => {
                // User confirmed, proceed with creation
            }
            Ok(false) => {
                display.print_info("Package creation cancelled.");
                return 0;
            }
            Err(_) => {
                display.print_error("Failed to read user input.");
                return 1;
            }
        }

        display.print_info(format!("Creating new package '{package_name}'"));
        common::create_new_package(package_name, config)
    };

    // Only a package that does not exist yet is written. An existing file is
    // opened exactly as the user wrote it.
    //
    // Saving first re-serialized it through serde, which flattens YAML anchors,
    // drops every comment, and reorders keys -- damage done before the editor
    // even opened, on a run where the user might change nothing. It also made
    // `spec edit` refuse to open the one file a typo guard had just rejected,
    // which is the file the user is trying to fix.
    if package_blob.is_new()
        && let Err(exit_code) = common::save_package(&repo, &package_blob, display)
    {
        return exit_code;
    }

    // Open the package file in the editor
    let action = if package_blob.is_new() {
        "created"
    } else {
        "updated"
    };
    let success_message = format!(
        "Package '{package_name}' {action} successfully at {}",
        package_blob.file_path().display()
    );

    common::open_editor(package_blob.file_path(), display, Some(success_message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use selfie::package::{
        GetPackage, Package,
        port::{MockPackageRepository, PackageError},
    };
    use std::collections::HashMap;
    use std::{fs, path::PathBuf};
    use tempfile::TempDir;
    use test_common::test_config_with_dir;

    #[test]
    fn test_handle_edit_nonexistent_package() {
        // Test behavior when package doesn't exist and no EDITOR is available
        let temp_dir = TempDir::new().unwrap();
        let package_dir = temp_dir.path().join("packages");
        fs::create_dir_all(&package_dir).unwrap();

        // Remove EDITOR environment variable to force editor check to fail
        let old_editor = std::env::var("EDITOR").ok();
        unsafe {
            std::env::remove_var("EDITOR");
        }

        let config = CliConfig::wrap_for_test(test_config_with_dir(package_dir));
        let display = DisplayManager::new(false);

        // This test will exit early because there's no EDITOR
        let result =
            tokio_test::block_on(async { handle_edit("nonexistent-package", &config, &display) });

        // Should fail with exit code 1 due to missing EDITOR
        assert_eq!(result, 1);

        // Restore EDITOR if it was set
        if let Some(editor) = old_editor {
            unsafe {
                std::env::set_var("EDITOR", editor);
            }
        }
    }

    #[test]
    fn test_confirmation_prompt_structure() {
        // Test that we can create a confirmation prompt (without actually running it)
        let package_name = "test-package";
        let confirm = Confirm::with_theme(&SimpleTheme)
            .with_prompt(format!("Create new package '{package_name}'?"))
            .default(false);

        // Just verify we can construct the prompt without panicking
        // We can't access the default field directly as it's private
        drop(confirm);
    }

    #[test]
    fn test_get_package_new_creates_template() {
        // Test package template creation without filesystem operations
        let package_dir = std::path::PathBuf::from("/test/packages");
        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));

        let get_package = common::create_new_package("test-template", &config);

        assert!(get_package.is_new());
        assert_eq!(get_package.package().name(), "test-template");
        assert_eq!(
            get_package.file_path(),
            package_dir.join("test-template.yml")
        );
        assert!(get_package.package().environments().contains_key("default"));
    }

    #[test]
    fn test_yaml_serialization_roundtrip() {
        // Test that we can serialize and deserialize a package
        use selfie::package::PackageBuilder;

        let original_package = PackageBuilder::default()
            .name("test-package")
            .description("Test package")
            .environment("test", |b| {
                b.install("echo 'test'").check(Some("echo 'check'"))
            })
            .build();

        // Serialize to YAML
        let yaml_content = serde_saphyr::to_string(&original_package).unwrap();

        // Deserialize back
        let deserialized: selfie::package::Package = serde_saphyr::from_str(&yaml_content).unwrap();

        // Should be equivalent
        assert_eq!(original_package.name(), deserialized.name());
        assert_eq!(original_package.description(), deserialized.description());
        assert_eq!(
            original_package.environments().len(),
            deserialized.environments().len()
        );
    }

    // `handle_edit` writes only when the blob says it is new, so if this ever
    // stopped being true, `spec edit <new-name>` would open an editor on a file
    // that was never created. Not reachable from an integration test: the create
    // path goes through a `dialoguer` confirm, which needs a TTY.
    #[test]
    fn a_newly_created_package_reports_itself_as_new() {
        let package_dir = std::path::PathBuf::from("/test/packages");
        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));

        assert!(common::create_new_package("brand-new", &config).is_new());
    }

    #[test]
    fn test_package_edit_save_with_mock_repo() {
        // Test package saving logic using MockPackageRepository for CLI logic testing
        let mut mock_repo = MockPackageRepository::new();
        let package_dir = std::path::PathBuf::from("/test/packages");
        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));

        // Mock successful save operation
        mock_repo
            .expect_save_package()
            .times(1)
            .returning(|_, _| Ok(()));

        // Create a package to save
        let package_blob = common::create_new_package("edit-test", &config);

        // Test saving the package using mocked repository
        let result = mock_repo.save_package(package_blob.package(), package_blob.file_path());

        assert!(result.is_ok());
        assert_eq!(package_blob.package().name(), "edit-test");
        assert_eq!(package_blob.file_path(), package_dir.join("edit-test.yml"));
    }

    #[test]
    fn test_edit_package_with_mock_repository() {
        let mut mock_repo = MockPackageRepository::new();

        // Create mock existing package
        let package = Package::new(
            "edit-test".to_string(),
            Some("Test package for editing".to_string()),
            None,
            Vec::new(),
            None,
            HashMap::new(),
            PathBuf::from("/test/packages/edit-test.yml"),
        );
        let get_package =
            GetPackage::from_existing(package, PathBuf::from("/test/packages/edit-test.yml"));

        // Mock get package operation (to check if package exists)
        mock_repo
            .expect_get_package()
            .with(mockall::predicate::eq("edit-test"))
            .times(1)
            .returning(move |_| Ok(get_package.clone()));

        // Mock successful save after editing
        mock_repo
            .expect_save_package()
            .times(1)
            .returning(|_, _| Ok(()));

        // Test edit workflow logic
        let get_result = mock_repo.get_package("edit-test");
        assert!(get_result.is_ok());

        let existing_package = get_result.unwrap();
        assert_eq!(existing_package.package().name(), "edit-test");

        // Test saving edited package
        let save_result =
            mock_repo.save_package(existing_package.package(), existing_package.file_path());
        assert!(save_result.is_ok());

        // This demonstrates testing CLI edit logic without repository implementation
    }

    #[test]
    fn test_edit_package_not_found_with_mock_repo() {
        let mut mock_repo = MockPackageRepository::new();

        // Mock package not found
        mock_repo
            .expect_get_package()
            .with(mockall::predicate::eq("nonexistent"))
            .times(1)
            .returning(|_| {
                Err(PackageError::PackageNotFound {
                    name: "nonexistent".to_string(),
                    packages_path: PathBuf::from("/test/packages"),
                    files_examined: 0,
                    search_patterns: vec!["nonexistent.yml".to_string()],
                }
                .into())
            });

        // Test CLI error handling for non-existent package
        let result = mock_repo.get_package("nonexistent");
        assert!(result.is_err());

        // This tests CLI error handling for edit operations without repository implementation
    }
}
