//!
//! Helps break down the pieces of running the `package validate` command.
//!

use crate::{
    config::SelfieConfig,
    package::{
        Package,
        collect::{
            UnplacedSpec,
            unchecked::{Checking, Others, OthersListing, send_unchecked_warnings},
        },
        event::{
            EventSender, OperationFailure, OperationResult, OperationSuccess, Outcome, SpecWrite,
            ValidationIssueData, ValidationResultData,
        },
        port::PackageRepository,
        service::ProgressTracker,
        validate::{
            CrossSpecCollision, asked_environments, cross_spec_collisions,
            unreadable_template_issue, validate_template_vars,
        },
    },
    validation::{ValidationIssue, ValidationIssues},
};

/// Check every templated dotfile's placeholders against its declared bindings.
///
/// Reads each template through the repository, since `Package::validate` is a
/// pure, offline check with no file system of its own. Never executes a binding:
/// validation must work offline and must not trigger an authentication prompt.
pub(super) fn validate_package_templates<PR>(package: &Package, repo: &PR) -> Vec<ValidationIssue>
where
    PR: PackageRepository,
{
    package
        .template_dotfiles()
        .iter()
        .flat_map(
            |reference| match repo.read_referenced_file(package.path(), reference.source) {
                Ok(template) => validate_template_vars(&template, reference),
                Err(e) => vec![unreadable_template_issue(reference, &e)],
            },
        )
        .collect()
}

/// Every issue a package has: the offline checks plus the template checks that
/// need the repository to read a file.
pub(super) fn all_issues<PR>(package: &Package, repo: &PR, environment: &str) -> ValidationIssues
where
    PR: PackageRepository,
{
    let mut issues = package.validate(environment).issues().all_issues().to_vec();
    issues.extend(validate_package_templates(package, repo));
    issues.into()
}

/// Convert issues into the event payload, errors first, then warnings, then
/// informational notices.
pub(super) fn issue_payload(issues: &ValidationIssues) -> Vec<ValidationIssueData> {
    // One conversion for every command that sends issues, so a level cannot be
    // wired into one command and forgotten in another. The match is exhaustive:
    // a level added later fails the build here. The sort is stable, so issues of
    // one level keep their order.
    let mut ordered: Vec<_> = issues.all_issues().iter().collect();
    ordered.sort_by_key(|issue| match issue.level() {
        crate::validation::ValidationLevel::Error => 0,
        crate::validation::ValidationLevel::Warning => 1,
        crate::validation::ValidationLevel::Info => 2,
    });
    ordered
        .into_iter()
        .map(|issue| ValidationIssueData {
            category: issue.category(),
            field: issue.field().to_string(),
            message: issue.message().to_string(),
            level: issue.level(),
            suggestion: issue.suggestion().map(std::string::ToString::to_string),
            location: issue.location(),
        })
        .collect()
}

/// What an issue found on a spec about to be written does to the write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OnWrite {
    /// An error with this effect refuses the write. A warning or notice is
    /// reported whatever its effect.
    Refuses,
    /// The issue is reported, and the write goes ahead.
    Reports,
}

/// Decides whether a spec about to be written may be, and tells the caller what
/// it has to say if so.
///
/// An error tagged [`OnWrite::Refuses`] refuses the write: the failure carries
/// every issue and nothing is reported. Otherwise the issues go out as a
/// `validation_result`, for warnings, notices, and errors that do not stop this
/// write, and nothing goes out for a spec with no issues. The result is sent
/// before the caller saves, so it is there when the save then fails.
///
/// Returns the [`OperationFailure::InvalidSpec`] to fail with when an error
/// stops the write, and `None` when the caller may save.
pub(super) async fn refuse_or_report(
    sender: &EventSender,
    action: SpecWrite,
    package_name: &str,
    environment: &str,
    judged: Vec<(ValidationIssue, OnWrite)>,
) -> Option<OperationFailure> {
    let refused = judged.iter().any(|(issue, on_write)| {
        issue.level() == crate::validation::ValidationLevel::Error && *on_write == OnWrite::Refuses
    });
    let issues: ValidationIssues = judged
        .into_iter()
        .map(|(issue, _)| issue)
        .collect::<Vec<_>>()
        .into();
    if refused {
        return Some(OperationFailure::InvalidSpec {
            action,
            package_name: package_name.to_string(),
            issues: issue_payload(&issues),
        });
    }
    // Any issue at all is something to say: a notice alone still reaches the user.
    if !issues.all_issues().is_empty() {
        sender
            .send_validation_result(ValidationResultData {
                package_name: package_name.to_string(),
                environment: environment.to_string(),
                outcome: issues.outcome(),
                issues: issue_payload(&issues),
            })
            .await;
    }
    None
}

/// Whether a spec's file already had a collision with another spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Standing {
    /// The spec as its file holds it did not have the collision.
    Introduced,
    /// The spec as its file holds it had the collision already.
    Preexisting,
}

impl Standing {
    /// Returns what a collision of this standing does to a write: one the write
    /// brings in refuses it, and one already there is reported.
    pub(super) fn on_write(self) -> OnWrite {
        match self {
            Self::Introduced => OnWrite::Refuses,
            Self::Preexisting => OnWrite::Reports,
        }
    }
}

/// Holds a spec's errors for entries that deploy to a target another spec deploys,
/// each with its standing.
#[derive(Debug, Default)]
pub(super) struct AcrossSpecs {
    pub(super) issues: Vec<(ValidationIssue, Standing)>,
}

/// Returns the errors for `candidate`'s entries that deploy to a target another
/// spec deploys. The specs are collected as `--all` collects them, with
/// `candidate`'s file written: it takes the place of the listed spec at its path,
/// or is listed where its file would be.
///
/// `on_disk` is the spec as its file holds it, for a write that replaces one: a
/// collision it has already is [`Standing::Preexisting`]. With `None`, every
/// collision is [`Standing::Introduced`]. An introduced collision's error names
/// only the environments where it is new.
///
/// Sends, as a run for one name does, the warnings collecting found and a warning
/// for each place it could not use, including when the spec cannot be placed. A
/// spec with no dotfile entry in any environment it declares gets neither, and no
/// collection is done for it.
///
/// # Errors
///
/// Returns [`UnplacedSpec`] when `candidate` or `on_disk` cannot be placed in the
/// listing, so the spec was compared with nothing. A write must then be refused.
/// A validation can still report the spec's own issues.
pub(super) async fn collisions_with_other_specs<PR>(
    on_disk: Option<&Package>,
    candidate: &Package,
    package_name: &str,
    repo: &PR,
    dotfiles_repo: &PR,
    config: &SelfieConfig,
    sender: &EventSender,
) -> Result<AcrossSpecs, UnplacedSpec>
where
    PR: PackageRepository,
{
    // Such a spec cannot share a target, so nothing collection found is about it.
    if !candidate.has_dotfiles_in_any_environment() {
        return Ok(AcrossSpecs::default());
    }
    // Listed once and settled twice. The names are settled by collection itself
    // with the file written, so an unparsable file still claims its name. Only an
    // update clones the listing, for the spec as its file holds it.
    let mut listing = OthersListing::of(repo, dotfiles_repo);
    let mut before = on_disk.map(|on_disk| (listing.clone(), on_disk));
    let placed = listing.place(candidate).and_then(|()| {
        before
            .as_mut()
            .map_or(Ok(()), |(before, on_disk)| before.place(on_disk))
    });
    let after = listing.collect(config);
    report_collection(&after, candidate, package_name, config, sender).await;
    placed?;

    let path = candidate.path();
    // One set for both scans. A spec's shared entries are scanned in every asked
    // environment, declared or not, so a collision the spec had already is found in
    // an environment only the candidate declares, and is not taken for a new one.
    let environments = asked_environments(&after.packages, config.environment());
    let involving = |set: &[Package]| -> Vec<CrossSpecCollision> {
        cross_spec_collisions(set, &environments)
            .into_iter()
            .filter(|collision| collision.involves(path))
            .collect()
    };
    let before = before.map_or_else(Vec::new, |(before, _)| {
        involving(&before.collect(config).packages)
    });

    // Judged per environment, not per field: an update that removes the environment
    // an unknown key had the spec refused in brings its shared entries into that
    // environment's scan, a collision on a field that already collided elsewhere.
    // The entries' fields are stable because no update edits a dotfile list; one
    // that did would shift the indices this key holds.
    let mut across = AcrossSpecs::default();
    for collision in involving(&after.packages) {
        let new = collision.new_against(&before);
        if new.is_empty() {
            let issues = collision.into_issues_at(path);
            across.issues.extend(
                issues
                    .into_iter()
                    .map(|issue| (issue, Standing::Preexisting)),
            );
        } else {
            let issues = collision.issues_at(path, &new);
            across.issues.extend(
                issues
                    .into_iter()
                    .map(|issue| (issue, Standing::Introduced)),
            );
        }
    }
    Ok(across)
}

// Sends the warnings collecting found for `package_name`, and a warning for each
// place it could not use.
async fn report_collection(
    others: &Others,
    candidate: &Package,
    package_name: &str,
    config: &SelfieConfig,
    sender: &EventSender,
) {
    // A refusal is told once, as the place it left unchecked, so only the warnings
    // are sent here. A neighbor's fault does not change the named spec's own result.
    for finding in &others.findings {
        if finding.is_warning_for(package_name) {
            finding.send(sender).await;
        }
    }
    send_unchecked_warnings(
        sender,
        &others.unchecked,
        std::slice::from_ref(candidate),
        config,
        Checking::Validate,
    )
    .await;
}

pub(super) async fn handle_validate<PR>(
    package_name: &str,
    repo: &PR,
    dotfiles_repo: &PR,
    config: &SelfieConfig,
    sender: &EventSender,
    progress: &mut ProgressTracker,
) -> OperationResult
where
    PR: PackageRepository,
{
    // Step 1: Fetch package
    progress.next(sender, "Loading package definition").await;

    // A name the package directory does not hold may be a standalone dotfile
    // spec, which apply deploys too. The package directory is asked first, and
    // wins a name both hold, as it does for apply. The dotfiles directory's
    // answer is given only when it found the spec, or a file by that name it
    // could not use. Otherwise the package directory's is, since any other
    // failure says nothing about a name the user may simply have mistyped.
    let (found, source) = match repo.get_package(package_name) {
        Err(err) if err.means_no_such_package() => match dotfiles_repo.get_package(package_name) {
            Ok(found) => (Ok(found), dotfiles_repo),
            Err(other) if other.names_an_unusable_spec() => (Err(other), dotfiles_repo),
            Err(_) => (Err(crate::package::port::with_name_hint(repo, err)), repo),
        },
        from_packages => (from_packages, repo),
    };
    let package_blob = match found {
        Ok(pkg) => {
            sender
                .send_debug(format!("Successfully loaded package: {package_name}"))
                .await;
            pkg
        }
        Err(err) => {
            return OperationResult::Failure(err.into());
        }
    };

    // Step 2: Validate the package for the current environment
    progress.next(sender, "Validating package definition").await;

    let environment = config.environment();
    let mut issues = all_issues(&package_blob.package, source, environment);
    // The spec is judged against the others as `--all` judges every spec, and
    // only its own collisions are attached.
    // A spec that cannot be placed is compared with nothing, and still has its own
    // result: the warnings already sent say what could not be checked.
    let across = collisions_with_other_specs(
        None,
        &package_blob.package,
        package_name,
        repo,
        dotfiles_repo,
        config,
        sender,
    )
    .await
    .unwrap_or_default();
    issues.extend(across.issues.into_iter().map(|(issue, _)| issue));
    let issues = &issues;

    // Step 3: Process validation results
    progress.next(sender, "Processing validation results").await;

    // Convert validation issues to structured data
    let validation_issues = issue_payload(issues);

    // Scored once, here, from the issues; every consumer reads this value.
    let outcome = issues.outcome();

    // Send structured validation result
    let validation_result = ValidationResultData {
        package_name: package_name.to_string(),
        environment: config.environment().to_string(),
        outcome,
        issues: validation_issues,
    };

    sender.send_validation_result(validation_result).await;

    // Return appropriate operation result
    match outcome {
        Outcome::Clean => {
            sender
                .send_debug("Package definition is valid for the current environment")
                .await;

            OperationResult::Success(OperationSuccess::package_validated(
                package_name.to_string(),
                config.environment().to_string(),
                Outcome::Clean,
                0,
                None,
                (progress.current_step(), progress.total_steps()).into(),
            ))
        }
        Outcome::Found => {
            let warning_count = issues.warnings().len();
            OperationResult::Success(OperationSuccess::package_validated(
                package_name.to_string(),
                config.environment().to_string(),
                Outcome::Found,
                0,
                Some(warning_count),
                (progress.current_step(), progress.total_steps()).into(),
            ))
        }
        // A spec with errors is still a validation that answered: its outcome,
        // Failed, is what makes the run fail.
        Outcome::Failed => OperationResult::Success(OperationSuccess::package_validated(
            package_name.to_string(),
            config.environment().to_string(),
            Outcome::Failed,
            issues.errors().len(),
            Some(issues.warnings().len()),
            (progress.current_step(), progress.total_steps()).into(),
        )),
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::validation::{ValidationErrorCategory, ValidationIssue};

    // The payload lists errors, then warnings, then notices, and keeps the order
    // issues of one level arrived in.
    #[test]
    fn issues_go_out_by_level_keeping_their_order() {
        type Make = fn(ValidationErrorCategory, &str, &str, Option<&str>) -> ValidationIssue;
        let issue =
            |make: Make, field| make(ValidationErrorCategory::InvalidValue, field, "m", None);
        let issues: ValidationIssues = vec![
            issue(ValidationIssue::info, "i1"),
            issue(ValidationIssue::warning, "w1"),
            issue(ValidationIssue::error, "e1"),
            issue(ValidationIssue::warning, "w2"),
            issue(ValidationIssue::error, "e2"),
        ]
        .into();

        let fields: Vec<String> = issue_payload(&issues)
            .into_iter()
            .map(|issue| issue.field)
            .collect();

        assert_eq!(fields, ["e1", "e2", "w1", "w2", "i1"]);
    }

    // A spec with no dotfile entry cannot share a target, so validating it lists no
    // directory: the mocks expect no listing, and one fails the test.
    #[tokio::test]
    async fn a_spec_with_no_entries_collects_nothing() {
        use crate::{
            config::SelfieConfigBuilder,
            package::{
                GetPackage, PackageBuilder,
                event::{OperationContext, metadata::OperationType},
                port::MockPackageRepository,
            },
        };
        let package = PackageBuilder::default()
            .name("alpha")
            .environment("test", |b| b.install("true"))
            .path("/packages/alpha.yml")
            .build();
        let mut repo = MockPackageRepository::new();
        let found = GetPackage::from_existing(package, "/packages/alpha.yml".into());
        repo.expect_get_package().return_once(move |_| Ok(found));
        repo.expect_list_packages().times(0);
        let mut dotfiles_repo = MockPackageRepository::new();
        dotfiles_repo.expect_list_packages().times(0);
        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory("/packages")
            .build();
        let (tx, _rx) = tokio::sync::mpsc::channel(32);
        let sender = EventSender::new_with_context(
            tx,
            OperationType::PackageValidate,
            "alpha".to_string(),
            "test".to_string(),
            OperationContext::default(),
        );
        let mut progress = ProgressTracker::new(3);

        let result = handle_validate(
            "alpha",
            &repo,
            &dotfiles_repo,
            &config,
            &sender,
            &mut progress,
        )
        .await;

        assert!(matches!(result, OperationResult::Success(_)), "{result:?}");
    }

    // A repository listing no spec, for a run that compares against the others and
    // has none to compare.
    pub(in crate::package::service) fn listing_nothing()
    -> crate::package::port::MockPackageRepository {
        let mut repo = crate::package::port::MockPackageRepository::new();
        repo.expect_list_packages()
            .returning(|| Ok(crate::package::port::ListPackagesOutput(Vec::new())));
        repo.expect_directory_identity().return_const(None);
        repo
    }

    // A package directory that would not list but still answered for `alpha`, which
    // shares `~/.gemrc` with the standalone `gem`: collection leaves alpha out and
    // compares the standalone specs alone.
    pub(in crate::package::service) fn alpha_beside_gem_with_packages_unlisted(
        saves: usize,
    ) -> (
        crate::config::SelfieConfig,
        crate::package::port::MockPackageRepository,
        crate::package::port::MockPackageRepository,
    ) {
        use crate::package::{
            DotfileEntry, GetPackage, PackageBuilder, SpecOrigin,
            port::{ListPackagesOutput, MockPackageRepository, PackageListError},
        };
        let alpha = PackageBuilder::default()
            .name("alpha")
            .environment("test", |b| b.install("true"))
            .dotfiles(vec![DotfileEntry::new("alpha/rc", "~/.gemrc")])
            .path("/packages/alpha.yml")
            .origin(SpecOrigin::PackageDirectory)
            .build();
        let gem = PackageBuilder::default()
            .name("gem")
            .dotfiles(vec![DotfileEntry::new("gem/rc", "~/.gemrc")])
            .path("/dotfiles/gem.yml")
            .origin(SpecOrigin::DotfilesDirectory)
            .build();
        let mut repo = MockPackageRepository::new();
        repo.expect_get_package().returning(move |_| {
            Ok(GetPackage::from_existing(
                alpha.clone(),
                "/packages/alpha.yml".into(),
            ))
        });
        repo.expect_list_packages().returning(|| {
            Err(PackageListError::new(
                "/packages".into(),
                crate::fs::DirectoryState::Unlistable(std::sync::Arc::new(std::io::Error::other(
                    "denied",
                ))),
            ))
        });
        repo.expect_directory_identity().return_const(None);
        repo.expect_save_package()
            .times(saves)
            .returning(|_, _| Ok(()));
        let mut dotfiles_repo = MockPackageRepository::new();
        dotfiles_repo
            .expect_list_packages()
            .returning(move || Ok(ListPackagesOutput(vec![Ok(gem.clone())])));
        dotfiles_repo.expect_directory_identity().return_const(None);
        let config = crate::config::SelfieConfigBuilder::default()
            .environment("test")
            .package_directory("/packages")
            .dotfiles_directory("/dotfiles".into())
            .build();
        (config, repo, dotfiles_repo)
    }

    // Every cross-spec issue in the validation results the run sent.
    pub(in crate::package::service) fn reported_collisions(
        rx: &mut tokio::sync::mpsc::Receiver<crate::package::event::PackageEvent>,
    ) -> Vec<String> {
        let mut found = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let crate::package::event::PackageEvent::ValidationResultCompleted {
                validation_result,
                ..
            } = event
            {
                found.extend(
                    validation_result
                        .issues
                        .into_iter()
                        .filter(|issue| issue.message.contains("so apply deploys none"))
                        .map(|issue| issue.field),
                );
            }
        }
        found
    }

    // A standalone spec found by name, in a dotfiles directory that then will not
    // list, cannot be placed among the others. The validation still answers with
    // the spec's own result, and says the dotfiles directory went unchecked.
    #[tokio::test]
    async fn a_validated_spec_that_cannot_be_placed_still_answers() {
        use crate::package::{
            DotfileEntry, GetPackage, PackageBuilder, SpecOrigin,
            event::{OperationContext, PackageEvent, metadata::OperationType},
            port::{ListPackagesOutput, MockPackageRepository, PackageError, PackageListError},
        };
        let gem = PackageBuilder::default()
            .name("gem")
            .dotfiles(vec![DotfileEntry::new("gem/rc", "~/.gemrc")])
            .path("/dotfiles/gem.yml")
            .origin(SpecOrigin::DotfilesDirectory)
            .build();
        let mut repo = MockPackageRepository::new();
        repo.expect_list_packages()
            .returning(|| Ok(ListPackagesOutput(Vec::new())));
        repo.expect_directory_identity().return_const(None);
        repo.expect_get_package().returning(|name| {
            Err(PackageError::PackageNotFound {
                name: name.to_string(),
                packages_path: "/packages".into(),
                files_examined: 0,
                search_patterns: vec![],
                declared_by: Vec::new(),
            }
            .into())
        });
        let mut dotfiles_repo = MockPackageRepository::new();
        dotfiles_repo
            .expect_get_package()
            .return_once(move |_| Ok(GetPackage::from_existing(gem, "/dotfiles/gem.yml".into())));
        dotfiles_repo.expect_list_packages().returning(|| {
            Err(PackageListError::new(
                "/dotfiles".into(),
                crate::fs::DirectoryState::Unlistable(std::sync::Arc::new(std::io::Error::other(
                    "denied",
                ))),
            ))
        });
        dotfiles_repo.expect_directory_identity().return_const(None);
        let config = crate::config::SelfieConfigBuilder::default()
            .environment("test")
            .package_directory("/packages")
            .dotfiles_directory("/dotfiles".into())
            .build();
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let sender = EventSender::new_with_context(
            tx,
            OperationType::PackageValidate,
            "gem".to_string(),
            "test".to_string(),
            OperationContext::default(),
        );

        let result = handle_validate(
            "gem",
            &repo,
            &dotfiles_repo,
            &config,
            &sender,
            &mut ProgressTracker::new(3),
        )
        .await;
        drop(sender);

        assert!(matches!(result, OperationResult::Success(_)), "{result:?}");
        let mut warnings = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let PackageEvent::Warning { message, .. } = event {
                warnings.push(message);
            }
        }
        assert!(
            warnings
                .iter()
                .any(|message| message.starts_with("Could not check")
                    && message.contains("dotfiles")),
            "{warnings:?}"
        );
    }

    // Collection left alpha out, so a validate of it judges the loaded spec in its
    // place and still finds its collision with the standalone spec.
    #[tokio::test]
    async fn a_validated_spec_collection_left_out_is_still_compared() {
        use crate::package::event::{OperationContext, metadata::OperationType};
        let (config, repo, dotfiles_repo) = alpha_beside_gem_with_packages_unlisted(0);
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let sender = EventSender::new_with_context(
            tx,
            OperationType::PackageValidate,
            "alpha".to_string(),
            "test".to_string(),
            OperationContext::default(),
        );

        let _ = handle_validate(
            "alpha",
            &repo,
            &dotfiles_repo,
            &config,
            &sender,
            &mut ProgressTracker::new(3),
        )
        .await;

        assert_eq!(reported_collisions(&mut rx), ["dotfiles[0].target"]);
    }
}
