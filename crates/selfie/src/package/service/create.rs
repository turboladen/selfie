//!
//! Helps break down the pieces of running the `package create` command.
//!

use crate::{
    config::SelfieConfig,
    package::{
        Package, SpecOrigin,
        event::{
            EventSender, OperationFailure, OperationResult, OperationSuccess, Outcome,
            ValidationResultData, ValidationStatus,
        },
        port::{PackageRepoError, PackageRepository},
        service::{
            ProgressTracker,
            validate::{all_issues, issue_payload},
        },
    },
};

pub(super) async fn handle_create<PR>(
    package: Package,
    repo: &PR,
    config: &SelfieConfig,
    sender: &EventSender,
    progress: &mut ProgressTracker,
) -> OperationResult
where
    PR: PackageRepository,
{
    let package_name = package.name().to_string();

    // The spec-name rule the loader applies to a file's stem, so create never
    // writes a spec that every later command refuses to load. One check here
    // serves every adapter. First, before any lookup: a name such as
    // `../../x/y` would otherwise have the guards below report whether a file
    // exists outside the package directory.
    if !crate::package::is_valid_spec_name(&package_name) {
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "Refusing to create '{package_name}': it is not a valid spec name. Rename it: {}.",
            crate::package::SPEC_NAME_RULE
        )));
    }

    // Step 1: Check if package already exists
    progress
        .next(sender, "Checking if package already exists")
        .await;

    // Only a package that is genuinely absent may be created. Any other answer
    // means a file is there -- unparsable, refused, or ambiguous between two
    // names -- and creating would write over it. `save_package`'s guards cannot
    // help here: the package being written was built in memory, so it carries no
    // top-level keys to refuse over (selfie-3p8a).
    match repo.get_package(&package_name) {
        Ok(existing) => {
            let error = crate::package::port::PackageError::PackageAlreadyExists {
                name: package_name,
                file_path: existing.file_path().to_path_buf(),
            };
            let error_msg = error.to_string();
            sender
                .send_warning(format!("Package already exists: {error_msg}"))
                .await;
            return OperationResult::Failure(error.into());
        }
        Err(e) if e.means_no_such_package() => {}
        // Not about a file at this name: the package directory cannot be listed, or
        // something that is not a directory holds its path. The error's own sentence
        // names the directory and what is there.
        Err(e @ PackageRepoError::PackageListError(_)) => {
            sender
                .send_warning(format!("Refusing to create '{package_name}': {e}"))
                .await;
            return OperationResult::Failure(e.into());
        }
        Err(e) => {
            sender
                .send_warning(format!(
                    "Refusing to create '{package_name}': something is already there that selfie \
                     could not use, so creating would overwrite it. {e}"
                ))
                .await;
            return OperationResult::Failure(e.into());
        }
    }

    // The name said nothing was there; ask the file system about the path.
    //
    // Names fold case and extension, so `Neovim.yml` or `neovim.yaml` is caught
    // above, and so is a directory named like a spec, which the read refuses. What
    // reaches this is a path the file system matches and selfie's names do not: a
    // different Unicode normalization on a normalization-insensitive file system,
    // another spec's file at an interactive file name, or something created after
    // the lookup. Whatever it is, selfie must not write over it (selfie-6cg2).
    if repo.path_is_occupied(package.path()) {
        let path = package.path().to_path_buf();
        sender
            .send_warning(format!(
                "Refusing to create '{package_name}': {} is already taken by something selfie \
                 did not find under that name; selfie will not write over it.",
                path.display()
            ))
            .await;
        // A distinct variant from `PackageAlreadyExists`: the name really is
        // free, and reporting that the package exists would send someone
        // looking for a spec that answers to it. Consumers that only see the
        // error -- the MCP server among them -- get the accurate reason.
        return OperationResult::Failure(
            crate::package::port::PackageError::PackagePathOccupied {
                name: package_name,
                file_path: path,
            }
            .into(),
        );
    }

    // Create always writes into the package directory, so the spec is judged as a
    // package spec whatever the caller built it as: a standalone dotfile spec may
    // declare no environments, and a package spec may not.
    let mut package = package;
    package.set_origin(SpecOrigin::PackageDirectory);

    // The rule `spec validate` applies, so create never writes a spec that every
    // later command reports as broken or apply refuses.
    let issues = all_issues(&package, repo, config.environment());
    // Scored as `spec validate` scores it; only a clean spec with nothing at all to
    // say sends no result.
    let status = match issues.outcome() {
        Outcome::Failed => {
            return OperationResult::Failure(OperationFailure::InvalidSpec {
                package_name,
                issues: issue_payload(&issues),
            });
        }
        Outcome::Found => Some(ValidationStatus::HasWarnings),
        Outcome::Clean if issues.all_issues().is_empty() => None,
        Outcome::Clean => Some(ValidationStatus::Valid),
    };
    if let Some(status) = status {
        sender
            .send_validation_result(ValidationResultData {
                package_name: package_name.clone(),
                environment: config.environment().to_string(),
                status,
                issues: issue_payload(&issues),
            })
            .await;
    }

    // Step 2: Save the package
    progress.next(sender, "Saving package file").await;

    let file_path = package.path().to_path_buf();

    if let Err(err) = repo.save_package(&package, &file_path) {
        return OperationResult::Failure(err.into());
    }

    sender
        .send_debug(format!(
            "Package '{}' saved to {}",
            package_name,
            file_path.display()
        ))
        .await;

    OperationResult::Success(OperationSuccess::package_created(
        package_name,
        file_path,
        config.environment().to_string(),
        (progress.current_step(), progress.total_steps()).into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::SelfieConfigBuilder,
        package::{
            PackageBuilder,
            event::{
                OperationContext, OperationFailure, PackageEvent, ValidationLevel,
                metadata::OperationType,
            },
            port::{
                MockPackageRepository, PackageError, PackageListError, PackageParseError,
                PackageRepoError,
            },
        },
    };
    use std::path::PathBuf;
    use tokio::sync::mpsc;

    fn test_sender() -> (EventSender, mpsc::Receiver<PackageEvent>) {
        let (tx, rx) = mpsc::channel(256);
        let sender = EventSender::new_with_context(
            tx,
            OperationType::PackageCreate,
            "myapp".to_string(),
            "test".to_string(),
            OperationContext::default(),
        );
        (sender, rx)
    }

    fn fixture() -> (tempfile::TempDir, crate::config::SelfieConfig, Package) {
        let temp = tempfile::TempDir::new().unwrap();
        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory(temp.path())
            .build();
        let path = temp.path().join("myapp.yml");
        let package = PackageBuilder::default()
            .name("myapp")
            .environment("test", |b| b.install("true"))
            .path(&path)
            .build();
        (temp, config, package)
    }

    // A real parse failure, not a synthesized one, so the fixture cannot drift
    // from what the repository actually returns.
    fn a_real_parse_error() -> PackageError {
        let source = crate::yaml::parse::<Package>("name: [unclosed")
            .expect_err("fixture must fail to parse");
        PackageError::ParseError {
            name: "myapp".to_string(),
            packages_path: PathBuf::from("/packages"),
            failed_file: PathBuf::from("/packages/myapp.yml"),
            source: PackageParseError::new(
                PathBuf::from("/packages/myapp.yml"),
                crate::package::port::PackageParseKind::Yaml { source },
            ),
        }
    }

    // A repository where the name is free and the path is not taken, ready to
    // save once if `saves` is 1, or to refuse any save if it is 0.
    fn a_free_name(saves: usize) -> MockPackageRepository {
        let mut repo = MockPackageRepository::new();
        repo.expect_get_package().returning(|name| {
            Err(PackageError::PackageNotFound {
                name: name.to_string(),
                packages_path: PathBuf::from("/packages"),
                files_examined: 0,
                search_patterns: vec![],
            }
            .into())
        });
        repo.expect_path_is_occupied().returning(|_| false);
        repo.expect_save_package()
            .times(saves)
            .returning(|_, _| Ok(()));
        repo
    }

    // Every validation result the run sent.
    fn validation_results(rx: &mut mpsc::Receiver<PackageEvent>) -> Vec<ValidationResultData> {
        let mut results = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let PackageEvent::ValidationResultCompleted {
                validation_result, ..
            } = event
            {
                results.push(validation_result);
            }
        }
        results
    }

    // A spec `spec validate` would report an error for is refused before anything is
    // written, and the failure carries the issue rather than a sentence about it.
    #[tokio::test]
    async fn create_refuses_a_spec_that_would_not_validate() {
        let (temp, config, _) = fixture();
        let (sender, _rx) = test_sender();
        let mut progress = ProgressTracker::new(2);
        let package = PackageBuilder::default()
            .name("myapp")
            .environment("test", |b| b.install(""))
            .path(temp.path().join("myapp.yml"))
            .build();

        let repo = a_free_name(0);
        let result = handle_create(package, &repo, &config, &sender, &mut progress).await;

        let OperationResult::Failure(OperationFailure::InvalidSpec {
            package_name,
            issues,
        }) = result
        else {
            panic!("expected the create to be refused as invalid, got: {result:?}");
        };
        assert_eq!(package_name, "myapp");
        assert!(
            issues
                .iter()
                .any(|issue| matches!(issue.level, ValidationLevel::Error)
                    && issue.field == "environments.test.install"
                    && issue.message.contains("Install command is required")),
            "got: {issues:?}"
        );
    }

    // A spec the caller built as a standalone dotfile spec is still written into the
    // package directory, so it is held to the package rule: no environments is an
    // error there, and nothing is written.
    #[tokio::test]
    async fn create_judges_a_spec_by_where_it_is_written() {
        let (temp, config, _) = fixture();
        let (sender, _rx) = test_sender();
        let mut progress = ProgressTracker::new(2);
        let package = PackageBuilder::default()
            .name("myapp")
            .origin(SpecOrigin::DotfilesDirectory)
            .path(temp.path().join("myapp.yml"))
            .build();

        let repo = a_free_name(0);
        let result = handle_create(package, &repo, &config, &sender, &mut progress).await;

        let OperationResult::Failure(OperationFailure::InvalidSpec { issues, .. }) = result else {
            panic!("expected the create to be refused as invalid, got: {result:?}");
        };
        assert!(
            issues.iter().any(|issue| issue.field == "environments"
                && matches!(issue.level, ValidationLevel::Error)),
            "got: {issues:?}"
        );
    }

    // A package spec with no environment at all, which apply refuses to deploy.
    #[tokio::test]
    async fn create_refuses_a_spec_with_no_environments() {
        let (temp, config, _) = fixture();
        let (sender, _rx) = test_sender();
        let mut progress = ProgressTracker::new(2);
        let package = PackageBuilder::default()
            .name("myapp")
            .path(temp.path().join("myapp.yml"))
            .build();

        let repo = a_free_name(0);
        let result = handle_create(package, &repo, &config, &sender, &mut progress).await;

        let OperationResult::Failure(OperationFailure::InvalidSpec { issues, .. }) = result else {
            panic!("expected the create to be refused as invalid, got: {result:?}");
        };
        assert!(
            issues.iter().any(|issue| issue.field == "environments"
                && matches!(issue.level, ValidationLevel::Error)),
            "got: {issues:?}"
        );
    }

    // Selfie runs commands through the user's own shell, so a command that does not
    // parse as POSIX sh, such as fish's `\'` inside single quotes or a trailing
    // backslash, is written and reported as a warning, never refused.
    #[tokio::test]
    async fn create_writes_a_command_that_is_not_posix_sh() {
        let (temp, config, _) = fixture();
        for command in [r"echo 'it\'s fish'", r"echo foo \"] {
            let (sender, mut rx) = test_sender();
            let mut progress = ProgressTracker::new(2);
            let package = PackageBuilder::default()
                .name("myapp")
                .environment("test", |b| b.install(command))
                .path(temp.path().join("myapp.yml"))
                .build();

            let repo = a_free_name(1);
            let result = handle_create(package, &repo, &config, &sender, &mut progress).await;

            assert!(
                matches!(result, OperationResult::Success(_)),
                "{command}: got: {result:?}"
            );
            let results = validation_results(&mut rx);
            assert!(
                results
                    .iter()
                    .any(|result| result.issues.iter().any(|issue| matches!(
                        issue.level,
                        ValidationLevel::Warning
                    ) && issue
                        .message
                        .contains("does not parse as POSIX sh"))),
                "{command}: got: {results:?}"
            );
        }
    }

    // Warnings do not stop a create, and they are not swallowed either: the run
    // sends them as a validation result and still writes the spec.
    #[tokio::test]
    async fn create_writes_a_spec_with_warnings_and_reports_them() {
        let (temp, config, _) = fixture();
        let (sender, mut rx) = test_sender();
        let mut progress = ProgressTracker::new(2);
        // The configured environment is `test`; this spec configures only `other`.
        let package = PackageBuilder::default()
            .name("myapp")
            .environment("other", |b| b.install("true"))
            .path(temp.path().join("myapp.yml"))
            .build();

        let repo = a_free_name(1);
        let result = handle_create(package, &repo, &config, &sender, &mut progress).await;

        assert!(
            matches!(result, OperationResult::Success(_)),
            "got: {result:?}"
        );
        let results = validation_results(&mut rx);
        assert_eq!(results.len(), 1, "got: {results:?}");
        assert!(
            results[0]
                .issues
                .iter()
                .any(|issue| matches!(issue.level, ValidationLevel::Warning)
                    && issue
                        .message
                        .contains("Current environment 'test' is not configured")),
            "got: {results:?}"
        );
    }

    // A notice is reported too, with no warning beside it: it is what a reader of
    // a package that runs commands needs to see.
    #[tokio::test]
    async fn create_reports_a_notice_with_no_warning() {
        let (temp, config, _) = fixture();
        let (sender, mut rx) = test_sender();
        let mut progress = ProgressTracker::new(2);
        let entry: crate::package::DotfileEntry =
            crate::yaml::parse("command: echo key\ntarget: ~/.key\n").expect("fixture must parse");
        let package = PackageBuilder::default()
            .name("myapp")
            .environment("test", |b| b.install("true"))
            .dotfiles(vec![entry])
            .path(temp.path().join("myapp.yml"))
            .build();

        let repo = a_free_name(1);
        let result = handle_create(package, &repo, &config, &sender, &mut progress).await;

        assert!(
            matches!(result, OperationResult::Success(_)),
            "got: {result:?}"
        );
        let results = validation_results(&mut rx);
        assert_eq!(results.len(), 1, "got: {results:?}");
        assert!(
            results[0]
                .issues
                .iter()
                .any(|issue| matches!(issue.level, ValidationLevel::Info)
                    && issue.message.contains("executes 1 command(s)")),
            "got: {results:?}"
        );
        assert!(
            matches!(results[0].status, ValidationStatus::Valid),
            "a notice is not a warning, got: {results:?}"
        );
    }

    // A path can be held by something no name resolves to -- a directory, or a
    // file selfie will not load as a spec -- so the file system has to be asked
    // about the path even when the name check came back clean.
    #[tokio::test]
    async fn create_refuses_when_the_path_is_already_taken() {
        let (_temp, config, package) = fixture();
        let (sender, mut rx) = test_sender();
        let mut progress = ProgressTracker::new(2);

        let mut repo = MockPackageRepository::new();
        // The name is genuinely free -- this is the case the name check misses.
        repo.expect_get_package().returning(|name| {
            Err(PackageError::PackageNotFound {
                name: name.to_string(),
                packages_path: PathBuf::from("/packages"),
                files_examined: 0,
                search_patterns: vec![],
            }
            .into())
        });
        repo.expect_path_is_occupied().returning(|_| true);
        // The assertion that matters: nothing is written.
        repo.expect_save_package().times(0);

        let result = handle_create(package, &repo, &config, &sender, &mut progress).await;

        // Which refusal it is matters as much as that it refused. Reporting
        // that the package already exists would be false here -- the name check
        // above found nothing -- and sends a reader looking for a spec that
        // answers to the name. Matching the variant rather than its rendering
        // keeps that pinned when the wording changes.
        assert!(
            matches!(
                result,
                OperationResult::Failure(OperationFailure::Package(
                    PackageError::PackagePathOccupied { .. }
                ))
            ),
            "got: {result:?}"
        );

        // The words say what the guard catches. Capitalization never reaches it: the
        // name lookup folds case and would have found the file.
        let mut warnings = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let PackageEvent::Warning { message, .. } = event {
                warnings.push(message);
            }
        }
        assert_eq!(warnings.len(), 1, "got: {warnings:?}");
        // The warning and the error the adapters print both say the same thing, and
        // neither calls what is there a file or says it would be replaced: it may be a
        // directory, which a write refuses rather than replaces.
        let OperationResult::Failure(failure) = result else {
            unreachable!("matched as a failure above");
        };
        for said in [warnings[0].clone(), failure.to_string()] {
            assert!(
                said.contains(
                    "is already taken by something selfie did not find under that name; selfie \
                     will not write over it"
                ),
                "got: {said}"
            );
            for wrong in ["capitalization", "replace", "file"] {
                assert!(!said.contains(wrong), "{wrong}: {said}");
            }
        }
    }

    // A file that is present but will not parse is not an absent package.
    // Creating over it destroys what the user wrote, and `save_package`'s guards
    // cannot catch it: the package being written was built in memory, so it has
    // no top-level keys to refuse over.
    #[tokio::test]
    async fn create_refuses_when_a_file_is_there_but_cannot_be_read() {
        let (_temp, config, package) = fixture();
        let (sender, _rx) = test_sender();
        let mut progress = ProgressTracker::new(2);

        let mut repo = MockPackageRepository::new();
        repo.expect_path_is_occupied().returning(|_| false);
        repo.expect_get_package()
            .returning(|_| Err(a_real_parse_error().into()));
        // The assertion that matters: nothing is written.
        repo.expect_save_package().times(0);

        let result = handle_create(package, &repo, &config, &sender, &mut progress).await;

        assert!(
            matches!(result, OperationResult::Failure(_)),
            "got: {result:?}"
        );
    }

    // The control. A genuinely absent package must still be creatable, or the
    // guard above has simply broken `spec create`.
    #[tokio::test]
    async fn create_still_writes_when_no_file_is_there() {
        let (_temp, config, package) = fixture();
        let (sender, mut rx) = test_sender();
        let mut progress = ProgressTracker::new(2);

        let mut repo = MockPackageRepository::new();
        repo.expect_path_is_occupied().returning(|_| false);
        repo.expect_get_package().returning(|name| {
            Err(PackageError::PackageNotFound {
                name: name.to_string(),
                packages_path: PathBuf::from("/packages"),
                files_examined: 0,
                search_patterns: vec![],
            }
            .into())
        });
        repo.expect_save_package().times(1).returning(|_, _| Ok(()));

        let result = handle_create(package, &repo, &config, &sender, &mut progress).await;

        assert!(
            matches!(result, OperationResult::Success(_)),
            "a package with no file must still be created, got: {result:?}"
        );
        // A clean spec has nothing to report, so no validation result is sent.
        let results = validation_results(&mut rx);
        assert!(results.is_empty(), "got: {results:?}");
    }

    // Every adapter creates through here, so this one check keeps both the CLI
    // and the MCP server from writing a spec the loader then refuses.
    #[tokio::test]
    async fn create_refuses_a_name_the_loader_would_refuse() {
        let (temp, config, _) = fixture();
        let (sender, _rx) = test_sender();
        let mut progress = ProgressTracker::new(2);
        let package = PackageBuilder::default()
            .name("my tool")
            .environment("test", |b| b.install("true"))
            .path(temp.path().join("my tool.yml"))
            .build();

        // Nothing is asked of the repository at all: not written, and not
        // looked up either, so a name cannot probe the file system.
        let mut repo = MockPackageRepository::new();
        repo.expect_get_package().times(0);
        repo.expect_path_is_occupied().times(0);
        repo.expect_save_package().times(0);

        let result = handle_create(package, &repo, &config, &sender, &mut progress).await;

        match result {
            OperationResult::Failure(OperationFailure::Generic(message)) => {
                assert!(message.contains("'my tool'"), "got: {message}");
                assert!(message.contains("'@' and '+'"), "got: {message}");
            }
            other => panic!("expected the create to be refused, got: {other:?}"),
        }
    }

    // The second control. A package directory that does not exist yet holds
    // nothing to overwrite, and the save creates it -- refusing here would mean
    // no first package could be created on a fresh machine.
    #[tokio::test]
    async fn create_still_writes_when_the_package_directory_is_not_there() {
        let (_temp, config, package) = fixture();
        let (sender, _rx) = test_sender();
        let mut progress = ProgressTracker::new(2);

        let mut repo = MockPackageRepository::new();
        repo.expect_path_is_occupied().returning(|_| false);
        repo.expect_get_package().returning(|_| {
            Err(PackageRepoError::PackageListError(PackageListError::new(
                PathBuf::from("/packages"),
                crate::fs::DirectoryState::Absent(crate::fs::AbsentReason::Empty),
            )))
        });
        repo.expect_save_package().times(1).returning(|_, _| Ok(()));

        let result = handle_create(package, &repo, &config, &sender, &mut progress).await;

        assert!(
            matches!(result, OperationResult::Success(_)),
            "a missing package directory is not a file to overwrite, got: {result:?}"
        );
    }

    // A file at the package directory's path holds no spec, but the save's
    // `create_dir_all` fails against it, so the create refuses before writing and
    // says what is there.
    #[tokio::test]
    async fn create_refuses_when_a_file_holds_the_package_directory() {
        let (_temp, config, package) = fixture();
        let (sender, _rx) = test_sender();
        let mut progress = ProgressTracker::new(2);

        let mut repo = MockPackageRepository::new();
        repo.expect_get_package().returning(|_| {
            Err(PackageRepoError::PackageListError(PackageListError::new(
                PathBuf::from("/packages"),
                crate::fs::DirectoryState::Absent(crate::fs::AbsentReason::Occupied {
                    kind: "regular file",
                }),
            )))
        });
        repo.expect_path_is_occupied().times(0);
        // The assertion that matters: the write that would reach `create_dir_all`
        // never happens.
        repo.expect_save_package().times(0);

        let result = handle_create(package, &repo, &config, &sender, &mut progress).await;

        let OperationResult::Failure(failure) = result else {
            panic!("a file at the package directory must refuse, got: {result:?}");
        };
        let rendered = failure.to_string();
        assert!(
            rendered.contains("/packages is not a directory, it is a regular file"),
            "the refusal must name the directory and what is there, got: {rendered}"
        );
    }
}
