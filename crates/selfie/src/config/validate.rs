//! Configuration validation functionality
//!
//! This module provides validation capabilities for application configuration,
//! ensuring that configuration values are valid and complete before use.

use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::{
    fs::{AbsentReason, DirectoryState, FileSystem},
    validation::{ValidationErrorCategory, ValidationIssue, ValidationIssues},
};

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
    /// Validate every configuration field: environment name, package directory,
    /// the optional directories, and the command timeout.
    ///
    /// The [`ValidationResult`] separates errors, which stop the configuration
    /// being used, from warnings, which flag a potential problem. `fs` answers
    /// what is at each directory path.
    #[must_use]
    pub fn validate(&self, fs: &impl FileSystem) -> ValidationResult {
        let mut issues = Vec::new();

        if let Some(issue) = validate_environment(&self.environment) {
            issues.push(issue);
        }

        issues.extend(validate_package_directory(fs, &self.package_directory));

        if let Some(ref path) = self.dotfiles_directory {
            issues.extend(validate_optional_directory(fs, "dotfiles_directory", path));
        }

        if let Some(ref path) = self.state_directory {
            issues.extend(validate_optional_directory(fs, "state_directory", path));
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

impl super::diagnostics::LoadedConfig {
    /// Validate the settings **and** report anything in the file selfie ignored.
    ///
    /// Ignored keys are warnings rather than errors: the file still describes a
    /// usable configuration, but it is not valid. `fs` answers what is at each
    /// directory path.
    #[must_use]
    pub fn validate(&self, fs: &impl FileSystem) -> ValidationResult {
        let mut result = self.config().validate(fs);

        let mut issues = result.issues.all_issues().to_vec();
        issues.extend(self.ignored_keys().iter().map(|ignored| {
            // `Advisory`, not `InvalidValue`: an ignored key is not a bad value,
            // and every other category names a kind of mistake. The level stays
            // `warning` so a file carrying one is not reported as valid.
            ValidationIssue::warning(
                ValidationErrorCategory::Advisory,
                ignored.key(),
                &ignored.message(),
                Some(&ignored.suggestion()),
            )
        }));

        result.issues = issues.into();
        result
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

/// Validate the package directory path: it must be set, absolute after `~`
/// expansion, and a directory, as [`validate_directory_path`] reports.
fn validate_package_directory(
    fs: &impl FileSystem,
    package_directory: &Path,
) -> Vec<ValidationIssue> {
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

    validate_directory_path(fs, "package_directory", package_directory)
        .into_iter()
        .collect()
}

/// Validate an optional directory path: it must not be empty, and it is
/// checked as [`validate_directory_path`] checks a directory.
fn validate_optional_directory(
    fs: &impl FileSystem,
    field_name: &str,
    path: &Path,
) -> Vec<ValidationIssue> {
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

    validate_directory_path(fs, field_name, path)
        .into_iter()
        .collect()
}

/// Validate a directory path: `~` is expanded through `fs`, the result must be
/// absolute, and what `fs` finds at it is reported when that is not a directory.
fn validate_directory_path(
    fs: &impl FileSystem,
    field_name: &str,
    path: &Path,
) -> Option<ValidationIssue> {
    let expanded_path = match super::yaml::expand_tilde_only(fs, path) {
        Ok(expanded) => expanded.unwrap_or_else(|| path.to_path_buf()),
        // Nothing is wrong with the setting itself, so this is no error.
        Err(error) => {
            return Some(ValidationIssue::warning(
                ValidationErrorCategory::Advisory,
                field_name,
                &format!(
                    "The `{field_name}` path could not be checked: could not resolve ~: {error}"
                ),
                None,
            ));
        }
    };

    if !expanded_path.is_absolute() {
        return Some(ValidationIssue::error(
            ValidationErrorCategory::PathFormat,
            field_name,
            &format!("The `{field_name}` path is relative and cannot be resolved"),
            Some("Provide an absolute path or use ~ for the home directory"),
        ));
    }

    read_directory_issue(
        field_name,
        &expanded_path,
        fs.directory_state(&expanded_path),
    )
}

/// What to report about a directory setting whose state is `state`.
fn read_directory_issue(
    field_name: &str,
    path: &Path,
    state: DirectoryState,
) -> Option<ValidationIssue> {
    // Only a path something else occupies is an error, since nothing can be
    // created there. Every other state warns, as a command reading the directory
    // warns and carries on.
    match state {
        DirectoryState::Directory => None,
        DirectoryState::Absent(AbsentReason::Occupied { .. }) => Some(ValidationIssue::error(
            ValidationErrorCategory::PathFormat,
            field_name,
            &format!(
                "The `{field_name}` path {} {}",
                path.display(),
                state.clause()
            ),
            Some("Provide a path to a directory, not a file"),
        )),
        DirectoryState::Absent(AbsentReason::Empty) => Some(ValidationIssue::warning(
            ValidationErrorCategory::PathFormat,
            field_name,
            &format!(
                "The `{field_name}` path {} {}",
                path.display(),
                state.clause()
            ),
            Some(&format!(
                "Correct the setting if the path is a typo, or create it with: mkdir -p -- {}",
                crate::fs::shell_quote(path)
            )),
        )),
        DirectoryState::Absent(_) => Some(ValidationIssue::warning(
            ValidationErrorCategory::PathFormat,
            field_name,
            &format!(
                "The `{field_name}` path {} {}",
                path.display(),
                state.clause()
            ),
            Some("Update the path to name a directory"),
        )),
        // `Unlistable` needs a listing to discover, and this answer comes from a
        // stat, so it is grouped in to keep the match total.
        DirectoryState::Unlistable(_) | DirectoryState::Unknown(_) => {
            Some(ValidationIssue::warning(
                ValidationErrorCategory::Advisory,
                field_name,
                &format!(
                    "The `{field_name}` path {} {}",
                    path.display(),
                    state.clause()
                ),
                Some("Check the path and each directory above it"),
            ))
        }
    }
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

        let result = config.validate(&crate::fs::RealFileSystem);
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

        let result = config.validate(&crate::fs::RealFileSystem);
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

        let result = config.validate(&crate::fs::RealFileSystem);
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

        let result = config.validate(&crate::fs::RealFileSystem);
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

        let result = config.validate(&crate::fs::RealFileSystem);
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

        let result = config.validate(&crate::fs::RealFileSystem);
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

        let result = config.validate(&crate::fs::RealFileSystem);
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

        let result = config.validate(&crate::fs::RealFileSystem);
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

        let result = config.validate(&crate::fs::RealFileSystem);
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

        let result = config.validate(&crate::fs::RealFileSystem);
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

        let result = config.validate(&crate::fs::RealFileSystem);
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

        let result = config.validate(&crate::fs::RealFileSystem);
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

        let result = config.validate(&crate::fs::RealFileSystem);
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

        let result = config.validate(&crate::fs::RealFileSystem);
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

        let result = config.validate(&crate::fs::RealFileSystem);
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

        let result = config.validate(&crate::fs::RealFileSystem);
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

        let result = config.validate(&crate::fs::RealFileSystem);
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

        let result = config.validate(&crate::fs::RealFileSystem);
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

        let result = config.validate(&crate::fs::RealFileSystem);
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

        let result = config.validate(&crate::fs::RealFileSystem);
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

        let result = config.validate(&crate::fs::RealFileSystem);
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

        let result = config.validate(&crate::fs::RealFileSystem);
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

        let result = config.validate(&crate::fs::RealFileSystem);
        assert!(!result.issues().has_issues());
        assert!(result.issues().is_valid());
    }

    #[test]
    fn invalid_config_collects_all_issues() {
        let config = SelfieConfigBuilder::default()
            .environment("")
            .package_directory("")
            .build();

        let result = config.validate(&crate::fs::RealFileSystem);
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

        let result = config.validate(&crate::fs::RealFileSystem);
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

    // --- directory state, as the file system port reports it ---

    use crate::{
        fs::{AbsentReason, DirectoryState, MockFileSystem},
        validation::{ValidationIssue, ValidationLevel},
    };

    // The one issue `validate` reports for `field` when the port answers `state`
    // for every directory. The paths exist nowhere on disk, so a check that
    // asked the disk instead of the port would answer differently.
    fn directory_issue(field: &str, state: DirectoryState) -> Option<ValidationIssue> {
        let mut fs = MockFileSystem::default();
        fs.mock_directory_state(state);
        let config = SelfieConfigBuilder::default()
            .environment("linux")
            .package_directory("/nowhere/packages")
            .dotfiles_directory(PathBuf::from("/nowhere/dotfiles"))
            .state_directory(PathBuf::from("/nowhere/state"))
            .build();

        let result = config.validate(&fs);
        let mut issues: Vec<_> = result
            .issues()
            .all_issues()
            .iter()
            .filter(|i| i.field == field)
            .cloned()
            .collect();
        assert!(issues.len() <= 1, "{issues:?}");
        issues.pop()
    }

    fn suggestion(issue: &ValidationIssue) -> &str {
        issue.suggestion.as_deref().unwrap_or_default()
    }

    fn dangling() -> DirectoryState {
        DirectoryState::Absent(AbsentReason::DanglingSymlink {
            points_to: Some(PathBuf::from("/gone")),
        })
    }

    fn unknown() -> DirectoryState {
        DirectoryState::Unknown(std::sync::Arc::new(std::io::Error::from(
            std::io::ErrorKind::PermissionDenied,
        )))
    }

    #[test]
    fn a_directory_the_port_reports_is_not_an_issue() {
        assert_eq!(
            directory_issue("dotfiles_directory", DirectoryState::Directory),
            None
        );
        assert_eq!(
            directory_issue("state_directory", DirectoryState::Directory),
            None
        );
    }

    #[test]
    fn a_path_a_file_occupies_is_an_error() {
        let issue = directory_issue(
            "dotfiles_directory",
            DirectoryState::Absent(AbsentReason::Occupied {
                kind: "regular file",
            }),
        )
        .expect("an issue");

        assert_eq!(issue.level, ValidationLevel::Error);
        assert_eq!(issue.category, ValidationErrorCategory::PathFormat);
        assert!(issue.message.contains("it is a regular file"), "{issue:?}");
    }

    // A dotfiles directory that is not there leaves a command without standalone
    // specs, which it warns about and carries on. Only an empty path is offered
    // `mkdir -p`, which fails against a dangling link.
    #[test]
    fn a_dangling_symlink_is_a_warning_naming_it_without_mkdir() {
        let issue = directory_issue("dotfiles_directory", dangling()).expect("an issue");

        assert_eq!(issue.level, ValidationLevel::Warning);
        assert!(issue.message.contains("symlink to nothing"), "{issue:?}");
        assert!(!suggestion(&issue).contains("mkdir"), "{issue:?}");
    }

    #[test]
    fn an_empty_path_is_a_warning_offering_the_correction_then_mkdir() {
        let issue = directory_issue(
            "dotfiles_directory",
            DirectoryState::Absent(AbsentReason::Empty),
        )
        .expect("an issue");

        assert_eq!(issue.level, ValidationLevel::Warning);
        assert_eq!(issue.category, ValidationErrorCategory::PathFormat);
        assert!(issue.message.contains("does not exist"), "{issue:?}");
        assert!(
            suggestion(&issue).starts_with(
                "Correct the setting if the path is a typo, or create it with: mkdir -p --"
            ),
            "{issue:?}"
        );
    }

    #[test]
    fn a_path_that_could_not_be_checked_is_an_advisory_warning() {
        let issue = directory_issue("dotfiles_directory", unknown()).expect("an issue");

        assert_eq!(issue.level, ValidationLevel::Warning);
        assert_eq!(issue.category, ValidationErrorCategory::Advisory);
        assert!(issue.message.contains("could not be checked"), "{issue:?}");
        assert_eq!(
            suggestion(&issue),
            "Check the path and each directory above it"
        );
    }

    // `~` is expanded through the port, the way the loader expands it.
    #[test]
    fn a_leading_tilde_is_expanded_through_the_port() {
        let mut fs = MockFileSystem::default();
        fs.mock_expand_path("~", "/home/me");
        fs.expect_directory_state()
            .withf(|path| path == std::path::Path::new("/home/me/dotfiles"))
            .returning(|_| DirectoryState::Directory);
        fs.expect_directory_state()
            .returning(|_| DirectoryState::Directory);
        let config = SelfieConfigBuilder::default()
            .environment("linux")
            .package_directory("/nowhere/packages")
            .dotfiles_directory(PathBuf::from("~/dotfiles"))
            .build();

        let result = config.validate(&fs);

        assert!(
            result
                .issues()
                .all_issues()
                .iter()
                .all(|i| i.field != "dotfiles_directory"),
            "{:?}",
            result.issues()
        );
    }

    // Only `~` and `~/` are the home directory. `~user` and `~typo` are left as
    // written, and so are relative. The mock has no expansion, so asking it would
    // fail the test.
    #[test]
    fn a_tilde_naming_a_user_or_a_typo_is_relative() {
        for setting in ["~user/dotfiles", "~typo"] {
            let mut fs = MockFileSystem::default();
            fs.mock_directories_exist();
            let config = SelfieConfigBuilder::default()
                .environment("linux")
                .package_directory("/nowhere/packages")
                .dotfiles_directory(PathBuf::from(setting))
                .build();

            let result = config.validate(&fs);

            assert!(
                result
                    .issues()
                    .errors()
                    .iter()
                    .any(|i| i.field == "dotfiles_directory" && i.message.contains("relative")),
                "{setting}: {:?}",
                result.issues()
            );
        }
    }

    // A home directory selfie cannot resolve says nothing about the setting, so
    // it is reported as a warning rather than as a relative path.
    #[test]
    fn a_home_directory_that_cannot_be_resolved_is_a_warning() {
        let mut fs = MockFileSystem::default();
        fs.expect_expand_path()
            .returning(|_| Err(crate::fs::FileSystemError::HomeDirNotFound));
        fs.mock_directories_exist();
        let config = SelfieConfigBuilder::default()
            .environment("linux")
            .package_directory("/nowhere/packages")
            .dotfiles_directory(PathBuf::from("~/dotfiles"))
            .build();

        let result = config.validate(&fs);

        let issue = result
            .issues()
            .all_issues()
            .iter()
            .find(|i| i.field == "dotfiles_directory")
            .expect("an issue");
        assert_eq!(issue.level, ValidationLevel::Warning);
        assert!(issue.message.contains("could not resolve ~"), "{issue:?}");
    }
}
