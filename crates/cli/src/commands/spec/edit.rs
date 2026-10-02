use dialoguer::{Confirm, theme::SimpleTheme};
use selfie::{
    namespace,
    package::{SpecService, port::PackageRepository},
};

use crate::config::CliConfig;
use tracing::info;

use crate::display_manager::DisplayManager;
use crate::event_processor::Exit;
use crate::formatters::name_argument;

use crate::commands::common;
use crate::commands::spec::create::{create_basic_package, name_check_refusal, write_spec};

pub(crate) async fn handle_edit(
    service: &impl SpecService,
    package_name: &str,
    config: &CliConfig,
    display: &DisplayManager,
) -> i32 {
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
        // Not about a file at this name, so "a file is already there" would be
        // false. The error names the directory and what is at it.
        Err(e @ selfie::package::port::PackageRepoError::PackageListError(_)) => {
            display.print_error(format!("Cannot edit '{package_name}': {e}"));
            return 1;
        }
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

    // An existing file is opened exactly as the user wrote it, never saved
    // first: a serde round trip flattens YAML anchors, drops every comment and
    // reorders keys, and would refuse the very file a typo guard just rejected.
    let (path, created) = if let Some(pkg) = existing_package {
        display.print_run_note(format!(
            "Opening existing package '{package_name}' for editing"
        ));
        (pkg.file_path().to_path_buf(), false)
    } else {
        match create_for_edit(service, &repo, package_name, config, display).await {
            Ok(path) => (path, true),
            Err(code) => return code,
        }
    };

    common::open_editor(
        &path,
        display,
        editor_success_message(package_name, &path, created),
    )
}

/// The line to print once the editor closes: one for an existing spec, and
/// none for a created one, whose create already printed its line.
fn editor_success_message(
    package_name: &str,
    path: &std::path::Path,
    created: bool,
) -> Option<String> {
    (!created).then(|| {
        format!(
            "Package '{package_name}' updated successfully at {}",
            path.display()
        )
    })
}

/// The directory standalone dotfile specs are read from.
fn dotfiles_repo_directory(config: &CliConfig) -> std::path::PathBuf {
    config.selfie_config().dotfiles_directory()
}

/// Create `package_name` for `spec edit`, once the user confirms, the way
/// `spec create` creates one, and return where it was written.
///
/// # Errors
///
/// The exit code when the spec was not created: its path is taken, its name
/// is, the user did not confirm, or the create failed.
async fn create_for_edit(
    service: &impl SpecService,
    repo: &impl PackageRepository,
    package_name: &str,
    config: &CliConfig,
    display: &DisplayManager,
) -> Result<std::path::PathBuf, i32> {
    let package = create_basic_package(package_name, config);

    // Before offering to create, ask the file system whether the path is
    // free. Names fold case and extension, so those were found above; what
    // the name check cannot see is a path the file system matches and selfie's
    // names do not, and selfie must not write over it (selfie-6cg2).
    //
    // Every check comes ahead of the prompt: asking someone to confirm a create
    // that is about to be refused wastes the answer.
    if let Some(refusal) = selfie::package::spec_name_refusal(package_name) {
        display.print_error(refusal);
        return Err(Exit::Failed.code());
    }
    if repo.path_is_occupied(package.path()) {
        display.print_error(occupied_path_refusal(package_name, package.path()));
        return Err(Exit::Failed.code());
    }

    // The dotfiles directory as well: a standalone spec may hold the name.
    let dotfiles_repo = common::create_dotfiles_repository(config);
    if let Err(error) = namespace::validate_unique_name(package_name, repo, Some(&dotfiles_repo)) {
        // The user asked to edit that spec, so a different name is not the way
        // out: say which kind it is and where to edit it.
        display.print_error(match &error {
            namespace::NamespaceValidationError::Conflict(namespace::NamespaceConflict {
                found_in: namespace::NameLocation::Dotfiles,
                ..
            }) => format!(
                "'{package_name}' is a standalone dotfile spec, and `spec edit` opens package \
                 specs only. Edit its file in {} directly.",
                dotfiles_repo_directory(config).display()
            ),
            _ => name_check_refusal(package_name, &error),
        });
        return Err(Exit::Failed.code());
    }

    display.print_run_note(format!("Package '{package_name}' does not exist."));
    confirm_new_package(package_name, display)?;
    display.print_run_note(format!("Creating new package '{package_name}'"));

    write_spec(service, package, display)
        .await
        .map(|(_, path)| path)
}

/// Why `spec edit` will not create `package_name` at `path`: something is already
/// there that the name lookup did not find.
fn occupied_path_refusal(package_name: &str, path: &std::path::Path) -> String {
    format!(
        "Cannot create '{package_name}': {} is already taken by something selfie did not find \
         under that name; selfie will not write over it.",
        path.display()
    )
}

/// Ask whether to create `package_name`.
///
/// # Errors
///
/// The exit code when the user did not confirm.
fn confirm_new_package(package_name: &str, display: &DisplayManager) -> Result<(), i32> {
    let confirm = display.prompt(
        Confirm::with_theme(&SimpleTheme)
            .with_prompt(format!("Create new package '{package_name}'?"))
            .default(false),
    );

    match confirm {
        Ok(true) => Ok(()),
        // A create that wrote nothing did not do what it was asked.
        Ok(false) => {
            display.print_run_note("Package creation cancelled.");
            Err(Exit::Failed.code())
        }
        Err(failure) => Err(display
            .refuse_prompt(
                &failure,
                "Confirming a new package",
                format!(
                    "Create it with `selfie spec create {}`, then edit it.",
                    name_argument(package_name)
                )
                .as_str(),
            )
            .code()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use selfie::package::{
        Environments, GetPackage, Package,
        port::{MockPackageRepository, PackageError},
    };
    use std::{fs, path::PathBuf};
    use tempfile::TempDir;
    use test_common::test_config_with_dir;

    // The refusal names what the guard catches. Capitalization never reaches it,
    // because the name lookup folds case and would have found the file. Checked
    // here as well as end to end, because the end-to-end test can only run on a
    // file system that matches names across Unicode normalizations.
    #[test]
    fn the_occupied_path_refusal_does_not_blame_capitalization() {
        let refusal = occupied_path_refusal("naive", std::path::Path::new("/packages/naive.yml"));

        assert_eq!(
            refusal,
            "Cannot create 'naive': /packages/naive.yml is already taken by something selfie did \
             not find under that name; selfie will not write over it."
        );
        // What holds the path may be a directory, which nothing could replace.
        for wrong in ["capitalization", "replace", "file"] {
            assert!(!refusal.contains(wrong), "{wrong}: {refusal}");
        }
    }
    // stdout carries one line per run: a created spec's comes from the create.
    #[test]
    fn the_editor_adds_a_line_only_for_an_existing_spec() {
        let path = std::path::Path::new("/p/tool.yml");

        assert_eq!(editor_success_message("tool", path, true), None);
        assert_eq!(
            editor_success_message("tool", path, false).as_deref(),
            Some("Package 'tool' updated successfully at /p/tool.yml")
        );
    }

    // Ctrl+C at the confirmation ends the run canceled.
    #[test]
    fn ctrl_c_at_the_create_confirmation_is_canceled() {
        let display = DisplayManager::new(false).answering(vec![crate::display_manager::ctrl_c()]);

        assert_eq!(confirm_new_package("fresh", &display), Err(130));
    }

    // No at the confirmation is a decline: nothing was created, so the run fails.
    #[test]
    fn declining_the_create_confirmation_fails() {
        let display =
            DisplayManager::new(false).answering(vec![crate::display_manager::answer(false)]);

        assert_eq!(confirm_new_package("fresh", &display), Err(1));
    }

    // An empty package directory with a dotfiles directory beside it, a config
    // naming them for `environment`, and a service over them.
    fn empty_packages(environment: &str) -> (TempDir, PathBuf, CliConfig, impl SpecService) {
        let temp = TempDir::new().unwrap();
        let packages = temp.path().join("packages");
        fs::create_dir_all(&packages).unwrap();
        fs::create_dir_all(temp.path().join("dotfiles")).unwrap();
        let selfie_config = test_common::test_config_with_dir_and_env(&packages, environment);
        let service = test_common::create_test_service_with_config(selfie_config.clone());
        (
            temp,
            packages,
            CliConfig::wrap_for_test(selfie_config),
            service,
        )
    }

    fn listed(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    // A spec `spec edit` creates is spec create's template: its one environment
    // is the configured one, written to `<name>.yml`.
    #[tokio::test]
    async fn a_created_spec_names_the_configured_environment() {
        let (_temp, packages, config, service) = empty_packages("work");
        let repo = common::create_package_repository(&config);
        let display =
            DisplayManager::new(false).answering(vec![crate::display_manager::answer(true)]);

        let path = create_for_edit(&service, &repo, "fresh", &config, &display)
            .await
            .unwrap();

        assert_eq!(path, packages.join("fresh.yml"));
        let spec = fs::read_to_string(&path).unwrap();
        assert!(spec.contains("work:"), "{spec}");
        assert!(!spec.contains("default:"), "{spec}");
    }

    // A name a standalone dotfile spec holds is refused before the question is
    // asked, and nothing is written.
    #[tokio::test]
    async fn a_name_a_standalone_spec_holds_is_refused_before_asking() {
        let (temp, packages, config, service) = empty_packages(test_common::TEST_ENV);
        fs::write(
            temp.path().join("dotfiles/other.yml"),
            "name: other\ndotfiles:\n  - source: other\n    target: ~/.other\n",
        )
        .unwrap();
        let repo = common::create_package_repository(&config);
        // No scripted answer: a prompt here would find no terminal and exit 2.
        let display = DisplayManager::new(false);

        let code = create_for_edit(&service, &repo, "other", &config, &display)
            .await
            .unwrap_err();

        assert_eq!(code, 1, "{:?}", display.printed());
        assert!(
            display
                .printed()
                .iter()
                .any(|(_, line)| line.contains("`spec edit` opens package specs only")),
            "{:?}",
            display.printed()
        );
        assert!(listed(&packages).is_empty());
    }

    // The create goes through the service, which reports the spec it created.
    #[tokio::test]
    async fn a_create_goes_through_the_service() {
        let (_temp, _packages, config, service) = empty_packages(test_common::TEST_ENV);
        let repo = common::create_package_repository(&config);
        let display =
            DisplayManager::new(false).answering(vec![crate::display_manager::answer(true)]);

        create_for_edit(&service, &repo, "fresh", &config, &display)
            .await
            .unwrap();

        assert!(
            display
                .printed()
                .iter()
                .any(|(_, line)| line.starts_with("Package 'fresh' created at")),
            "{:?}",
            display.printed()
        );
    }

    // A name the spec-name rule refuses is refused before the question is asked,
    // and nothing is written.
    #[tokio::test]
    async fn a_name_the_rule_refuses_is_refused_before_asking() {
        let (_temp, packages, config, service) = empty_packages(test_common::TEST_ENV);
        let repo = common::create_package_repository(&config);
        // No scripted answer: a prompt here would find no terminal and exit 2.
        let display = DisplayManager::new(false);

        let code = create_for_edit(&service, &repo, "my app", &config, &display)
            .await
            .unwrap_err();

        assert_eq!(code, 1, "{:?}", display.printed());
        assert!(
            display
                .printed()
                .iter()
                .any(|(_, line)| line.contains("'my app': it is not a valid spec name")),
            "{:?}",
            display.printed()
        );
        assert!(listed(&packages).is_empty(), "{:?}", listed(&packages));
    }

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

        let service =
            test_common::create_test_service_with_config(test_config_with_dir(&package_dir));
        let config = CliConfig::wrap_for_test(test_config_with_dir(package_dir));
        let display = DisplayManager::new(false);

        // This test will exit early because there's no EDITOR
        let result = tokio_test::block_on(handle_edit(
            &service,
            "nonexistent-package",
            &config,
            &display,
        ));

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
        let deserialized: selfie::package::Package = selfie::yaml::parse(&yaml_content).unwrap();

        // Should be equivalent
        assert_eq!(original_package.name(), deserialized.name());
        assert_eq!(original_package.description(), deserialized.description());
        assert_eq!(
            original_package.environments().len(),
            deserialized.environments().len()
        );
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
            Environments::new(),
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
