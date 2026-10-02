//!
//! Helps break down the pieces of running the `package validate` command.
//!

use crate::{
    config::SelfieConfig,
    package::{
        Package,
        event::{
            EventSender, OperationResult, OperationSuccess, Outcome, ValidationIssueData,
            ValidationResultData,
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
pub(super) fn issue_payload(issues: &ValidationIssues) -> Vec<ValidationIssueData> {
    // Ranked by an exhaustive match, so a level added later fails the build here
    // rather than dropping out of every command's payload. The sort is stable,
    // so issues of one level keep their order.
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
}
