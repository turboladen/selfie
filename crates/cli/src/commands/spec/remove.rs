use crate::terminal_text::DisplayPath;
use dialoguer::{Confirm, theme::SimpleTheme};
use selfie::package::{
    event::{OperationResult, PackageEvent},
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

    // The plan is what the confirmation describes: the spec as loaded and what
    // depends on it. `service.remove()` loads and scans again after the prompt.
    //
    // Calling an unreadable file "not found" points the user at the wrong
    // problem: they can see the file, so selfie looks broken and the real
    // fault -- what is wrong inside it -- goes unnamed.
    let plan = match service.prepare_removal(package_name).await {
        Ok(plan) => plan,
        Err(e) if e.means_no_such_package() => {
            display.print_error(format!("Package '{package_name}' not found."));
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

    let mut already_reported: std::collections::HashSet<std::path::PathBuf> =
        std::collections::HashSet::new();
    for invalid in unreadable {
        already_reported.insert(invalid.package_path().to_path_buf());
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

    // Perform the actual removal through the service layer
    let event_stream = service.remove(package_name).await;

    let processor = EventProcessor::new(display.clone());
    let result = processor
        .process_events(event_stream, |event| match event {
            PackageEvent::RemovalDependencyInfo { .. } => {
                // Already displayed dependency info above before confirmation
                true
            }
            // The service repeats the dependency check, so a file named above
            // the prompt arrives here a second time and is dropped. Matched by
            // path rather than suppressed wholesale: the pre-flight can report
            // nothing at all -- its own listing failed -- and the repeat can
            // then be the only pass that sees the file. Swallowing every event
            // would make that the one case where nothing is said.
            PackageEvent::SpecSkipped { error, .. } => {
                already_reported.contains(error.package_path())
            }
            PackageEvent::Completed { result, .. } => {
                if let OperationResult::Success(_) = result {
                    display.print_success(format!(
                        "Package '{}' removed successfully from {}",
                        package_name,
                        DisplayPath::of(package_blob.file_path())
                    ));

                    if !dependent_packages.is_empty() {
                        display.print_warning(
                            "Note: The following packages may have broken dependencies:",
                        );
                        for dep in dependent_packages {
                            display.print_warning(format!("  - {}", dep.name()));
                        }
                        display.print_suggestion(
                            "You may need to update these packages to remove the dependency.",
                        );
                    }
                    return true;
                }
                false // Let default handling show the error
            }
            _ => false,
        })
        .await;

    result.exit
}

#[cfg(test)]
mod tests {
    use selfie::fs::MockFileSystem;
    use std::path::PathBuf;
    use test_common::test_config_with_dir;

    use crate::config::CliConfig;
    use crate::event_processor::Exit;

    use crate::commands::common;

    // A spec in a fresh package directory, and a service over that directory.
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
