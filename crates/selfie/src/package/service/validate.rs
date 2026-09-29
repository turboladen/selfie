//!
//! Helps break down the pieces of running the `package validate` command.
//!

use crate::{
    config::SelfieConfig,
    package::{
        Package,
        event::{
            EventSender, OperationResult, OperationSuccess, Outcome, ValidationIssueData,
            ValidationLevel, ValidationResultData, ValidationStatus,
        },
        port::PackageRepository,
        service::ProgressTracker,
        validate::{unreadable_template_issue, validate_template_vars},
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
///
/// Shared with `validate_all`: one conversion, so a newly added level cannot be
/// wired into one command and forgotten in the other.
pub(super) fn issue_payload(issues: &ValidationIssues) -> Vec<ValidationIssueData> {
    let level_of = |issue: &ValidationIssue| match issue.level() {
        crate::validation::ValidationLevel::Error => ValidationLevel::Error,
        crate::validation::ValidationLevel::Warning => ValidationLevel::Warning,
        crate::validation::ValidationLevel::Info => ValidationLevel::Info,
    };

    issues
        .errors()
        .into_iter()
        .chain(issues.warnings())
        .chain(issues.infos())
        .map(|issue| ValidationIssueData {
            category: format!("{:?}", issue.category()),
            field: issue.field().to_string(),
            message: issue.message().to_string(),
            level: level_of(issue),
            suggestion: issue.suggestion().map(std::string::ToString::to_string),
            location: issue.location().map(str::to_string),
        })
        .collect()
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
            Err(_) => (Err(err), repo),
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

    let issues = &all_issues(&package_blob.package, source, config.environment());

    // Step 3: Process validation results
    progress.next(sender, "Processing validation results").await;

    // Convert validation issues to structured data
    let validation_issues = issue_payload(issues);

    // Determine overall validation status
    let status = match issues.outcome() {
        Outcome::Failed => ValidationStatus::HasErrors,
        Outcome::Found => ValidationStatus::HasWarnings,
        Outcome::Clean => ValidationStatus::Valid,
    };

    // Send structured validation result
    let validation_result = ValidationResultData {
        package_name: package_name.to_string(),
        environment: config.environment().to_string(),
        status: status.clone(),
        issues: validation_issues,
    };

    sender.send_validation_result(validation_result).await;

    // Return appropriate operation result
    match status {
        ValidationStatus::Valid => {
            sender
                .send_debug("Package definition is valid for the current environment")
                .await;

            OperationResult::Success(OperationSuccess::package_validated(
                package_name.to_string(),
                config.environment().to_string(),
                ValidationStatus::Valid,
                None,
                (progress.current_step(), progress.total_steps()).into(),
            ))
        }
        ValidationStatus::HasWarnings => {
            let warning_count = issues.warnings().len();
            OperationResult::Success(OperationSuccess::package_validated(
                package_name.to_string(),
                config.environment().to_string(),
                ValidationStatus::HasWarnings,
                Some(warning_count),
                (progress.current_step(), progress.total_steps()).into(),
            ))
        }
        ValidationStatus::HasErrors => {
            let error_count = issues.errors().len();
            let warning_count = issues.warnings().len();
            let error_msg = format!(
                "Package '{}' validation failed with {} error(s) and {} warning(s) (completed {}/{} steps)",
                package_name,
                error_count,
                warning_count,
                progress.current_step(),
                progress.total_steps()
            );
            OperationResult::Failure(error_msg.into())
        }
    }
}
