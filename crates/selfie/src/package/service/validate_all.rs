//!
//! Handles the `spec validate --all` operation — validates all package definitions.
//!

use crate::{
    config::SelfieConfig,
    package::{
        event::{
            EventSender, OperationResult, OperationSuccess, ValidationIssueData, ValidationLevel,
            ValidationResultData, ValidationStatus,
        },
        port::PackageRepository,
        service::ProgressTracker,
    },
};

pub(super) async fn handle_validate_all<PR>(
    repo: &PR,
    config: &SelfieConfig,
    sender: &EventSender,
    progress: &mut ProgressTracker,
) -> OperationResult
where
    PR: PackageRepository,
{
    // Step 1: Load all packages
    progress.next(sender, "Loading package definitions").await;

    let list_output = match repo.list_packages() {
        Ok(output) => {
            sender.send_debug("Successfully loaded package list").await;
            output
        }
        Err(err) => {
            return OperationResult::Failure(err.into());
        }
    };

    let valid_packages: Vec<_> = list_output
        .valid_packages()
        .filter(|p| p.environments().contains_key(config.environment()))
        .collect();

    // Step 2: Validate each package
    progress.next(sender, "Validating packages").await;

    let mut has_errors = false;
    let mut total_warnings: usize = 0;

    for package in &valid_packages {
        let validation = package.validate(config.environment());
        let issues = validation.issues();

        let mut validation_issues = Vec::new();

        for error in issues.errors() {
            validation_issues.push(ValidationIssueData {
                category: format!("{:?}", error.category()),
                field: error.field().to_string(),
                message: error.message().to_string(),
                level: ValidationLevel::Error,
                suggestion: error.suggestion().map(std::string::ToString::to_string),
            });
        }

        for warning in issues.warnings() {
            validation_issues.push(ValidationIssueData {
                category: format!("{:?}", warning.category()),
                field: warning.field().to_string(),
                message: warning.message().to_string(),
                level: ValidationLevel::Warning,
                suggestion: warning.suggestion().map(std::string::ToString::to_string),
            });
        }

        let status = if issues.has_errors() {
            has_errors = true;
            ValidationStatus::HasErrors
        } else if issues.has_warnings() {
            total_warnings += issues.warnings().len();
            ValidationStatus::HasWarnings
        } else {
            ValidationStatus::Valid
        };

        let validation_result = ValidationResultData {
            package_name: package.name().to_string(),
            environment: config.environment().to_string(),
            status,
            issues: validation_issues,
        };

        sender.send_validation_result(validation_result).await;
    }

    if has_errors {
        OperationResult::Failure(
            format!(
                "One or more packages failed validation (completed {}/{} steps)",
                progress.current_step(),
                progress.total_steps()
            )
            .into(),
        )
    } else {
        let (status, warning_count) = if total_warnings > 0 {
            (ValidationStatus::HasWarnings, Some(total_warnings))
        } else {
            (ValidationStatus::Valid, None)
        };
        OperationResult::Success(OperationSuccess::package_validated(
            String::new(),
            config.environment().to_string(),
            status,
            warning_count,
            (progress.current_step(), progress.total_steps()).into(),
        ))
    }
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
                .version("1.0.0")
                .environment("macos", |b| {
                    b.install("brew install ripgrep")
                        .check_some("command -v rg")
                })
                .path(temp_dir.path().join("ripgrep.yml"))
                .build(),
            PackageBuilder::default()
                .name("node")
                .version("20.0.0")
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

        let result = handle_validate_all(&mock_repo, &config, &sender, &mut progress).await;

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
                .version("1.0.0")
                .environment("macos", |b| b.install("brew install ripgrep"))
                .path(temp_dir.path().join("ripgrep.yml"))
                .build(),
            PackageBuilder::default()
                .name("apt-tool")
                .version("1.0.0")
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

        let result = handle_validate_all(&mock_repo, &config, &sender, &mut progress).await;
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
