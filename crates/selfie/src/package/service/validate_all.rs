//!
//! Handles the `spec validate --all` operation — validates all specs.
//!

use crate::{
    config::SelfieConfig,
    dotfile_service::{
        collect::{Collected, collect_all_packages},
        warning::CollectionRefusal,
    },
    package::{
        Package, SpecOrigin,
        event::{
            EventSender, OperationResult, OperationSuccess, ValidationResultData, ValidationStatus,
        },
        port::PackageRepository,
        service::ProgressTracker,
    },
};

pub(super) async fn handle_validate_all<PR>(
    repo: &PR,
    dotfiles_repo: &PR,
    config: &SelfieConfig,
    sender: &EventSender,
    progress: &mut ProgressTracker,
) -> OperationResult
where
    PR: PackageRepository,
{
    // Step 1: Load all packages
    progress.next(sender, "Loading specs").await;

    // The collection apply makes, so the standalone specs it deploys are
    // validated too, and a file it would not use is reported for the reason it
    // would not use it: a name several files claim, a dotfiles copy a package
    // spec shadows, or a dotfiles directory it could not list.
    let Collected {
        packages,
        warnings,
        refusals,
        unrefused_ambiguities,
    } = match collect_all_packages(
        repo,
        dotfiles_repo,
        config.dotfiles_directory_is_expected(),
        config.environment(),
    ) {
        Ok(collected) => {
            sender.send_debug("Successfully loaded package list").await;
            collected
        }
        Err(err) => {
            return OperationResult::Failure(err.into());
        }
    };

    // Every unparsable file is reported, but only the ones apply could have used
    // are errors: a dotfiles copy a package spec shadows is not one, as it is not
    // one for apply. Collection refuses exactly those, one refusal per file.
    for warning in warnings {
        warning.send(sender).await;
    }

    // Anything else collection refused is an error here: an ambiguous name is one
    // install refuses too, and an unlistable dotfiles directory may hide specs
    // this run never saw. An ambiguity collection let through, because no file of
    // it deploys here, is still one install refuses.
    let mut unparsable = 0;
    let mut uncollected = 0;
    let unrefused = unrefused_ambiguities
        .into_iter()
        .map(|(name, paths)| CollectionRefusal::AmbiguousName { name, paths });
    for refusal in refusals.into_iter().chain(unrefused) {
        if matches!(refusal, CollectionRefusal::UnloadableSpec(_)) {
            unparsable += 1;
        } else {
            uncollected += 1;
            refusal.send(sender).await;
        }
    }

    let environment = config.environment();
    let valid_packages: Vec<&Package> = packages
        .iter()
        .filter(|package| is_validated_here(package, environment))
        .collect();

    // Step 2: Validate each package
    progress.next(sender, "Validating packages").await;

    let mut error_count: usize = 0;
    let mut warning_count: usize = 0;

    for package in &valid_packages {
        // A spec is read through the repository that loaded it, which is the one
        // that reads the files it refers to.
        let source = if package.origin() == SpecOrigin::DotfilesDirectory {
            dotfiles_repo
        } else {
            repo
        };
        let issues = &super::validate::all_issues(package, source, environment);
        let validation_issues = super::validate::issue_payload(issues);

        let status = if issues.has_errors() {
            error_count += 1;
            ValidationStatus::HasErrors
        } else if issues.has_warnings() {
            warning_count += 1;
            ValidationStatus::HasWarnings
        } else {
            ValidationStatus::Valid
        };

        let validation_result = ValidationResultData {
            package_name: package.name().to_string(),
            environment: environment.to_string(),
            status,
            issues: validation_issues,
        };

        sender.send_validation_result(validation_result).await;
    }

    let total_errors = error_count + unparsable + uncollected;

    if total_errors > 0 {
        OperationResult::Failure(
            format!(
                "Validation failed: {} package(s) with errors, {} with warnings, {} unparsable, {} ambiguous or unlistable (completed {}/{} steps)",
                error_count,
                warning_count,
                unparsable,
                uncollected,
                progress.current_step(),
                progress.total_steps()
            )
            .into(),
        )
    } else {
        OperationResult::Success(OperationSuccess::specs_validated(
            valid_packages.len(),
            0,
            warning_count,
            environment.to_string(),
            (progress.current_step(), progress.total_steps()).into(),
        ))
    }
}

/// Whether `--all` validates `package` in `environment`.
///
/// A package spec declaring another environment only is left out, as it is from
/// the run. One apply refuses here is validated whatever it declares, since the
/// mapping that would leave it out is what cannot be trusted. A standalone
/// dotfile spec declares no environment and deploys everywhere.
fn is_validated_here(package: &Package, environment: &str) -> bool {
    package.origin() == SpecOrigin::DotfilesDirectory
        || package.is_refused(environment)
        || package.environments().contains_key(environment)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::SelfieConfigBuilder,
        package::{PackageBuilder, event::PackageEvent, port::MockPackageRepository},
    };
    use tokio::sync::mpsc;

    fn test_sender() -> (EventSender, mpsc::Receiver<PackageEvent>) {
        let (tx, rx) = mpsc::channel(256);
        let sender = EventSender::new_with_context(
            tx,
            crate::package::event::metadata::OperationType::SpecValidateAll,
            String::new(),
            "test".to_string(),
            crate::package::event::OperationContext::default(),
        );
        (sender, rx)
    }

    // A dotfiles directory holding nothing, which no test here is about.
    fn empty_dotfiles() -> MockPackageRepository {
        let mut repo = MockPackageRepository::new();
        repo.expect_resolved_directory().return_const(None);
        repo.expect_list_packages()
            .returning(|| Ok(crate::package::port::ListPackagesOutput(Vec::new())));
        repo
    }

    // A fixture value, never a real credential. High-entropy and not path-shaped:
    // the scan uses a twelve-character window, so a path-like value would match
    // ordinary output and pass for the wrong reason.
    const SECRET: &str = "Xq7Rm2Kz9Wp4Ns6Tv8Bh3Gd5";

    // The event carries a `PackageParseError` now, so someone will eventually be
    // tempted to hang the file's text off it. This is what says no.
    //
    // The control matters as much as the scan: without it a run that emitted no
    // event at all would pass, and a scan for absence over an empty stream proves
    // nothing.
    #[tokio::test]
    async fn a_skipped_spec_carries_none_of_the_file_it_could_not_parse() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            temp_dir.path().join("creds.yml"),
            format!(
                "name: creds\ndotfiles:\n  - command: op read op://vault/item/field\n    \
                 vars:\n      token: {SECRET}\n    target: ~/.npmrc\nenvironments: {{oops\n"
            ),
        )
        .unwrap();

        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory(temp_dir.path())
            .build();

        let mut repo = MockPackageRepository::new();
        let dir = temp_dir.path().to_path_buf();
        repo.expect_list_packages().returning(move || {
            let repo = crate::package::repository::yaml::YamlPackageRepository::new(
                crate::fs::real::RealFileSystem,
                dir.clone(),
                crate::package::SpecOrigin::PackageDirectory,
            );
            crate::package::port::PackageRepository::list_packages(&repo)
        });

        let (sender, mut rx) = test_sender();
        repo.expect_resolved_directory().return_const(None);
        let _ = handle_validate_all(
            &repo,
            &empty_dotfiles(),
            &config,
            &sender,
            &mut ProgressTracker::new(1),
        )
        .await;
        drop(sender);

        let mut skipped = 0;
        while let Ok(event) = rx.try_recv() {
            if matches!(event, PackageEvent::SpecSkipped { .. }) {
                skipped += 1;
            }
            test_common::assert_secret_free(&format!("{event:?}"), SECRET, "an event");
        }

        // The control: the scan above proves nothing about a stream that was empty.
        assert_eq!(skipped, 1, "the unparsable spec must have been reported");
    }

    #[tokio::test]
    async fn test_validate_all_emits_per_package_results() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory(temp_dir.path())
            .build();

        let packages = vec![
            PackageBuilder::default()
                .name("ripgrep")
                .environment("macos", |b| {
                    b.install("brew install ripgrep")
                        .check_some("command -v rg")
                })
                .path(temp_dir.path().join("ripgrep.yml"))
                .build(),
            PackageBuilder::default()
                .name("node")
                .environment("macos", |b| b.install("brew install node"))
                .path(temp_dir.path().join("node.yml"))
                .build(),
        ];

        let mut mock_repo = MockPackageRepository::new();
        let packages_clone = packages.clone();
        mock_repo.expect_list_packages().returning(move || {
            Ok(crate::package::port::ListPackagesOutput(
                packages_clone.iter().cloned().map(Ok).collect(),
            ))
        });

        let (sender, mut rx) = test_sender();
        let mut progress = ProgressTracker::new(2);

        mock_repo.expect_resolved_directory().return_const(None);
        let result = handle_validate_all(
            &mock_repo,
            &empty_dotfiles(),
            &config,
            &sender,
            &mut progress,
        )
        .await;

        assert!(matches!(result, OperationResult::Success(_)));

        drop(sender);
        let mut validation_events = Vec::new();
        while let Some(event) = rx.recv().await {
            if let PackageEvent::ValidationResultCompleted {
                validation_result, ..
            } = event
            {
                validation_events.push(validation_result);
            }
        }

        // Should have one validation result per package
        assert_eq!(validation_events.len(), 2);
    }

    #[tokio::test]
    async fn test_validate_all_filters_by_environment() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory(temp_dir.path())
            .build();

        let packages = vec![
            PackageBuilder::default()
                .name("ripgrep")
                .environment("macos", |b| b.install("brew install ripgrep"))
                .path(temp_dir.path().join("ripgrep.yml"))
                .build(),
            PackageBuilder::default()
                .name("apt-tool")
                .environment("ubuntu", |b| b.install("apt install apt-tool"))
                .path(temp_dir.path().join("apt-tool.yml"))
                .build(),
        ];

        let mut mock_repo = MockPackageRepository::new();
        let packages_clone = packages.clone();
        mock_repo.expect_list_packages().returning(move || {
            Ok(crate::package::port::ListPackagesOutput(
                packages_clone.iter().cloned().map(Ok).collect(),
            ))
        });

        let (sender, mut rx) = test_sender();
        let mut progress = ProgressTracker::new(2);

        mock_repo.expect_resolved_directory().return_const(None);
        let result = handle_validate_all(
            &mock_repo,
            &empty_dotfiles(),
            &config,
            &sender,
            &mut progress,
        )
        .await;
        assert!(matches!(result, OperationResult::Success(_)));

        drop(sender);
        let mut validation_events = Vec::new();
        while let Some(event) = rx.recv().await {
            if let PackageEvent::ValidationResultCompleted {
                validation_result, ..
            } = event
            {
                validation_events.push(validation_result);
            }
        }

        // Only macos package should be validated
        assert_eq!(validation_events.len(), 1);
        assert_eq!(validation_events[0].package_name, "ripgrep");
    }
}
