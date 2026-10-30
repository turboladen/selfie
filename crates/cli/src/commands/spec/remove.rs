use crate::terminal_text::DisplayPath;
use dialoguer::{Confirm, theme::SimpleTheme};
use selfie::package::{
    event::{OperationResult, OperationSuccess, PackageEvent},
    service::SpecService,
};

use crate::display_manager::DisplayManager;
use crate::event_processor::{EventProcessor, Exit};
use tracing::info;

pub(crate) async fn handle_remove(
    service: &impl SpecService,
    package_name: &str,
    display: &DisplayManager,
    skip_confirm: bool,
) -> Exit {
    info!("Removing package: {}", package_name);

    // The plan is what the confirmation describes, and the removal is checked
    // against it: `remove_shown` deletes the file only if it still holds the
    // bytes loaded here and the name still resolves to it. Removing by name would
    // load whatever is there after the prompt, an edit made while the user
    // decided included, and delete it.
    //
    // Calling an unreadable file "not found" points the user at the wrong
    // problem: they can see the file, so selfie looks broken and the real
    // fault -- what is wrong inside it -- goes unnamed.
    let plan = match service.prepare_removal(package_name).await {
        Ok(plan) => plan,
        // A not-found answer that names a spec declaring this name says so in its
        // own sentence; one with nothing to suggest keeps the short line.
        Err(e) if e.means_no_such_package() => {
            if names_a_declaring_spec(&e) {
                display.print_error(e.to_string());
            } else {
                display.print_error(format!("Package '{package_name}' not found."));
            }
            return Exit::Failed;
        }
        // Not about a file at this name, so "could not load that spec" would be false:
        // there may be no spec at all. The error names the directory and what is at it.
        Err(e @ selfie::package::port::PackageRepoError::PackageListError(_)) => {
            display.print_error(format!("Cannot remove '{package_name}': {e}"));
            return Exit::Failed;
        }
        // Everything else the repository can answer here: a file that will not
        // parse, one selfie declined to open, several files claiming the name,
        // a directory it could not list. The sentence must fit all of them, so
        // it says what selfie could not do and leaves the cause to `e`, which
        // already names the files and says what to do about them.
        Err(e) => {
            display.print_error(format!(
                "Cannot remove '{package_name}': selfie could not load that spec, so it cannot \
                 tell what depends on it. Resolve what is reported below, then try again. {e}"
            ));
            return Exit::Failed;
        }
    };

    let package_blob = plan.shown();
    display.print_run_note(format!("Package '{package_name}' found at:"));
    display.print_run_note(format!("  {}", DisplayPath::of(package_blob.file_path())));

    // `checked` is what separates "nothing depends on this" from "selfie does not
    // know". An empty dependents list means the first only when the scan ran.
    let checked = match plan.scan_failure() {
        None => true,
        Some(e) => {
            display.print_warning(format!("Could not check for dependent packages: {e}"));
            false
        }
    };
    let dependent_packages = plan.dependents();
    let unreadable = plan.unreadable();

    for invalid in unreadable {
        display.print_warning(selfie::package::service::skipped_spec_warning(invalid));
    }

    // A spec selfie could not read may name this package, so neither an
    // unreadable spec nor a failed listing can support a clearance.
    let complete = checked && unreadable.is_empty();

    let shown_name = crate::terminal_text::escape(package_name);
    let (prompt, default_answer) = if dependent_packages.is_empty() && complete {
        display.print_run_note(format!(
            "Package '{package_name}' is not a dependency of any other packages."
        ));
        (format!("Remove package '{shown_name}'?"), false)
    } else if dependent_packages.is_empty() {
        display.print_warning(format!(
            "Nothing selfie could read depends on '{package_name}', but it could not read \
             everything, so this is not a clearance."
        ));
        (format!("Remove package '{shown_name}' anyway?"), false)
    } else {
        display.print_warning(format!(
            "Package '{package_name}' is a dependency of the following packages:"
        ));
        for dep in dependent_packages {
            display.print_warning(format!("  - {}", dep.name()));
        }
        (
            "Are you sure you want to remove this package?".to_string(),
            false,
        )
    };

    if !skip_confirm {
        let confirm_removal = display.prompt(
            Confirm::with_theme(&SimpleTheme)
                .with_prompt(prompt)
                .default(default_answer),
        );

        match confirm_removal {
            Ok(true) => {}
            // A remove that removed nothing did not do what it was asked.
            Ok(false) => {
                display.print_run_note("Package removal canceled.");
                return Exit::Failed;
            }
            Err(failure) => {
                return display.refuse_prompt(
                    &failure,
                    "Confirming the removal",
                    format!("Pass --yes to remove '{package_name}' without asking.").as_str(),
                );
            }
        }
    }

    // This passes the plan shown above, not the name; the comment at the load
    // says why.
    let event_stream = service.remove_shown(plan).await;

    let processor = EventProcessor::new(display.clone());
    let result = processor
        .process_events(event_stream, |event| match event {
            // The dependents were listed above the prompt, and the success line
            // below names them again as possibly broken.
            PackageEvent::RemovalDependencyInfo { .. } => true,
            PackageEvent::Completed {
                result:
                    OperationResult::Success(OperationSuccess::PackageRemoved {
                        file_path,
                        dependent_packages,
                        ..
                    }),
                ..
            } => {
                display.print_success(format!(
                    "Package '{}' removed successfully from {}",
                    package_name,
                    DisplayPath::of(file_path)
                ));

                if !dependent_packages.is_empty() {
                    display.print_warning(
                        "Note: The following packages may have broken dependencies:",
                    );
                    for dep in dependent_packages {
                        display.print_warning(format!("  - {dep}"));
                    }
                    display.print_suggestion(
                        "You may need to update these packages to remove the dependency.",
                    );
                }
                true
            }
            // Anything else, a failure included, gets the default handling.
            _ => false,
        })
        .await;

    result.exit
}

// Whether a not-found answer names a spec whose `name:` field declares the name
// that was not found, which is the hint worth showing.
fn names_a_declaring_spec(error: &selfie::package::port::PackageRepoError) -> bool {
    use selfie::package::port::{PackageError, PackageRepoError};
    let PackageRepoError::PackageError(boxed) = error else {
        return false;
    };
    matches!(&**boxed, PackageError::PackageNotFound { declared_by, .. } if !declared_by.is_empty())
}

#[cfg(test)]
mod tests {
    use selfie::fs::MockFileSystem;
    use std::path::PathBuf;
    use test_common::test_config_with_dir;

    use crate::config::CliConfig;
    use crate::event_processor::Exit;

    use crate::commands::common;

    // Returns a spec in a fresh package directory, and a service over that
    // directory.
    fn one_spec() -> (
        tempfile::TempDir,
        PathBuf,
        impl selfie::package::service::SpecService,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let packages = temp.path().join("packages");
        std::fs::create_dir(&packages).unwrap();
        let spec = packages.join("tool.yml");
        std::fs::write(
            &spec,
            format!(
                "name: tool\nenvironments:\n  {}:\n    install: \"true\"\n",
                test_common::TEST_ENV
            ),
        )
        .unwrap();
        let service = test_common::create_test_service_with_config(test_config_with_dir(&packages));
        (temp, spec, service)
    }

    // Ctrl+C at the confirmation ends the run canceled, and removes nothing.
    #[tokio::test]
    async fn ctrl_c_at_the_confirmation_is_canceled_and_keeps_the_spec() {
        use crate::display_manager::{DisplayManager, ctrl_c};

        let (_temp, spec, service) = one_spec();
        let display = DisplayManager::new(false).answering(vec![ctrl_c()]);

        let exit = super::handle_remove(&service, "tool", &display, false).await;

        assert_eq!(exit, Exit::Canceled);
        assert!(spec.exists());
    }

    // An edit that lands while the confirmation waits is the user's, and the
    // removal they confirmed was of the file as it stood before it. Nothing is
    // removed, and the run fails naming the spec.
    #[tokio::test]
    async fn an_edit_made_while_the_removal_prompt_waits_is_not_removed() {
        use crate::display_manager::{DisplayManager, answer};

        const EDITED: &str = "name: tool\ndescription: edited by hand\nenvironments: {}\n";
        let (_temp, spec, service) = one_spec();
        let edited = spec.clone();
        let display = DisplayManager::new(false)
            .on_next_prompt(move || std::fs::write(&edited, EDITED).unwrap())
            .answering(vec![answer(true)]);

        let exit = super::handle_remove(&service, "tool", &display, false).await;

        assert_eq!(
            std::fs::read_to_string(&spec).ok().as_deref(),
            Some(EDITED),
            "the edit made while the prompt waited was removed"
        );
        assert_eq!(exit, Exit::Failed);
        let expected = format!(
            "{} changed after selfie read it, so it was not removed.",
            spec.display()
        );
        let printed = display.printed();
        assert!(
            printed.iter().any(|(_, line)| line.contains(&expected)),
            "{printed:?}"
        );
    }

    // Control for the test above: a confirmed removal of a spec nobody touched
    // removes it.
    #[tokio::test]
    async fn a_confirmed_removal_of_an_unchanged_spec_removes_it() {
        use crate::display_manager::{DisplayManager, answer};

        let (_temp, spec, service) = one_spec();
        let display = DisplayManager::new(false).answering(vec![answer(true)]);

        let exit = super::handle_remove(&service, "tool", &display, false).await;

        assert_eq!(exit, Exit::Clean);
        assert!(!spec.exists(), "the confirmed removal left the spec");
    }

    // A second file claiming the name, appearing while the confirmation waits,
    // makes the name ambiguous. The removal refuses rather than deleting one of
    // the two and reporting the package removed while it still resolves.
    #[tokio::test]
    async fn a_second_spec_for_the_name_appearing_while_the_prompt_waits_refuses() {
        use crate::display_manager::{DisplayManager, answer};

        let (_temp, spec, service) = one_spec();
        let rival = spec.with_extension("yaml");
        let planted = rival.clone();
        let display = DisplayManager::new(false)
            .on_next_prompt(move || std::fs::write(&planted, "name: tool\n").unwrap())
            .answering(vec![answer(true)]);

        let exit = super::handle_remove(&service, "tool", &display, false).await;

        assert_eq!(exit, Exit::Failed);
        assert!(spec.exists(), "the spec shown at the prompt was removed");
        assert!(rival.exists(), "the file that appeared was removed");
        // The refusal names both files, as an ambiguous name does everywhere.
        let printed = display.printed();
        assert!(
            printed.iter().any(|(_, line)| line.contains("tool.yaml")),
            "{printed:?}"
        );
    }

    // A name no spec file answers to, but one spec's `name:` field declares, gets
    // the hint naming that file, as the service's own removal gives it.
    #[tokio::test]
    async fn a_name_only_a_name_field_declares_is_answered_with_the_file() {
        use crate::display_manager::DisplayManager;

        let (_temp, spec, service) = one_spec();
        let declared = std::fs::read_to_string(&spec)
            .unwrap()
            .replace("name: tool", "name: the-tool");
        std::fs::write(&spec, declared).unwrap();
        let display = DisplayManager::new(false);

        let exit = super::handle_remove(&service, "the-tool", &display, false).await;

        assert_eq!(exit, Exit::Failed);
        assert!(spec.exists());
        let printed = display.printed();
        assert!(
            printed.iter().any(|(_, line)| line.contains("tool.yml")),
            "the hint naming the declaring file is missing: {printed:?}"
        );
    }

    // No at the confirmation is a decline: the spec stays, and the run fails.
    #[tokio::test]
    async fn declining_the_removal_fails_and_keeps_the_spec() {
        use crate::display_manager::{DisplayManager, answer};

        let (_temp, spec, service) = one_spec();
        let display = DisplayManager::new(false).answering(vec![answer(false)]);

        let exit = super::handle_remove(&service, "tool", &display, false).await;

        assert_eq!(exit, Exit::Failed);
        assert!(spec.exists());
        // Everything before the question, and the decline, is about the run;
        // a run that removed nothing has no answer for stdout.
        let printed = display.printed();
        assert!(
            printed
                .iter()
                .all(|(channel, _)| *channel == crate::display_manager::Channel::Stderr),
            "{printed:?}"
        );
        assert!(
            printed.iter().any(|(_, line)| line.contains("found at")),
            "{printed:?}"
        );
    }

    #[test]
    fn test_dependency_check_integration() {
        use selfie::package::{EnvironmentConfig, Package};

        let mut target_envs = selfie::package::Environments::new();
        target_envs.insert(
            "test".to_string(),
            EnvironmentConfig::new(
                "echo 'install target'".to_string(),
                Some("echo 'check target'".to_string()),
                None,
                Vec::new(),
                Vec::new(),
            ),
        );
        let target_package = Package::new(
            "target-package".to_string(),
            None,
            None,
            Vec::new(),
            None,
            target_envs,
            PathBuf::from("/test/packages/target-package.yml"),
        );

        let mut dependent_envs = selfie::package::Environments::new();
        dependent_envs.insert(
            "test".to_string(),
            EnvironmentConfig::new(
                "echo 'install dependent'".to_string(),
                Some("echo 'check dependent'".to_string()),
                None,
                vec!["target-package".to_string()],
                Vec::new(),
            ),
        );
        let dependent_package = Package::new(
            "dependent-package".to_string(),
            None,
            None,
            Vec::new(),
            None,
            dependent_envs,
            PathBuf::from("/test/packages/dependent-package.yml"),
        );

        let target_deps = target_package.environments()["test"].dependencies();
        let dependent_deps = dependent_package.environments()["test"].dependencies();

        assert!(target_deps.is_empty());
        assert_eq!(dependent_deps.len(), 1);
        assert_eq!(dependent_deps[0], "target-package");

        assert_eq!(target_package.name(), "target-package");
        assert_eq!(dependent_package.name(), "dependent-package");
    }

    #[test]
    fn test_repository_creation_with_different_filesystems() {
        let package_dir = PathBuf::from("/test/packages");
        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));

        let repo = common::create_package_repository_with_fs(&config, selfie::fs::RealFileSystem);
        drop(repo);

        let mock_fs = MockFileSystem::default();
        let mock_repo = common::create_package_repository_with_fs(&config, mock_fs);
        drop(mock_repo);
    }

    #[test]
    fn test_remove_package_with_dependencies_mock_fs() {
        let mut mock_fs = MockFileSystem::default();
        mock_fs.mock_no_irregular_files();
        let package_dir = PathBuf::from("/test/packages");

        let target_path = package_dir.join("target-package.yml");
        let dependent_path = package_dir.join("dependent-package.yml");

        let target_yaml = r#"
name: target-package
environments:
  test:
    install: echo "install target"
    dependencies: []
"#;

        let dependent_yaml = r#"
name: dependent-package
environments:
  test:
    install: echo "install dependent"
    dependencies:
      - target-package
"#;

        mock_fs.mock_directories_exist();
        mock_fs.mock_path_exists(&package_dir, true);
        mock_fs.mock_list_directory(&package_dir, &[&target_path, &dependent_path]);
        mock_fs.mock_read_file(&target_path, target_yaml);
        mock_fs.mock_read_file(&dependent_path, dependent_yaml);

        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));
        let repo = common::create_package_repository_with_fs(&config, mock_fs);

        use selfie::package::port::PackageRepository;
        let (dependents, _unreadable) = repo.find_dependent_packages("target-package").unwrap();
        assert_eq!(dependents.len(), 1);
        assert_eq!(dependents[0].name(), "dependent-package");
    }

    #[test]
    fn test_remove_package_not_found_with_mock_repo() {
        use selfie::package::port::{MockPackageRepository, PackageError, PackageRepository};

        let mut mock_repo = MockPackageRepository::new();

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
                    declared_by: Vec::new(),
                }
                .into())
            });

        let result = mock_repo.get_package("nonexistent");
        assert!(result.is_err());
    }
}
