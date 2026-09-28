//! Configuration validation functionality
//!
//! This module provides validation capabilities for application configuration,
//! ensuring that configuration values are valid and complete before use.

use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::{
    dotfile_service::directory::{UnlistedDotfilesDirectory, absent_warning},
    fs::{AbsentReason, DirectoryState, FileSystem},
    package::repository::yaml::list_spec_directory,
    validation::{ValidationErrorCategory, ValidationIssue, ValidationIssues},
};

use super::ConfigFile;

/// Maximum recommended command timeout in seconds before a warning is emitted.
const MAX_RECOMMENDED_TIMEOUT_SECS: u64 = 600;

/// Holds the issues validating a configuration found.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ValidationResult {
    /// List of validation issues found during validation
    pub(crate) issues: ValidationIssues,
}

impl ValidationResult {
    /// Get the validation issues found during validation
    #[must_use]
    pub fn issues(&self) -> &ValidationIssues {
        &self.issues
    }
}

impl ConfigFile {
    /// Validate every setting the file holds: environment name, package
    /// directory, the optional directories, and the command timeout. A required
    /// setting the file leaves out is an error, since a run has nothing to fill
    /// it from unless a flag supplies it.
    ///
    /// The [`ValidationResult`] separates errors, which stop the configuration
    /// being used, from warnings, which flag a potential problem. `fs` answers
    /// what is at each directory path.
    #[must_use]
    pub fn validate(&self, fs: &impl FileSystem) -> ValidationResult {
        let mut issues = Vec::new();

        if let Some(issue) = validate_environment(self.environment.as_deref()) {
            issues.push(issue);
        }

        issues.extend(validate_package_directory(
            fs,
            self.package_directory.as_deref(),
        ));

        // An unset directory setting is checked at the default the commands use,
        // since they read it there.
        match super::setting_path(self.dotfiles_directory.as_deref()) {
            Some(path) => issues.extend(
                validate_directory_path(fs, "dotfiles_directory", path, Unresolved::Tolerated)
                    .map_or_else(Some, |path| dotfiles_directory_issue(fs, &path, true)),
            ),
            None => {
                if let Some(default) = self.default_dotfiles_directory_for_validation(fs)
                    && default.is_absolute()
                {
                    issues.extend(dotfiles_directory_issue(fs, &default, false));
                }
            }
        }

        match super::setting_path(self.state_directory.as_deref()) {
            Some(path) => issues.extend(
                validate_directory_path(fs, "state_directory", path, Unresolved::Refused)
                    .map_or_else(Some, |path| {
                        state_directory_issue("state_directory", &path, fs.directory_state(&path))
                    }),
            ),
            None => issues.extend(default_state_directory_issue(fs)),
        }

        if let Some(issue) = validate_command_timeout(self.command_timeout) {
            issues.push(issue);
        }

        ValidationResult {
            issues: issues.into(),
        }
    }

    // The default beside the package directory. `None` when the package
    // directory is unset or its `~` cannot be resolved, which is reported on its
    // own.
    fn default_dotfiles_directory_for_validation(&self, fs: &impl FileSystem) -> Option<PathBuf> {
        let package_directory = super::setting_path(self.package_directory.as_deref())?;
        let expanded = super::expand_setting(fs, package_directory).ok()?;
        Some(super::default_dotfiles_directory(&expanded))
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
/// Ensures the environment name is set and not empty, as it's required for
/// determining which package installation commands to use.
fn validate_environment(environment: Option<&str>) -> Option<ValidationIssue> {
    match environment {
        None => Some(missing_setting_issue("environment", "`environment: macos`")),
        Some(value) if value.trim().is_empty() => Some(ValidationIssue::error(
            ValidationErrorCategory::RequiredField,
            "environment",
            "The `environment` field exists, but has no value",
            Some("Set a value for `environment`. Ex. `environment: macos`"),
        )),
        Some(_) => None,
    }
}

/// The error for a required setting the file leaves out, where `example` is a
/// line that would set it.
fn missing_setting_issue(field_name: &str, example: &str) -> ValidationIssue {
    ValidationIssue::error(
        ValidationErrorCategory::RequiredField,
        field_name,
        &format!("The `{field_name}` setting is missing"),
        Some(&format!("Add `{field_name}` to the file. Ex. {example}")),
    )
}

/// Validate the package directory path: it must be set, absolute after `~`
/// expansion, and a directory, as [`validate_directory_path`] reports.
fn validate_package_directory(
    fs: &impl FileSystem,
    package_directory: Option<&Path>,
) -> Vec<ValidationIssue> {
    let Some(package_directory) = package_directory else {
        return vec![missing_setting_issue(
            "package_directory",
            "`package_directory: ~/dev/selfie-packages`",
        )];
    };
    if super::setting_path(Some(package_directory)).is_none() {
        return vec![ValidationIssue::error(
            ValidationErrorCategory::RequiredField,
            "package_directory",
            "The `package_directory` field exists, but has no value",
            Some(
                "Set a value for `package_directory`. Ex. `package_directory: ~/dev/selfie-packages`",
            ),
        )];
    }

    validate_directory_path(
        fs,
        "package_directory",
        package_directory,
        Unresolved::Refused,
    )
    .map_or_else(Some, |path| package_directory_issue(fs, &path))
    .into_iter()
    .collect()
}

/// What a run does with a directory setting whose `~` it cannot resolve: it
/// keeps the path as written, which is relative.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Unresolved {
    /// The reading commands refuse it.
    Refused,
    /// The reading commands carry on without what it holds.
    Tolerated,
}

/// Validate a directory path: `~` is expanded through `fs`, and the result must
/// be absolute. `unresolved` grades a `~` that cannot be resolved.
///
/// `Ok` carries the expanded path, for the caller to check what is there.
fn validate_directory_path(
    fs: &impl FileSystem,
    field_name: &str,
    path: &Path,
    unresolved: Unresolved,
) -> Result<PathBuf, ValidationIssue> {
    // The same expansion a run applies, which also says when the home directory
    // could not be found, where a run would keep the path as written.
    let expanded_path = match super::expand_setting(fs, path) {
        Ok(expanded) => expanded,
        Err(error) => {
            let message = format!(
                "The `{field_name}` path could not be checked: could not resolve ~: {error}"
            );
            return Err(if unresolved == Unresolved::Tolerated {
                ValidationIssue::warning(
                    ValidationErrorCategory::Advisory,
                    field_name,
                    &message,
                    None,
                )
            } else {
                ValidationIssue::error(
                    ValidationErrorCategory::PathFormat,
                    field_name,
                    &message,
                    None,
                )
            });
        }
    };

    if !expanded_path.is_absolute() {
        return Err(ValidationIssue::error(
            ValidationErrorCategory::PathFormat,
            field_name,
            &format!("The `{field_name}` path is relative and cannot be resolved"),
            Some("Provide an absolute path or use ~ for the home directory"),
        ));
    }

    Ok(expanded_path)
}

/// What to report about the package directory at `path`: an error whenever it
/// cannot be listed, since every command reading it fails.
fn package_directory_issue(fs: &impl FileSystem, path: &Path) -> Option<ValidationIssue> {
    // Listed the way the repository lists it, so the verdict is the one a
    // command reaches, `Unlistable` included. `spec create` alone carries on over
    // a missing directory, creating it, which is why the remedy is still offered.
    let error = list_spec_directory(fs, path).err()?;
    Some(ValidationIssue::error(
        ValidationErrorCategory::PathFormat,
        "package_directory",
        &format!("The `package_directory` path {error}"),
        Some(&directory_suggestion(path, error.state())),
    ))
}

/// What to report about the standalone dotfiles directory at `path`, where
/// `configured` says whether the user named it: what the reading commands do
/// with it, as they decide it.
fn dotfiles_directory_issue(
    fs: &impl FileSystem,
    path: &Path,
    configured: bool,
) -> Option<ValidationIssue> {
    let error = list_spec_directory(fs, path).err()?;
    let check_it = directory_suggestion(path, error.state());
    let unlisted = UnlistedDotfilesDirectory::classify(error, configured);
    let refuses = unlisted.refuses_collection();
    let (message, suggestion) = match unlisted {
        UnlistedDotfilesDirectory::OrdinarilyAbsent => return None,
        // The commands' own sentence already ends with the remedy, so only the
        // correction of a setting the user typed is suggested beside it.
        UnlistedDotfilesDirectory::Absent { path, reason } => (
            absent_warning(&path, &reason),
            configured.then(|| "Correct the setting if the path is a typo".to_string()),
        ),
        UnlistedDotfilesDirectory::Unlistable(error)
        | UnlistedDotfilesDirectory::Unknown(error) => (
            format!("The `dotfiles_directory` path {error}"),
            Some(check_it),
        ),
    };
    Some(if refuses {
        ValidationIssue::error(
            ValidationErrorCategory::PathFormat,
            "dotfiles_directory",
            &message,
            suggestion.as_deref(),
        )
    } else {
        ValidationIssue::warning(
            ValidationErrorCategory::PathFormat,
            "dotfiles_directory",
            &message,
            suggestion.as_deref(),
        )
    })
}

/// What to do about a spec directory at `path` whose state is `state`.
fn directory_suggestion(path: &Path, state: &DirectoryState) -> String {
    match state {
        DirectoryState::Absent(AbsentReason::Empty) => format!(
            "Correct the setting if the path is a typo, or create it with: mkdir -p -- {}",
            crate::fs::shell_quote(path)
        ),
        DirectoryState::Absent(AbsentReason::Occupied { .. }) => {
            "Provide a path to a directory, not a file".to_string()
        }
        DirectoryState::Absent(_) => "Update the path to name a directory".to_string(),
        DirectoryState::Directory | DirectoryState::Unlistable(_) | DirectoryState::Unknown(_) => {
            "Check the path and each directory above it".to_string()
        }
    }
}

/// What to report about the state directory, whose state is `state`: the
/// verdict a run reaches over the same path, in the run's own words.
fn state_directory_issue(
    field_name: &str,
    path: &Path,
    state: DirectoryState,
) -> Option<ValidationIssue> {
    use crate::dotfile_service::state_file::{
        StateDirectoryVerdict, not_there_yet_warning, state_directory_verdict,
    };

    match state_directory_verdict(path, state) {
        StateDirectoryVerdict::InUse => None,
        StateDirectoryVerdict::NotThereYet => Some(ValidationIssue::warning(
            ValidationErrorCategory::Advisory,
            field_name,
            &not_there_yet_warning(path),
            None,
        )),
        // An error, because a run refuses it.
        StateDirectoryVerdict::Refused(failure) => Some(ValidationIssue::error(
            ValidationErrorCategory::PathFormat,
            field_name,
            &failure.to_string(),
            None,
        )),
    }
}

/// What to report about the default state directory an unset
/// `state_directory` takes.
fn default_state_directory_issue(fs: &impl FileSystem) -> Option<ValidationIssue> {
    use crate::dotfile_service::state_file::{StateDirectoryVerdict, state_directory_verdict};

    let directory = match crate::fs::target::state_directory(fs, None) {
        Ok(directory) => directory,
        Err(error) => {
            return Some(ValidationIssue::warning(
                ValidationErrorCategory::Advisory,
                "state_directory",
                &format!("The default `state_directory` could not be checked: {error}"),
                None,
            ));
        }
    };
    // The run's own verdict. A default that is not there yet is its ordinary
    // first run, and the typo it may be applies only to a path the user typed.
    match state_directory_verdict(&directory, fs.directory_state(&directory)) {
        StateDirectoryVerdict::InUse | StateDirectoryVerdict::NotThereYet => None,
        StateDirectoryVerdict::Refused(failure) => Some(ValidationIssue::error(
            ValidationErrorCategory::PathFormat,
            "state_directory",
            &failure.to_string(),
            None,
        )),
    }
}

/// Validate the command timeout value
///
/// Warns if the timeout exceeds the recommended maximum.
fn validate_command_timeout(timeout: Option<NonZeroU64>) -> Option<ValidationIssue> {
    let timeout = timeout?;
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

    use crate::config::{ConfigFile, SelfieConfig, SelfieConfigBuilder};
    use crate::validation::ValidationErrorCategory;

    // Every directory the port calls a directory lists, empty.
    fn listable(fs: &mut crate::fs::MockFileSystem) {
        fs.expect_list_directory().returning(|_| Ok(Vec::new()));
    }

    // The file that would name exactly what `config` holds, so the fixtures below
    // can keep using the builder.
    fn as_file(config: &SelfieConfig) -> ConfigFile {
        ConfigFile {
            environment: Some(config.environment.clone()),
            package_directory: Some(config.package_directory.clone()),
            dotfiles_directory: config.dotfiles_directory.clone(),
            state_directory: config.state_directory.clone(),
            command_timeout: Some(config.command_timeout),
            stop_on_error: Some(config.stop_on_error),
            max_concurrency: Some(config.max_concurrency),
        }
    }

    use super::ConfigValidationError;

    // --- validate_environment tests ---

    #[test]
    fn valid_environment_passes() {
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory("/tmp")
            .build();

        let result = as_file(&config).validate(&crate::fs::RealFileSystem);
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

        let result = as_file(&config).validate(&crate::fs::RealFileSystem);
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

    // A file may leave a required setting to a flag, but `config validate`
    // reports the file, so the gap is an error there.
    #[test]
    fn a_missing_environment_is_an_error() {
        let file = ConfigFile {
            package_directory: Some(PathBuf::from("/tmp")),
            ..ConfigFile::default()
        };

        let result = file.validate(&crate::fs::RealFileSystem);
        let env_issues: Vec<_> = result
            .issues()
            .errors()
            .into_iter()
            .filter(|i| i.field == "environment")
            .collect();

        assert_eq!(env_issues.len(), 1, "{:?}", result.issues());
        assert_eq!(
            env_issues[0].category,
            ValidationErrorCategory::RequiredField
        );
        assert!(
            env_issues[0].message.contains("is missing"),
            "{:?}",
            env_issues[0]
        );
    }

    // --- validate_package_directory tests ---

    #[test]
    fn valid_absolute_directory_passes() {
        let config = SelfieConfigBuilder::default()
            .environment("linux")
            .package_directory("/tmp")
            .build();

        let result = as_file(&config).validate(&crate::fs::RealFileSystem);
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

        let result = as_file(&config).validate(&crate::fs::RealFileSystem);
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

    // Whitespace names no directory, so it is as empty as nothing, not relative.
    #[test]
    fn whitespace_package_directory_produces_error() {
        let config = SelfieConfigBuilder::default()
            .environment("linux")
            .package_directory("  ")
            .build();

        let result = as_file(&config).validate(&crate::fs::RealFileSystem);
        let dir_issues: Vec<_> = result
            .issues()
            .all_issues()
            .iter()
            .filter(|i| i.field == "package_directory")
            .cloned()
            .collect();

        assert_eq!(dir_issues.len(), 1, "{dir_issues:?}");
        assert_eq!(
            dir_issues[0].category,
            ValidationErrorCategory::RequiredField
        );
    }

    #[test]
    fn relative_package_directory_produces_error() {
        let config = SelfieConfigBuilder::default()
            .environment("linux")
            .package_directory("packages")
            .build();

        let result = as_file(&config).validate(&crate::fs::RealFileSystem);
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
        let mut fs = crate::fs::MockFileSystem::default();
        fs.expect_expand_path()
            .returning(|_| Ok(PathBuf::from("/home/me")));
        fs.expect_directory_state()
            .withf(|path| path == std::path::Path::new("/home/me/packages"))
            .returning(|_| crate::fs::DirectoryState::Directory);
        fs.expect_directory_state()
            .returning(|_| crate::fs::DirectoryState::Absent(crate::fs::AbsentReason::Empty));
        listable(&mut fs);

        let result = as_file(&config).validate(&fs);
        let dir_errors: Vec<_> = result
            .issues()
            .errors()
            .into_iter()
            .filter(|i| i.field == "package_directory")
            .collect();

        assert!(dir_errors.is_empty());
    }

    // Every command that reads the package directory fails without it, so a
    // missing one is an error, with the remedy offered.
    #[test]
    fn nonexistent_package_directory_produces_error() {
        let tmp = tempfile::tempdir().unwrap();
        let nonexistent = tmp.path().join("does-not-exist");

        let config = SelfieConfigBuilder::default()
            .environment("linux")
            .package_directory(&nonexistent)
            .build();

        let result = as_file(&config).validate(&crate::fs::RealFileSystem);
        let errors: Vec<_> = result
            .issues()
            .errors()
            .into_iter()
            .filter(|i| i.field == "package_directory")
            .collect();

        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].category, ValidationErrorCategory::PathFormat);
        assert!(errors[0].message.contains("does not exist"));
        assert!(
            errors[0]
                .suggestion
                .as_deref()
                .is_some_and(|s| s.contains("mkdir -p --")),
            "{:?}",
            errors[0]
        );
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

        let result = as_file(&config).validate(&crate::fs::RealFileSystem);
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

        let result = as_file(&config).validate(&crate::fs::RealFileSystem);
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

        let result = as_file(&config).validate(&crate::fs::RealFileSystem);
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

        let result = as_file(&config).validate(&crate::fs::RealFileSystem);
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

        let result = as_file(&config).validate(&crate::fs::RealFileSystem);
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

    // An empty value is unset, as it is to a run, so the default is checked.
    #[test]
    fn dotfiles_directory_empty_takes_the_default() {
        // A file at the default, so checking it is visible as a row.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("packages")).unwrap();
        std::fs::write(dir.path().join("dotfiles"), "not a directory").unwrap();
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory(dir.path().join("packages"))
            .dotfiles_directory(PathBuf::from(""))
            .state_directory(dir.path().join("state"))
            .build();

        let result = as_file(&config).validate(&crate::fs::RealFileSystem);
        let issues: Vec<_> = result
            .issues()
            .all_issues()
            .iter()
            .filter(|i| i.field == "dotfiles_directory")
            .cloned()
            .collect();

        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_ne!(issues[0].category, ValidationErrorCategory::RequiredField);
        assert!(
            issues[0]
                .message
                .contains(&dir.path().join("dotfiles").display().to_string()),
            "{issues:?}"
        );
    }

    #[test]
    fn dotfiles_directory_tilde_produces_no_error() {
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory("/tmp")
            .dotfiles_directory(PathBuf::from("~/dotfiles"))
            .build();

        let result = as_file(&config).validate(&crate::fs::RealFileSystem);
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

        let result = as_file(&config).validate(&crate::fs::RealFileSystem);
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

        let result = as_file(&config).validate(&crate::fs::RealFileSystem);
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

        let result = as_file(&config).validate(&crate::fs::RealFileSystem);
        let issues: Vec<_> = result
            .issues()
            .all_issues()
            .iter()
            .filter(|i| i.field == "state_directory")
            .collect();

        assert!(issues.is_empty());
    }

    // A missing state directory on the real file system warns without offering
    // `mkdir`, since selfie creates it on first use.
    #[test]
    fn state_directory_absolute_nonexistent_warns_that_it_is_not_there_yet() {
        let tmp = tempfile::tempdir().unwrap();
        let nonexistent = tmp.path().join("does-not-exist");

        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory("/tmp")
            .state_directory(nonexistent)
            .build();

        let result = as_file(&config).validate(&crate::fs::RealFileSystem);
        let issues: Vec<_> = result
            .issues()
            .warnings()
            .into_iter()
            .filter(|i| i.field == "state_directory")
            .collect();

        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].category, ValidationErrorCategory::Advisory);
        assert!(issues[0].message.contains("not there yet"), "{issues:?}");
    }

    // An empty value is unset, as it is to a run, so the default is checked.
    #[test]
    fn state_directory_empty_takes_the_default() {
        // A file at the default under the home directory, so checking it is
        // visible as a row.
        let mut fs = crate::fs::MockFileSystem::default();
        fs.expect_expand_path()
            .withf(|path| path == std::path::Path::new("~"))
            .returning(|_| Ok(PathBuf::from("/home/me")));
        fs.expect_directory_state()
            .withf(|path| path == std::path::Path::new("/home/me/.local/state/selfie"))
            .returning(|_| {
                crate::fs::DirectoryState::Absent(crate::fs::AbsentReason::Occupied {
                    kind: "regular file",
                })
            });
        fs.expect_directory_state()
            .returning(|_| crate::fs::DirectoryState::Directory);
        fs.expect_list_directory().returning(|_| Ok(Vec::new()));
        fs.expect_irregular_target_refusal().returning(|_| None);
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory("/nowhere/packages")
            .state_directory(PathBuf::from(""))
            .build();

        let result = as_file(&config).validate(&fs);
        let issues: Vec<_> = result
            .issues()
            .all_issues()
            .iter()
            .filter(|i| i.field == "state_directory")
            .cloned()
            .collect();

        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_ne!(issues[0].category, ValidationErrorCategory::RequiredField);
        assert!(
            issues[0].message.contains("/home/me/.local/state/selfie"),
            "{issues:?}"
        );
    }

    // --- validate_command_timeout tests ---

    #[test]
    fn default_command_timeout_produces_no_issues() {
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory("/tmp")
            .build();

        let result = as_file(&config).validate(&crate::fs::RealFileSystem);
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

        let result = as_file(&config).validate(&crate::fs::RealFileSystem);
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

        let result = as_file(&config).validate(&crate::fs::RealFileSystem);
        let warnings: Vec<_> = result
            .issues()
            .warnings()
            .into_iter()
            .filter(|i| i.field == "command_timeout")
            .collect();

        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].category, ValidationErrorCategory::InvalidValue);
    }

    // --- ConfigFile::validate() integration tests ---

    #[test]
    fn valid_config_has_no_issues() {
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory("/tmp")
            .build();

        let result = as_file(&config).validate(&crate::fs::RealFileSystem);
        assert!(!result.issues().has_issues());
        assert!(result.issues().is_valid());
    }

    #[test]
    fn invalid_config_collects_all_issues() {
        let config = SelfieConfigBuilder::default()
            .environment("")
            .package_directory("")
            .build();

        let result = as_file(&config).validate(&crate::fs::RealFileSystem);
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
        listable(&mut fs);
        let config = SelfieConfigBuilder::default()
            .environment("linux")
            .package_directory("/nowhere/packages")
            .dotfiles_directory(PathBuf::from("/nowhere/dotfiles"))
            .state_directory(PathBuf::from("/nowhere/state"))
            .build();

        let result = as_file(&config).validate(&fs);
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

    // No command can read specs from a file, and none can create the directory
    // over it, so the package directory's report is an error.
    #[test]
    fn a_package_directory_a_file_occupies_is_an_error() {
        let issue = directory_issue(
            "package_directory",
            DirectoryState::Absent(AbsentReason::Occupied {
                kind: "regular file",
            }),
        )
        .expect("an issue");

        assert_eq!(issue.level, ValidationLevel::Error);
        assert_eq!(issue.category, ValidationErrorCategory::PathFormat);
        assert!(issue.message.contains("it is a regular file"), "{issue:?}");
    }

    // The reading commands warn about a file at the dotfiles directory and carry
    // on without standalone dotfiles, so this reports what they do.
    #[test]
    fn a_dotfiles_directory_a_file_occupies_is_a_warning() {
        let issue = directory_issue(
            "dotfiles_directory",
            DirectoryState::Absent(AbsentReason::Occupied {
                kind: "regular file",
            }),
        )
        .expect("an issue");

        assert_eq!(issue.level, ValidationLevel::Warning);
        assert!(issue.message.contains("it is a regular file"), "{issue:?}");
        assert!(
            issue
                .message
                .contains("standalone dotfiles will not be read"),
            "{issue:?}"
        );
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
        assert!(issue.message.contains("mkdir -p --"), "{issue:?}");
        // Said once, in the message: the suggestion adds only the correction.
        assert_eq!(
            suggestion(&issue),
            "Correct the setting if the path is a typo",
            "{issue:?}"
        );
    }

    // A path that could not be checked may hide standalone dotfiles, and the
    // reading commands refuse to go on without them. A loop is one such path.
    #[test]
    fn a_looping_dotfiles_directory_is_an_error() {
        let issue = directory_issue("dotfiles_directory", unknown()).expect("an issue");

        assert_eq!(issue.level, ValidationLevel::Error);
        assert!(issue.message.contains("could not be checked"), "{issue:?}");
        assert_eq!(
            suggestion(&issue),
            "Check the path and each directory above it"
        );
    }

    // A directory whose entries cannot be read is found only by listing it, as
    // the commands do. The port calls it a directory.
    fn unlistable_issue(field: &str) -> Option<ValidationIssue> {
        let mut fs = MockFileSystem::default();
        fs.mock_directories_exist();
        fs.expect_list_directory().returning(|_| {
            Err(crate::fs::FileSystemError::IoError(std::sync::Arc::new(
                std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            )))
        });
        let config = SelfieConfigBuilder::default()
            .environment("linux")
            .package_directory("/nowhere/packages")
            .dotfiles_directory(PathBuf::from("/nowhere/dotfiles"))
            .state_directory(PathBuf::from("/nowhere/state"))
            .build();

        as_file(&config)
            .validate(&fs)
            .issues()
            .all_issues()
            .iter()
            .find(|i| i.field == field)
            .cloned()
    }

    #[test]
    fn an_unlistable_package_directory_is_an_error() {
        let issue = unlistable_issue("package_directory").expect("an issue");

        assert_eq!(issue.level, ValidationLevel::Error);
        assert!(issue.message.contains("could not be listed"), "{issue:?}");
    }

    #[test]
    fn an_unlistable_dotfiles_directory_is_an_error() {
        let issue = unlistable_issue("dotfiles_directory").expect("an issue");

        assert_eq!(issue.level, ValidationLevel::Error);
        assert!(issue.message.contains("could not be listed"), "{issue:?}");
    }

    // The state directory gets the verdict a run reaches over the same path. A
    // missing one is created on first use, so it warns only that the path may be
    // a typo, in the run's own words.
    #[test]
    fn a_missing_state_directory_warns_that_it_is_not_there_yet() {
        let issue = directory_issue(
            "state_directory",
            DirectoryState::Absent(AbsentReason::Empty),
        )
        .expect("an issue");

        assert_eq!(issue.level, ValidationLevel::Warning);
        assert_eq!(issue.category, ValidationErrorCategory::Advisory);
        assert!(issue.message.contains("is not there yet"), "{issue:?}");
        assert!(issue.message.contains("typo"), "{issue:?}");
        assert!(!suggestion(&issue).contains("mkdir"), "{issue:?}");
    }

    // A run refuses a state directory that anything else holds, or that it cannot
    // check, so the check of the configuration reports each as an error.
    #[test]
    fn a_state_directory_a_run_refuses_is_an_error() {
        for state in [
            dangling(),
            DirectoryState::Absent(AbsentReason::Occupied {
                kind: "regular file",
            }),
            DirectoryState::Absent(AbsentReason::ParentNotADirectory {
                parent: PathBuf::from("/nowhere"),
            }),
            unknown(),
        ] {
            let issue = directory_issue("state_directory", state).expect("an issue");

            assert_eq!(issue.level, ValidationLevel::Error, "{issue:?}");
            assert!(
                issue.message.contains("Cannot use the deploy state"),
                "{issue:?}"
            );
        }
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
        listable(&mut fs);
        let config = SelfieConfigBuilder::default()
            .environment("linux")
            .package_directory("/nowhere/packages")
            .dotfiles_directory(PathBuf::from("~/dotfiles"))
            .state_directory(PathBuf::from("/nowhere/state"))
            .build();

        let result = as_file(&config).validate(&fs);

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
            listable(&mut fs);
            let config = SelfieConfigBuilder::default()
                .environment("linux")
                .package_directory("/nowhere/packages")
                .dotfiles_directory(PathBuf::from(setting))
                .state_directory(PathBuf::from("/nowhere/state"))
                .build();

            let result = as_file(&config).validate(&fs);

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
        listable(&mut fs);
        let config = SelfieConfigBuilder::default()
            .environment("linux")
            .package_directory("/nowhere/packages")
            .dotfiles_directory(PathBuf::from("~/dotfiles"))
            .state_directory(PathBuf::from("/nowhere/state"))
            .build();

        let result = as_file(&config).validate(&fs);

        let issue = result
            .issues()
            .all_issues()
            .iter()
            .find(|i| i.field == "dotfiles_directory")
            .expect("an issue");
        assert_eq!(issue.level, ValidationLevel::Warning);
        assert!(issue.message.contains("could not resolve ~"), "{issue:?}");
    }

    // A run keeps a `~` it cannot resolve as written and refuses the relative
    // path that leaves, so validate reports an error there too.
    #[test]
    fn a_state_directory_whose_home_cannot_be_resolved_is_an_error() {
        let mut fs = MockFileSystem::default();
        fs.expect_expand_path()
            .returning(|_| Err(crate::fs::FileSystemError::HomeDirNotFound));
        fs.mock_directories_exist();
        listable(&mut fs);
        let config = SelfieConfigBuilder::default()
            .environment("linux")
            .package_directory("/nowhere/packages")
            .state_directory(PathBuf::from("~/state"))
            .build();

        let result = as_file(&config).validate(&fs);

        let issue = result
            .issues()
            .all_issues()
            .iter()
            .find(|i| i.field == "state_directory")
            .expect("an issue");
        assert_eq!(issue.level, ValidationLevel::Error, "{issue:?}");
        assert!(issue.message.contains("could not resolve ~"), "{issue:?}");
    }

    #[test]
    fn a_relative_state_directory_is_an_error() {
        let config = SelfieConfigBuilder::default()
            .environment("linux")
            .package_directory("/tmp")
            .state_directory(PathBuf::from("state"))
            .build();

        let result = as_file(&config).validate(&crate::fs::RealFileSystem);
        assert!(
            result
                .issues()
                .errors()
                .iter()
                .any(|i| i.field == "state_directory"),
            "{:?}",
            result.issues()
        );
    }

    // An unset directory setting is checked at the default the commands read,
    // so what is wrong there is reported as for a configured path. Nothing at an
    // unset default is the ordinary state and is not reported.
    fn default_directory_issues(
        dotfiles_default: DirectoryState,
        state_default: DirectoryState,
    ) -> Vec<ValidationIssue> {
        let mut fs = MockFileSystem::default();
        fs.expect_expand_path()
            .withf(|path| path == std::path::Path::new("~"))
            .returning(|_| Ok(PathBuf::from("/home/me")));
        fs.expect_directory_state()
            .withf(|path| path == std::path::Path::new("/nowhere/dotfiles"))
            .returning(move |_| dotfiles_default.clone());
        fs.expect_directory_state()
            .withf(|path| path == std::path::Path::new("/home/me/.local/state/selfie"))
            .returning(move |_| state_default.clone());
        fs.mock_directories_exist();
        listable(&mut fs);
        let config = SelfieConfigBuilder::default()
            .environment("linux")
            .package_directory("/nowhere/packages")
            .build();

        as_file(&config)
            .validate(&fs)
            .issues()
            .all_issues()
            .iter()
            .filter(|i| i.field == "dotfiles_directory" || i.field == "state_directory")
            .cloned()
            .collect()
    }

    #[test]
    fn a_file_at_the_default_dotfiles_directory_is_reported() {
        let issues = default_directory_issues(
            DirectoryState::Absent(AbsentReason::Occupied {
                kind: "regular file",
            }),
            DirectoryState::Directory,
        );

        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].field, "dotfiles_directory");
        assert_eq!(issues[0].level, ValidationLevel::Warning);
        assert!(
            issues[0].message.contains("/nowhere/dotfiles"),
            "{issues:?}"
        );
    }

    #[test]
    fn a_dangling_link_at_the_default_state_directory_is_an_error() {
        let issues = default_directory_issues(DirectoryState::Directory, dangling());

        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].field, "state_directory");
        assert_eq!(issues[0].level, ValidationLevel::Error);
        assert!(
            issues[0].message.contains("Cannot use the deploy state"),
            "{issues:?}"
        );
    }

    #[test]
    fn missing_unset_defaults_are_not_reported() {
        let issues = default_directory_issues(
            DirectoryState::Absent(AbsentReason::Empty),
            DirectoryState::Absent(AbsentReason::Empty),
        );

        assert!(issues.is_empty(), "{issues:?}");
    }
}
