//!
//! Helps break down the pieces of running the `package validate` command.
//!

use crate::{
    config::SelfieConfig,
    package::{
        Package,
        event::{
            EventSender, OperationResult, OperationSuccess, ValidationIssueData, ValidationLevel,
            ValidationResultData, ValidationStatus,
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

pub(super) async fn handle_validate<PR>(
    package_name: &str,
    repo: &PR,
    config: &SelfieConfig,
    sender: &EventSender,
    progress: &mut ProgressTracker,
) -> OperationResult
where
    PR: PackageRepository,
{
    // Step 1: Fetch package
    progress.next(sender, "Loading package definition").await;

    let package_blob = match repo.get_package(package_name) {
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

    let validation_result = package_blob.package.validate(config.environment());
    let mut all_issues = validation_result.issues().all_issues().to_vec();
    all_issues.extend(validate_package_templates(&package_blob.package, repo));
    let issues: ValidationIssues = all_issues.into();
    let issues = &issues;

    // Step 3: Process validation results
    progress.next(sender, "Processing validation results").await;

    // Convert validation issues to structured data
    let mut validation_issues = Vec::new();

    for error in issues.errors() {
        validation_issues.push(ValidationIssueData {
            category: format!("{:?}", error.category()),
            field: error.field().to_string(),
            message: error.message().to_string(),
            level: ValidationLevel::Error,
            suggestion: error.suggestion().map(std::string::ToString::to_string),
            location: error.location().map(str::to_string),
        });
    }

    for warning in issues.warnings() {
        validation_issues.push(ValidationIssueData {
            category: format!("{:?}", warning.category()),
            field: warning.field().to_string(),
            message: warning.message().to_string(),
            level: ValidationLevel::Warning,
            suggestion: warning.suggestion().map(std::string::ToString::to_string),
            location: warning.location().map(str::to_string),
        });
    }

    // Determine overall validation status
    let status = if issues.has_errors() {
        ValidationStatus::HasErrors
    } else if issues.has_warnings() {
        ValidationStatus::HasWarnings
    } else {
        ValidationStatus::Valid
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
