//!
//! Helps break down the pieces of running the `package validate` command.
//!

use crate::{
    config::SelfieConfig,
    package::{
        Package,
        collect::unchecked::{Checking, collect_others, send_unchecked_warnings},
        event::{
            EventSender, OperationFailure, OperationResult, OperationSuccess, Outcome, SpecWrite,
            ValidationIssueData, ValidationResultData,
        },
        port::PackageRepository,
        service::ProgressTracker,
        validate::{cross_spec_issues, unreadable_template_issue, validate_template_vars},
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

/// Decide whether a spec about to be written may be, and tell the caller what it
/// has to say if so.
///
/// An error for which `stops` is true refuses the write: the failure carries
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
    issues: &ValidationIssues,
    stops: impl Fn(&ValidationIssue) -> bool,
) -> Option<OperationFailure> {
    if issues.errors().into_iter().any(stops) {
        return Some(OperationFailure::InvalidSpec {
            action,
            package_name: package_name.to_string(),
            issues: issue_payload(issues),
        });
    }
    // Any issue at all is something to say: a notice alone still reaches the user.
    if !issues.all_issues().is_empty() {
        sender
            .send_validation_result(ValidationResultData {
                package_name: package_name.to_string(),
                environment: environment.to_string(),
                outcome: issues.outcome(),
                issues: issue_payload(issues),
            })
            .await;
    }
    None
}

/// The errors for `package`'s entries that deploy to a target another spec
/// deploys, found by collecting the specs as `--all` does.
///
/// Sends, as a run for one name does, the warnings collecting found and a warning
/// for each place it could not use. A spec with no dotfile entry in any
/// environment it declares gets neither, and no collection is done for it.
async fn collisions_with_other_specs<PR>(
    package: &Package,
    package_name: &str,
    repo: &PR,
    dotfiles_repo: &PR,
    config: &SelfieConfig,
    sender: &EventSender,
) -> Vec<ValidationIssue>
where
    PR: PackageRepository,
{
    // Such a spec cannot share a target, so nothing collection found is about it.
    if !package.has_dotfiles_in_any_environment() {
        return Vec::new();
    }
    let others = collect_others(repo, dotfiles_repo, config);

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
        std::slice::from_ref(package),
        config,
        Checking::Validate,
    )
    .await;

    cross_spec_issues(&others.packages, config.environment())
        .remove(package.path())
        .unwrap_or_default()
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
    issues.extend(
        collisions_with_other_specs(
            &package_blob.package,
            package_name,
            repo,
            dotfiles_repo,
            config,
            sender,
        )
        .await,
    );
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
mod tests {
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
}
