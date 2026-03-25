//! Configuration validation functionality
//!
//! This module provides validation capabilities for application configuration,
//! ensuring that configuration values are valid and complete before use.

use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::validation::{ValidationErrorCategory, ValidationIssue, ValidationIssues};

use super::SelfieConfig;

/// Maximum recommended command timeout in seconds before a warning is emitted.
const MAX_RECOMMENDED_TIMEOUT_SECS: u64 = 600;

/// Result of configuration validation
///
/// Contains the path to the configuration file that was validated
/// and any validation issues that were found during the process.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ValidationResult {
    /// The config file path that was validated
    pub(crate) config_file_path: Option<PathBuf>,

    /// List of validation issues found during validation
    pub(crate) issues: ValidationIssues,
}

impl ValidationResult {
    /// Get the path to the configuration file that was validated
    #[must_use]
    pub fn config_file_path(&self) -> Option<&PathBuf> {
        self.config_file_path.as_ref()
    }

    /// Get the validation issues found during validation
    #[must_use]
    pub fn issues(&self) -> &ValidationIssues {
        &self.issues
    }
}

impl SelfieConfig {
    /// Perform comprehensive validation of the application configuration
    ///
    /// Validates all configuration fields including environment name,
    /// package directory path, optional directories, and command timeout
    /// to ensure they are valid and usable.
    ///
    /// # Returns
    ///
    /// A [`ValidationResult`] containing any issues found during validation.
    /// The result includes both errors (which prevent the configuration from
    /// being used) and warnings (which indicate potential problems).
    #[must_use]
    pub fn validate(&self) -> ValidationResult {
        let mut issues = Vec::new();

        if let Some(issue) = validate_environment(&self.environment) {
            issues.push(issue);
        }

        issues.extend(validate_package_directory(&self.package_directory));

        if let Some(ref path) = self.dotfiles_directory {
            issues.extend(validate_optional_directory("dotfiles_directory", path));
        }

        if let Some(ref path) = self.state_directory {
            issues.extend(validate_optional_directory("state_directory", path));
        }

        if let Some(issue) = validate_command_timeout(self.command_timeout) {
            issues.push(issue);
        }

        ValidationResult {
            config_file_path: Some(self.package_directory().clone()),
            issues: issues.into(),
        }
    }
}

/// Errors that can occur during configuration validation
///
/// These errors represent specific validation failures that can be
/// programmatically handled or displayed to users.
#[derive(Error, Debug, PartialEq)]
pub enum ConfigValidationError {
    /// A required configuration field is empty or missing
    #[error("Empty field: {0}")]
    EmptyField(String),

    /// The package directory path is invalid or cannot be used
    #[error("Invalid package directory: {0}")]
    InvalidPackageDirectory(String),
}

/// Validate the environment field
///
/// Ensures the environment name is not empty, as it's required for
/// determining which package installation commands to use.
fn validate_environment(environment: &str) -> Option<ValidationIssue> {
    environment.is_empty().then(|| {
        ValidationIssue::error(
            ValidationErrorCategory::RequiredField,
            "environment",
            "The `environment` field exists, but has no value",
            Some("Set a value for `environment`. Ex. `environment: macos`"),
        )
    })
}

/// Validate the package directory path
///
/// Ensures the package directory path is not empty and can be resolved
/// to an absolute path. Also warns if the directory doesn't exist on disk.
fn validate_package_directory(package_directory: &Path) -> Vec<ValidationIssue> {
    if package_directory.as_os_str().is_empty() {
        return vec![ValidationIssue::error(
            ValidationErrorCategory::RequiredField,
            "package_directory",
            "The `package_directory` field exists, but has no value",
            Some(
                "Set a value for `package_directory`. Ex. `package_directory: ~/dev/selfie-packages`",
            ),
        )];
    }

    validate_directory_path("package_directory", package_directory)
        .into_iter()
        .collect()
}

/// Validate an optional directory path: check for empty value, check it's
/// absolute after tilde expansion, and warn if it doesn't exist on disk.
fn validate_optional_directory(field_name: &str, path: &Path) -> Vec<ValidationIssue> {
    if path.as_os_str().is_empty() {
        return vec![ValidationIssue::error(
            ValidationErrorCategory::RequiredField,
            field_name,
            &format!("The `{field_name}` field exists, but has no value"),
            Some(&format!(
                "Set a value for `{field_name}` or remove the field to use the default"
            )),
        )];
    }

    validate_directory_path(field_name, path)
        .into_iter()
        .collect()
}

/// Validate a directory path: check it's absolute after tilde expansion,
/// check it exists and is a directory (not a file).
fn validate_directory_path(field_name: &str, path: &Path) -> Option<ValidationIssue> {
    let path_str = path.to_string_lossy();
    let expanded = shellexpand::tilde(&path_str);
    let expanded_path = Path::new(expanded.as_ref());

    if !expanded_path.is_absolute() {
        return Some(ValidationIssue::error(
            ValidationErrorCategory::PathFormat,
            field_name,
            &format!("The `{field_name}` path is relative and cannot be resolved"),
            Some("Provide an absolute path or use ~ for the home directory"),
        ));
    }

    if expanded_path.exists() && !expanded_path.is_dir() {
        return Some(ValidationIssue::error(
            ValidationErrorCategory::PathFormat,
            field_name,
            &format!(
                "The path for `{field_name}` is not a directory: {}",
                expanded_path.display()
            ),
            Some("Provide a path to a directory, not a file"),
        ));
    }

    if !expanded_path.exists() {
        return Some(ValidationIssue::warning(
            ValidationErrorCategory::PathFormat,
            field_name,
            &format!(
                "The directory at `{field_name}` does not exist: {}",
                expanded_path.display()
            ),
            Some("Create the directory or update the path"),
        ));
    }

    None
}

/// Validate the command timeout value
///
/// Warns if the timeout exceeds the recommended maximum.
fn validate_command_timeout(timeout: NonZeroU64) -> Option<ValidationIssue> {
    (timeout.get() > MAX_RECOMMENDED_TIMEOUT_SECS).then(|| {
        ValidationIssue::warning(
            ValidationErrorCategory::InvalidValue,
            "command_timeout",
            &format!(
                "Command timeout is {} seconds, which exceeds the recommended maximum of {MAX_RECOMMENDED_TIMEOUT_SECS} seconds",
                timeout.get()
            ),
            Some(&format!(
                "Consider lowering `command_timeout` to {MAX_RECOMMENDED_TIMEOUT_SECS} or less"
            )),
        )
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::config::SelfieConfigBuilder;
    use crate::validation::ValidationErrorCategory;

    use super::ConfigValidationError;

    // --- validate_environment tests ---

    #[test]
    fn valid_environment_passes() {
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory("/tmp")
            .build();

        let result = config.validate();
        let env_issues: Vec<_> = result
            .issues()
            .all_issues()
            .iter()
            .filter(|i| i.field == "environment")
            .collect();

        assert!(env_issues.is_empty());
    }

    #[test]
    fn empty_environment_produces_error() {
        let config = SelfieConfigBuilder::default()
            .environment("")
            .package_directory("/tmp")
            .build();

        let result = config.validate();
        let env_issues: Vec<_> = result
            .issues()
            .all_issues()
            .iter()
            .filter(|i| i.field == "environment")
            .collect();

        assert_eq!(env_issues.len(), 1);
        assert_eq!(
            env_issues[0].category,
            ValidationErrorCategory::RequiredField
        );
    }

    // --- validate_package_directory tests ---

    #[test]
    fn valid_absolute_directory_passes() {
        let config = SelfieConfigBuilder::default()
            .environment("linux")
            .package_directory("/tmp")
            .build();

        let result = config.validate();
        let dir_issues: Vec<_> = result
            .issues()
            .all_issues()
            .iter()
            .filter(|i| i.field == "package_directory")
            .collect();

        assert!(dir_issues.is_empty());
    }

    #[test]
    fn empty_package_directory_produces_error() {
        let config = SelfieConfigBuilder::default()
            .environment("linux")
            .package_directory("")
            .build();

        let result = config.validate();
        let dir_issues: Vec<_> = result
            .issues()
            .all_issues()
            .iter()
            .filter(|i| i.field == "package_directory")
            .collect();

        // Empty path produces RequiredField error (and also PathFormat since "" is not absolute)
        let required: Vec<_> = dir_issues
            .iter()
            .filter(|i| i.category == ValidationErrorCategory::RequiredField)
            .collect();
        assert_eq!(required.len(), 1);
    }

    #[test]
    fn relative_package_directory_produces_error() {
        let config = SelfieConfigBuilder::default()
            .environment("linux")
            .package_directory("packages")
            .build();

        let result = config.validate();
        let path_issues: Vec<_> = result
            .issues()
            .all_issues()
            .iter()
            .filter(|i| {
                i.field == "package_directory" && i.category == ValidationErrorCategory::PathFormat
            })
            .collect();

        assert_eq!(path_issues.len(), 1);
        assert!(path_issues[0].message.contains("relative"));
    }

    #[test]
    fn tilde_package_directory_passes() {
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory("~/packages")
            .build();

        let result = config.validate();
        let dir_errors: Vec<_> = result
            .issues()
            .errors()
            .into_iter()
            .filter(|i| i.field == "package_directory")
            .collect();

        assert!(dir_errors.is_empty());
    }

    #[test]
    fn nonexistent_package_directory_produces_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let nonexistent = tmp.path().join("does-not-exist");

        let config = SelfieConfigBuilder::default()
            .environment("linux")
            .package_directory(&nonexistent)
            .build();

        let result = config.validate();
        let warnings: Vec<_> = result
            .issues()
            .warnings()
            .into_iter()
            .filter(|i| i.field == "package_directory")
            .collect();

        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].category, ValidationErrorCategory::PathFormat);
        assert!(warnings[0].message.contains("does not exist"));
    }

    #[test]
    fn file_path_as_package_directory_produces_error() {
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("not-a-dir.txt");
        std::fs::write(&file_path, "").unwrap();

        let config = SelfieConfigBuilder::default()
            .environment("linux")
            .package_directory(&file_path)
            .build();

        let result = config.validate();
        let errors: Vec<_> = result
            .issues()
            .errors()
            .into_iter()
            .filter(|i| i.field == "package_directory")
            .collect();

        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].category, ValidationErrorCategory::PathFormat);
        assert!(errors[0].message.contains("not a directory"));
    }

    // --- validate_dotfiles_directory tests ---

    #[test]
    fn dotfiles_directory_none_produces_no_issues() {
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory("/tmp")
            .build();

        let result = config.validate();
        let issues: Vec<_> = result
            .issues()
            .all_issues()
            .iter()
            .filter(|i| i.field == "dotfiles_directory")
            .collect();

        assert!(issues.is_empty());
    }

    #[test]
    fn dotfiles_directory_relative_produces_error() {
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory("/tmp")
            .dotfiles_directory(PathBuf::from("relative/dotfiles"))
            .build();

        let result = config.validate();
        let errors: Vec<_> = result
            .issues()
            .errors()
            .into_iter()
            .filter(|i| i.field == "dotfiles_directory")
            .collect();

        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].category, ValidationErrorCategory::PathFormat);
    }

    #[test]
    fn dotfiles_directory_absolute_existing_produces_no_issues() {
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory("/tmp")
            .dotfiles_directory(PathBuf::from("/tmp"))
            .build();

        let result = config.validate();
        let issues: Vec<_> = result
            .issues()
            .all_issues()
            .iter()
            .filter(|i| i.field == "dotfiles_directory")
            .collect();

        assert!(issues.is_empty());
    }

    #[test]
    fn dotfiles_directory_absolute_nonexistent_produces_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let nonexistent = tmp.path().join("does-not-exist");

        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory("/tmp")
            .dotfiles_directory(nonexistent)
            .build();

        let result = config.validate();
        let warnings: Vec<_> = result
            .issues()
            .warnings()
            .into_iter()
            .filter(|i| i.field == "dotfiles_directory")
            .collect();

        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].category, ValidationErrorCategory::PathFormat);
        assert!(warnings[0].message.contains("does not exist"));
    }

    #[test]
    fn dotfiles_directory_empty_produces_error() {
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory("/tmp")
            .dotfiles_directory(PathBuf::from(""))
            .build();

        let result = config.validate();
        let errors: Vec<_> = result
            .issues()
            .errors()
            .into_iter()
            .filter(|i| i.field == "dotfiles_directory")
            .collect();

        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].category, ValidationErrorCategory::RequiredField);
    }

    #[test]
    fn dotfiles_directory_tilde_produces_no_error() {
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory("/tmp")
            .dotfiles_directory(PathBuf::from("~/dotfiles"))
            .build();

        let result = config.validate();
        let errors: Vec<_> = result
            .issues()
            .errors()
            .into_iter()
            .filter(|i| i.field == "dotfiles_directory")
            .collect();

        assert!(errors.is_empty());
    }

    // --- validate_state_directory tests ---

    #[test]
    fn state_directory_none_produces_no_issues() {
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory("/tmp")
            .build();

        let result = config.validate();
        let issues: Vec<_> = result
            .issues()
            .all_issues()
            .iter()
            .filter(|i| i.field == "state_directory")
            .collect();

        assert!(issues.is_empty());
    }

    #[test]
    fn state_directory_relative_produces_error() {
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory("/tmp")
            .state_directory(PathBuf::from("relative/state"))
            .build();

        let result = config.validate();
        let errors: Vec<_> = result
            .issues()
            .errors()
            .into_iter()
            .filter(|i| i.field == "state_directory")
            .collect();

        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].category, ValidationErrorCategory::PathFormat);
    }

    #[test]
    fn state_directory_absolute_existing_produces_no_issues() {
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory("/tmp")
            .state_directory(PathBuf::from("/tmp"))
            .build();

        let result = config.validate();
        let issues: Vec<_> = result
            .issues()
            .all_issues()
            .iter()
            .filter(|i| i.field == "state_directory")
            .collect();

        assert!(issues.is_empty());
    }

    #[test]
    fn state_directory_absolute_nonexistent_produces_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let nonexistent = tmp.path().join("does-not-exist");

        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory("/tmp")
            .state_directory(nonexistent)
            .build();

        let result = config.validate();
        let warnings: Vec<_> = result
            .issues()
            .warnings()
            .into_iter()
            .filter(|i| i.field == "state_directory")
            .collect();

        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].category, ValidationErrorCategory::PathFormat);
        assert!(warnings[0].message.contains("does not exist"));
    }

    #[test]
    fn state_directory_empty_produces_error() {
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory("/tmp")
            .state_directory(PathBuf::from(""))
            .build();

        let result = config.validate();
        let errors: Vec<_> = result
            .issues()
            .errors()
            .into_iter()
            .filter(|i| i.field == "state_directory")
            .collect();

        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].category, ValidationErrorCategory::RequiredField);
    }

    // --- validate_command_timeout tests ---

    #[test]
    fn default_command_timeout_produces_no_issues() {
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory("/tmp")
            .build();

        let result = config.validate();
        let issues: Vec<_> = result
            .issues()
            .all_issues()
            .iter()
            .filter(|i| i.field == "command_timeout")
            .collect();

        assert!(issues.is_empty());
    }

    #[test]
    fn command_timeout_at_600_produces_no_warning() {
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory("/tmp")
            .command_timeout_unchecked(600)
            .build();

        let result = config.validate();
        let issues: Vec<_> = result
            .issues()
            .all_issues()
            .iter()
            .filter(|i| i.field == "command_timeout")
            .collect();

        assert!(issues.is_empty());
    }

    #[test]
    fn command_timeout_over_600_produces_warning() {
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory("/tmp")
            .command_timeout_unchecked(601)
            .build();

        let result = config.validate();
        let warnings: Vec<_> = result
            .issues()
            .warnings()
            .into_iter()
            .filter(|i| i.field == "command_timeout")
            .collect();

        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].category, ValidationErrorCategory::InvalidValue);
    }

    // --- SelfieConfig::validate() integration tests ---

    #[test]
    fn valid_config_has_no_issues() {
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory("/tmp")
            .build();

        let result = config.validate();
        assert!(!result.issues().has_issues());
        assert!(result.issues().is_valid());
    }

    #[test]
    fn invalid_config_collects_all_issues() {
        let config = SelfieConfigBuilder::default()
            .environment("")
            .package_directory("")
            .build();

        let result = config.validate();
        // At minimum: empty environment + empty package_directory + non-absolute path
        assert!(result.issues().has_errors());
        assert!(result.issues().all_issues().len() >= 2);

        let categories: Vec<_> = result
            .issues()
            .all_issues()
            .iter()
            .map(|i| i.category)
            .collect();
        assert!(categories.contains(&ValidationErrorCategory::RequiredField));
    }

    #[test]
    fn validation_result_accessors_work() {
        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory("/tmp")
            .build();

        let result = config.validate();
        assert!(result.config_file_path().is_some());
        assert!(!result.issues().has_issues());
    }

    // --- ConfigValidationError display tests ---

    #[test]
    fn error_display_formatting() {
        let empty = ConfigValidationError::EmptyField("environment".to_string());
        assert_eq!(empty.to_string(), "Empty field: environment");

        let invalid = ConfigValidationError::InvalidPackageDirectory("not absolute".to_string());
        assert_eq!(
            invalid.to_string(),
            "Invalid package directory: not absolute"
        );
    }
}
