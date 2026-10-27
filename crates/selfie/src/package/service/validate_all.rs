//!
//! Handles the `spec validate --all` operation — validates all specs.
//!

use crate::{
    config::SelfieConfig,
    package::{
        Package, SpecOrigin,
        collect::{
            Collected, collect_all_packages,
            warning::{CollectionRefusal, Finding},
        },
        event::{EventSender, OperationResult, OperationSuccess, Outcome, ValidationResultData},
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
        findings,
        unrefused_ambiguities,
        set_aside: _,
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

    // The warnings count apart from specs with warnings, since some name no spec
    // at all. It counts only collection's warnings: whatever collection refused
    // is in `unvalidated`, which the result names one by one, and counting it
    // again would leave a warning count pointing at nothing a reader can find.
    let mut collection_warnings = 0;
    let mut unvalidated = Vec::new();
    let unrefused = unrefused_ambiguities
        .into_iter()
        .map(|(name, paths, base)| {
            Finding::Refusal(CollectionRefusal::AmbiguousName { name, paths, base })
        });
    // Every unparsable file is reported, but only the ones apply could have used
    // are errors: a dotfiles copy a package spec shadows is not one, as it is not
    // one for apply. Collection refuses exactly those, one refusal per file.
    //
    // Anything else collection refused is an error here: an ambiguous name is one
    // install refuses too, and an unlistable dotfiles directory may hide specs
    // this run never saw. An ambiguity collection let through, because no file of
    // it deploys here, is still one install refuses.
    for finding in findings.into_iter().chain(unrefused) {
        finding.send(sender).await;
        match finding {
            Finding::Warning(_) => collection_warnings += 1,
            Finding::Refusal(refusal) => unvalidated.push(refusal),
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

    // Collisions across specs are a property of the collection, not of any one
    // file, so they are asked of every collected spec and reported on each member
    // this run validates.
    let mut across = crate::package::validate::cross_spec_issues(&packages, environment);

    for package in &valid_packages {
        // A spec is read through the repository that loaded it, which is the one
        // that reads the files it refers to.
        let source = if package.origin() == SpecOrigin::DotfilesDirectory {
            dotfiles_repo
        } else {
            repo
        };
        let mut issues = super::validate::all_issues(package, source, environment);
        issues.extend(across.remove(package.path()).unwrap_or_default());
        let validation_issues = super::validate::issue_payload(&issues);

        let outcome = issues.outcome();
        match outcome {
            Outcome::Failed => error_count += 1,
            Outcome::Found => warning_count += 1,
            Outcome::Clean => {}
        }

        let validation_result = ValidationResultData {
            package_name: package.name().to_string(),
            environment: environment.to_string(),
            outcome,
            issues: validation_issues,
        };

        sender.send_validation_result(validation_result).await;
    }

    // A run with errors is still a validation that answered: its counts are the
    // answer, and its outcome, Failed, is what makes the run fail.
    OperationResult::Success(OperationSuccess::specs_validated(
        valid_packages.len(),
        error_count,
        unvalidated,
        warning_count,
        collection_warnings,
        environment.to_string(),
        (progress.current_step(), progress.total_steps()).into(),
    ))
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
        repo.expect_directory_identity().return_const(None);
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
        repo.expect_directory_identity().return_const(None);
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

    // What a run reports about the files it read.
    struct Validated {
        unvalidated: Vec<CollectionRefusal>,
        validated: usize,
        other_warnings: usize,
    }

    // An empty package directory and dotfiles directory, under a temporary one.
    fn directories() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let temp = tempfile::TempDir::new().unwrap();
        let packages = temp.path().join("packages");
        let dotfiles = temp.path().join("dotfiles");
        std::fs::create_dir_all(&packages).unwrap();
        std::fs::create_dir_all(&dotfiles).unwrap();
        (temp, packages, dotfiles)
    }

    // The result of validating `packages`, and `dotfiles` as the dotfiles
    // directory, each read from disk as apply reads them.
    async fn validated(packages: &std::path::Path, dotfiles: &std::path::Path) -> Validated {
        use crate::{fs::real::RealFileSystem, package::repository::yaml::YamlPackageRepository};

        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory(packages)
            .dotfiles_directory(dotfiles.to_path_buf())
            .build();
        let repo = YamlPackageRepository::new(
            RealFileSystem,
            packages.to_path_buf(),
            SpecOrigin::PackageDirectory,
        );
        let dotfiles_repo = YamlPackageRepository::new(
            RealFileSystem,
            dotfiles.to_path_buf(),
            SpecOrigin::DotfilesDirectory,
        );
        let (sender, _rx) = test_sender();
        let result = handle_validate_all(
            &repo,
            &dotfiles_repo,
            &config,
            &sender,
            &mut ProgressTracker::new(2),
        )
        .await;
        match result {
            OperationResult::Success(OperationSuccess::SpecsValidated {
                unvalidated,
                validated_count,
                other_warning_count,
                ..
            }) => Validated {
                unvalidated,
                validated: validated_count,
                other_warnings: other_warning_count,
            },
            other => panic!("validation must answer, got: {other:?}"),
        }
    }

    const GOOD: &str = "name: good\nenvironments:\n  test:\n    install: \"true\"\n";

    // An unparsable spec is named in the result, where it failed, and is not also
    // counted as a warning about the run.
    #[tokio::test]
    async fn an_unparsable_spec_is_named_in_the_result() {
        let (_temp, packages, dotfiles) = directories();
        std::fs::write(packages.join("good.yml"), GOOD).unwrap();
        std::fs::write(
            packages.join("creds.yml"),
            "name: creds\nenvironments: {oops\n",
        )
        .unwrap();

        let Validated {
            unvalidated,
            validated,
            other_warnings,
        } = validated(&packages, &dotfiles).await;

        assert_eq!(validated, 1, "the readable spec is still validated");
        assert_eq!(other_warnings, 0, "{unvalidated:?}");
        let [CollectionRefusal::UnloadableSpec { error, .. }] = unvalidated.as_slice() else {
            panic!("the unparsable spec must be the one refusal, got: {unvalidated:?}");
        };
        assert_eq!(error.package_path(), packages.join("creds.yml"));
        assert_eq!(
            error.location().map(|at| at.to_string()).as_deref(),
            Some("line 2, column 15")
        );
    }

    // A dotfiles spec that does not parse, under a name a package spec claims, is
    // one nothing would have used: a warning about the run, not a refusal.
    #[tokio::test]
    async fn a_shadowed_unparsable_spec_is_a_warning_and_not_named_as_unvalidated() {
        let (_temp, packages, dotfiles) = directories();
        std::fs::write(packages.join("good.yml"), GOOD).unwrap();
        std::fs::write(dotfiles.join("good.yaml"), "name: good\nenvironments: [\n").unwrap();

        let Validated {
            unvalidated,
            validated,
            other_warnings,
        } = validated(&packages, &dotfiles).await;

        assert_eq!(validated, 1);
        assert!(unvalidated.is_empty(), "{unvalidated:?}");
        // The skipped spec, and the notice that packages/ wins the name.
        assert_eq!(other_warnings, 2);
    }

    // A name two spec files claim is named in the result with both files.
    #[tokio::test]
    async fn an_ambiguous_name_is_named_in_the_result() {
        let (_temp, packages, dotfiles) = directories();
        std::fs::write(packages.join("nv.yml"), GOOD.replace("good", "nv")).unwrap();
        std::fs::write(packages.join("nv.yaml"), GOOD.replace("good", "nv")).unwrap();

        let Validated {
            unvalidated,
            other_warnings,
            ..
        } = validated(&packages, &dotfiles).await;

        assert_eq!(other_warnings, 0);
        let [CollectionRefusal::AmbiguousName { name, paths, .. }] = unvalidated.as_slice() else {
            panic!("the ambiguous name must be the one refusal, got: {unvalidated:?}");
        };
        assert_eq!(name, "nv");
        assert_eq!(paths, &[packages.join("nv.yaml"), packages.join("nv.yml")]);
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

        mock_repo.expect_directory_identity().return_const(None);
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

        mock_repo.expect_directory_identity().return_const(None);
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
