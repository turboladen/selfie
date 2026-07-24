use std::path::PathBuf;

use serde_saphyr::Location;

use crate::validation::{ValidationErrorCategory, ValidationIssue, ValidationIssues};

use super::Package;

/// Known top-level keys in a package YAML file.
///
/// Used by `validate_unknown_fields()` and verified against the `Package` struct
/// by `test_known_fields_matches_package_struct`.
pub(crate) const KNOWN_PACKAGE_FIELDS: &[&str] = &[
    "name",
    "homepage",
    "description",
    "dotfiles",
    "post_install_note",
    "environments",
];

/// Format a `Location` as a human-readable string, returning `None` for unknown locations.
fn location_string(loc: &Location) -> Option<String> {
    if *loc == Location::UNKNOWN {
        None
    } else {
        Some(format!("line {}, column {}", loc.line(), loc.column()))
    }
}

/// Results of a package validation
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ValidationResult {
    /// The package that was validated
    ///
    pub(crate) package_name: String,

    /// The package file path
    ///
    pub(crate) package_path: Option<PathBuf>,

    /// List of validation issues found
    ///
    pub(crate) issues: ValidationIssues,
}

impl ValidationResult {
    #[must_use]
    pub fn package_name(&self) -> &str {
        &self.package_name
    }

    #[must_use]
    pub fn package_path(&self) -> Option<&PathBuf> {
        self.package_path.as_ref()
    }

    #[must_use]
    pub fn issues(&self) -> &ValidationIssues {
        &self.issues
    }
}

impl Package {
    /// Perform all basic domain validations
    ///
    /// Validates the package definition against all known validation rules,
    /// including required fields, URL formats, environment configurations,
    /// and command syntax. Returns a comprehensive validation result that
    /// can be used to provide user feedback.
    ///
    /// # Arguments
    ///
    /// * `current_env` - The current environment name to use for validation context
    ///
    /// All validation issues are collected and returned in the `ValidationResult` structure.
    #[must_use]
    pub fn validate(&self, current_env: &str) -> ValidationResult {
        let mut issues = Vec::new();

        issues.extend(self.validate_required_fields());
        issues.extend(self.validate_unknown_fields());
        issues.extend(self.validate_urls());
        issues.extend(self.validate_environments_contents(current_env));
        issues.extend(self.validate_command_syntax());
        issues.extend(self.validate_dotfiles());
        issues.extend(self.validate_filename_consistency());

        ValidationResult {
            package_name: self.name.value.clone(),
            package_path: Some(self.path.clone()),
            issues: issues.into(),
        }
    }

    /// Validate that all required fields are present and properly formatted
    ///
    /// Checks for the presence and validity of essential package fields:
    /// - Package name (non-empty, valid characters)
    /// - At least one environment configuration
    ///
    /// All validation issues are collected and returned as a vector of `ValidationIssue`.
    pub(crate) fn validate_required_fields(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();

        // Check name
        if let Err(issue) = self.validate_name() {
            issues.push(issue);
        }

        // Check environments
        if let Err(issue) = self.validate_environments_exists() {
            issues.push(issue);
        }

        issues
    }

    /// Flag unknown top-level YAML fields by parsing the raw YAML content.
    ///
    /// Fields starting with `_` are allowed (YAML anchor definitions like
    /// `_brew: &brew`). Anything else is an error — likely a typo or a
    /// renamed field (e.g., `configs` instead of `dotfiles`).
    pub(crate) fn validate_unknown_fields(&self) -> Vec<ValidationIssue> {
        if self.raw_yaml.is_empty() {
            return vec![];
        }

        // Uses serde_json::Value as the "any value" container because serde-saphyr
        // has no Value type. This works because we only inspect keys, not values.
        let Ok(raw) = serde_saphyr::from_str::<std::collections::HashMap<String, serde_json::Value>>(
            &self.raw_yaml,
        ) else {
            return vec![]; // Parse errors handled elsewhere
        };

        let expected = KNOWN_PACKAGE_FIELDS.join(", ");

        raw.keys()
            .filter(|k| !k.starts_with('_') && !KNOWN_PACKAGE_FIELDS.contains(&k.as_str()))
            .map(|k| {
                ValidationIssue::error(
                    ValidationErrorCategory::InvalidValue,
                    k,
                    &format!("unknown field '{k}'; expected one of: {expected}"),
                    None,
                )
            })
            .collect()
    }

    /// Validate the package name format
    ///
    /// Package names must be non-empty and contain only alphanumeric characters,
    /// hyphens, and underscores. They cannot start or end with special characters.
    ///
    /// # Errors
    ///
    /// Returns a `ValidationIssue` if the package name is invalid:
    /// - Empty name
    /// - Contains invalid characters
    /// - Starts or ends with special characters
    fn validate_name(&self) -> Result<(), ValidationIssue> {
        /// Check if a string is a valid package name
        ///
        /// # Arguments
        ///
        /// * `name` - The package name to validate
        ///
        /// # Returns
        ///
        /// `true` if the name is valid, `false` otherwise
        fn is_valid_package_name(name: &str) -> bool {
            // Package names should only contain alphanumeric chars, hyphens, and underscores
            !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        }

        let name_loc = location_string(&self.name.defined);

        if self.name.value.is_empty() {
            return Err(ValidationIssue::error_at(
                ValidationErrorCategory::RequiredField,
                "name",
                "Package name is required",
                Some("Add 'name: your-package-name' to the package file."),
                name_loc,
            ));
        } else if !is_valid_package_name(&self.name.value) {
            return Err(ValidationIssue::error_at(
                ValidationErrorCategory::InvalidValue,
                "name",
                "Package name contains invalid characters",
                Some("Use only alphanumeric characters, hyphens, and underscores."),
                name_loc,
            ));
        }

        Ok(())
    }

    /// Validate that at least one environment is defined
    ///
    /// Ensures the package has at least one environment configuration,
    /// which is required for package operations.
    ///
    /// # Errors
    ///
    /// Returns a `ValidationIssue` error if no environments are defined.
    fn validate_environments_exists(&self) -> Result<(), ValidationIssue> {
        if self.environments.value.is_empty() {
            Err(ValidationIssue::error_at(
                ValidationErrorCategory::RequiredField,
                "environments",
                "At least one environment must be defined",
                Some("Add an 'environments' section with at least one environment."),
                location_string(&self.environments.defined),
            ))
        } else {
            Ok(())
        }
    }

    /// Validate environment configurations and content
    ///
    /// Checks that environments are properly configured, including whether
    /// the current environment is defined and that install commands are present.
    ///
    /// # Arguments
    ///
    /// * `current_env` - The current environment name to validate against
    ///
    /// All validation issues are collected and returned as a vector of `ValidationIssue`.
    pub(crate) fn validate_environments_contents(&self, current_env: &str) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();

        // Check if current environment is configured
        if !current_env.is_empty() && !self.environments.value.contains_key(current_env) {
            issues.push(ValidationIssue::warning(
                ValidationErrorCategory::Environment,
                "environments",
                &format!("Current environment '{current_env}' is not configured"),
                Some(&format!(
                    "Add an environment section for '{current_env}' if needed for this environment."
                )),
            ));
        }

        // Validate each environment's required fields
        for (env_name, env_config) in &self.environments.value {
            if env_config.install.is_empty() {
                issues.push(ValidationIssue::error(
                    ValidationErrorCategory::RequiredField,
                    &format!("environments.{env_name}.install"),
                    "Install command is required",
                    Some("Add an install command like 'brew install package-name'."),
                ));
            }

            // Validate dependencies (check for empty names)
            for (i, dep) in env_config.dependencies.iter().enumerate() {
                if dep.is_empty() {
                    issues.push(ValidationIssue::error(
                        ValidationErrorCategory::InvalidValue,
                        &format!("environments.{env_name}.dependencies[{i}]"),
                        "Dependency name cannot be empty",
                        Some("Remove the empty dependency or provide a valid name."),
                    ));
                }
            }
        }

        issues
    }

    /// Validate URL fields
    pub(crate) fn validate_urls(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();

        // Check homepage URL if present
        if let Some(spanned_homepage) = &self.homepage {
            let homepage = &spanned_homepage.value;
            let hp_loc = location_string(&spanned_homepage.defined);
            match url::Url::parse(homepage) {
                Ok(url) => {
                    // Check scheme
                    if url.scheme() != "http" && url.scheme() != "https" {
                        issues.push(ValidationIssue::warning_at(
                            ValidationErrorCategory::UrlFormat,
                            "homepage",
                            &format!(
                                "URL should use http or https scheme, found: {}",
                                url.scheme()
                            ),
                            Some("Use https:// prefix for the URL."),
                            hp_loc.clone(),
                        ));
                    }
                }
                Err(err) => {
                    issues.push(ValidationIssue::error_at(
                        ValidationErrorCategory::UrlFormat,
                        "homepage",
                        &format!("Invalid URL format: {err}"),
                        Some("Provide a valid URL with http:// or https:// prefix."),
                        hp_loc.clone(),
                    ));
                }
            }
        }

        issues
    }
    /// Basic command syntax validation that doesn't require external dependencies
    pub(crate) fn validate_command_syntax(&self) -> Vec<ValidationIssue> {
        self.validate_command_syntax_for(self.environments.value.keys().map(String::as_str))
    }

    /// Validate command syntax for only the specified environments
    pub(crate) fn validate_command_syntax_for<'a>(
        &self,
        env_names: impl Iterator<Item = &'a str>,
    ) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();

        for env_name in env_names {
            let Some(env_config) = self.environments.value.get(env_name) else {
                continue;
            };

            issues.extend(Self::validate_single_command(
                &env_config.install,
                &format!("environments.{env_name}.install"),
            ));

            if let Some(check_cmd) = &env_config.check {
                issues.extend(Self::validate_single_command(
                    check_cmd,
                    &format!("environments.{env_name}.check"),
                ));
            }

            if let Some(audit_cmd) = &env_config.audit {
                issues.extend(Self::validate_single_command(
                    audit_cmd,
                    &format!("environments.{env_name}.audit"),
                ));
            }
        }

        issues
    }

    /// Validate a single command for syntax issues
    fn validate_single_command(command: &str, field_name: &str) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();

        // Check for unmatched quotes
        let mut in_single_quotes = false;
        let mut in_double_quotes = false;

        for c in command.chars() {
            match c {
                '\'' if !in_double_quotes => in_single_quotes = !in_single_quotes,
                '"' if !in_single_quotes => in_double_quotes = !in_double_quotes,
                _ => {}
            }
        }

        if in_single_quotes {
            issues.push(ValidationIssue::error(
                ValidationErrorCategory::CommandSyntax,
                field_name,
                "Unmatched single quote in command",
                Some("Add a closing single quote (') to the command."),
            ));
        }

        if in_double_quotes {
            issues.push(ValidationIssue::error(
                ValidationErrorCategory::CommandSyntax,
                field_name,
                "Unmatched double quote in command",
                Some("Add a closing double quote (\") to the command."),
            ));
        }

        // Check for invalid pipe usage
        if command.contains("| |") {
            issues.push(ValidationIssue::error(
                ValidationErrorCategory::CommandSyntax,
                field_name,
                "Invalid pipe usage in command",
                Some("Remove duplicate pipe symbols."),
            ));
        }

        // Check for invalid redirections
        for redirect in &[">", ">>", "<"] {
            if command.contains(&format!("{redirect} "))
                && !command.contains(&format!("{redirect} /"))
                && !command.contains(&format!("{redirect} ~/"))
            {
                issues.push(ValidationIssue::warning(
                    ValidationErrorCategory::CommandSyntax,
                    field_name,
                    &format!("Potential invalid redirection with {redirect}"),
                    Some("Ensure the redirection path is valid and absolute."),
                ));
            }
        }

        // Check for command injection risks with backticks
        if command.contains('`') {
            issues.push(ValidationIssue::warning(
                ValidationErrorCategory::CommandSyntax,
                field_name,
                "Contains command substitution with backticks",
                Some("Consider using $() for command substitution instead of backticks."),
            ));
        }

        issues
    }

    /// Validate dotfile entry definitions
    ///
    /// Checks each dotfile entry for:
    /// - Empty source path (error)
    /// - Path traversal in source via `..` (error)
    /// - Target path that is not absolute and doesn't start with `~` (error)
    pub(crate) fn validate_dotfiles(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();

        for (i, dotfile) in self.dotfiles.iter().enumerate() {
            let source = dotfile.source();
            let target = dotfile.target();

            if source.is_empty() {
                issues.push(ValidationIssue::error(
                    ValidationErrorCategory::InvalidValue,
                    &format!("dotfiles[{i}].source"),
                    "Dotfile source path cannot be empty",
                    Some("Provide a relative path to the dotfile within the repository."),
                ));
            }

            if std::path::Path::new(source)
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                issues.push(ValidationIssue::error(
                    ValidationErrorCategory::InvalidValue,
                    &format!("dotfiles[{i}].source"),
                    "Dotfile source path must not contain '..' (path traversal)",
                    Some("Use a relative path without parent directory references."),
                ));
            }

            if source.starts_with('/') || source.starts_with('~') {
                issues.push(ValidationIssue::error(
                    ValidationErrorCategory::InvalidValue,
                    &format!("dotfiles[{i}].source"),
                    "Dotfile source path must be relative",
                    Some("Use a path relative to the dotfiles directory, e.g., 'pkg/config.toml'."),
                ));
            }

            if !target.starts_with('/') && !target.starts_with('~') {
                issues.push(ValidationIssue::error(
                    ValidationErrorCategory::InvalidValue,
                    &format!("dotfiles[{i}].target"),
                    "Dotfile target path must be absolute or start with '~'",
                    Some("Use an absolute path like '/etc/config' or '~/.config/file'."),
                ));
            }
        }

        issues
    }

    /// Warn when the package name field doesn't match the filename.
    ///
    /// While technically allowed, a mismatch between `name:` and the YAML filename
    /// causes confusion — tools report the internal name, but users expect it to
    /// match the file they see on disk.
    pub(crate) fn validate_filename_consistency(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();

        let file_stem = self.path().file_stem().and_then(|s| s.to_str());

        if let Some(stem) = file_stem
            && stem != self.name()
        {
            issues.push(ValidationIssue::warning(
                ValidationErrorCategory::InvalidValue,
                "name",
                &format!(
                    "Package name '{}' does not match filename '{}'",
                    self.name(),
                    self.path()
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                ),
                Some(&format!(
                    "Consider renaming to match: either set name to '{stem}' or rename the file to '{}.yml'",
                    self.name()
                )),
            ));
        }

        issues
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        package::{DotfileEntry, EnvironmentConfig, builder::PackageBuilder},
        validation::ValidationLevel,
    };

    use super::*;

    #[test]
    fn test_validate_valid_package() {
        let package = PackageBuilder::default()
            .name("test-package")
            .environment("test-env", |b| b.install("test install"))
            .build();

        assert!(package.validate("test-env").issues().is_valid());
    }

    #[test]
    fn test_validate_urls() {
        // Test invalid URL
        let package = PackageBuilder::default()
            .name("test-package")
            .homepage("not-a-valid-url")
            .environment("test-env", |b| b.install("test install"))
            .build();

        let issues = package.validate_urls();
        assert_eq!(issues.len(), 1);
        assert!(issues[0].category == ValidationErrorCategory::UrlFormat);

        // Test valid URL but wrong scheme (ftp)
        let package = PackageBuilder::default()
            .name("test-package")
            .homepage("ftp://example.com")
            .environment("test-env", |b| b.install("test install"))
            .build();

        let issues = package.validate_urls();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].level(), ValidationLevel::Warning);
        assert!(issues[0].message.contains("scheme"));

        // Test valid URL with correct scheme
        let package = PackageBuilder::default()
            .name("test-package")
            .homepage("https://example.com")
            .environment("test-env", |b| b.install("test install"))
            .build();

        let issues = package.validate_urls();
        assert_eq!(issues.len(), 0);
    }

    #[test]
    fn test_validate_environments() {
        // Test missing current environment
        let package = PackageBuilder::default()
            .name("test-package")
            .environment("other-env", |b| b.install("test install"))
            .build();

        let issues = package.validate_environments_contents("test-env");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].level(), ValidationLevel::Warning);
        assert!(issues[0].message.contains("not configured"));

        // Test empty install command
        let mut package = PackageBuilder::default().name("test-package").build();

        let env_config = EnvironmentConfig {
            install: String::new(),
            check: None,
            audit: None,
            dependencies: vec![],
            recommends: vec![],
            dotfiles: vec![],
        };

        package
            .environments
            .value
            .insert("test-env".to_string(), env_config);

        let issues = package.validate_environments_contents("test-env");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].level(), ValidationLevel::Error);
        assert!(issues[0].message.contains("required"));
    }

    #[test]
    fn test_validate_command_syntax() {
        // Test unmatched quote
        let package = PackageBuilder::default()
            .name("test-package")
            .environment("test-env", |b| b.install("echo 'unmatched"))
            .build();

        let issues = package.validate_command_syntax();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].level(), ValidationLevel::Error);
        assert!(issues[0].message.contains("Unmatched single quote"));

        // Test invalid pipe
        let package = PackageBuilder::default()
            .name("test-package")
            .environment("test-env", |b| b.install("echo test | | grep test"))
            .build();

        let issues = package.validate_command_syntax();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].level(), ValidationLevel::Error);
        assert!(issues[0].message.contains("Invalid pipe usage"));

        // Test backticks (warning)
        let package = PackageBuilder::default()
            .name("test-package")
            .environment("test-env", |b| b.install("echo `date`"))
            .build();

        let issues = package.validate_command_syntax();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].level(), ValidationLevel::Warning);
        assert!(issues[0].message.contains("backticks"));
    }

    #[test]
    fn test_full_validate() {
        // Test a valid package
        let package = PackageBuilder::default()
            .name("test-package")
            .homepage("https://example.com")
            .description("A test package")
            .environment("test-env", |b| b.install("echo test"))
            .build();

        let result = package.validate("test-env");
        assert!(!result.issues().has_issues());

        // Test an invalid package with multiple issues
        let package = PackageBuilder::default()
            .name("")
            .homepage("invalid-url")
            .environment("other-env", |b| b.install("echo `test`"))
            .build();

        let result = package.validate("test-env");
        assert!(result.issues().all_issues().len() >= 3); // At least 3 issues should be found
    }

    #[test]
    fn test_validate_dotfile_relative_target_errors() {
        let package = PackageBuilder::default()
            .name("bad-config")
            .dotfiles(vec![DotfileEntry::new("src/file.txt", "relative/path.txt")])
            .build();
        let result = package.validate("test-env");
        assert!(result.issues().has_errors());
    }

    #[test]
    fn test_validate_dotfile_absolute_target_passes() {
        let package = PackageBuilder::default()
            .name("good-config")
            .environment("test-env", |b| b.install("echo hi"))
            .dotfiles(vec![DotfileEntry::new(
                "src/file.txt",
                "~/.config/file.txt",
            )])
            .build();
        let result = package.validate("test-env");
        assert!(!result.issues().has_errors());
    }

    #[test]
    fn test_validate_dotfile_empty_source_errors() {
        let package = PackageBuilder::default()
            .name("bad-source")
            .dotfiles(vec![DotfileEntry::new("", "~/.config/file.txt")])
            .build();
        let result = package.validate("test-env");
        assert!(result.issues().has_errors());
    }

    #[test]
    fn test_validate_dotfile_path_traversal_errors() {
        let package = PackageBuilder::default()
            .name("bad-traversal")
            .dotfiles(vec![DotfileEntry::new(
                "../etc/passwd",
                "~/.config/file.txt",
            )])
            .build();
        let result = package.validate("test-env");
        assert!(result.issues().has_errors());
    }

    /// Ensures KNOWN_FIELDS stays in sync with the Package struct.
    ///
    /// Serializes a fully populated Package to YAML, re-parses as a raw map,
    /// and asserts every key is present in KNOWN_FIELDS. If this test fails,
    /// a field was added to Package without updating KNOWN_FIELDS.
    #[test]
    fn test_known_fields_matches_package_struct() {
        let package = PackageBuilder::default()
            .name("sync-test")
            .homepage("https://example.com")
            .description("A test")
            .post_install_note("note")
            .dotfiles(vec![DotfileEntry::new("src", "~/.target")])
            .environment("test-env", |b| b.install("echo hi"))
            .build();

        let yaml = serde_saphyr::to_string(&package).unwrap();
        let raw: std::collections::HashMap<String, serde_json::Value> =
            serde_saphyr::from_str(&yaml).unwrap();

        for key in raw.keys() {
            assert!(
                KNOWN_PACKAGE_FIELDS.contains(&key.as_str()),
                "Package serialized a field '{key}' not in KNOWN_PACKAGE_FIELDS — update the list in validate.rs"
            );
        }
    }

    #[test]
    fn test_validate_filename_consistency_matching() {
        let package = PackageBuilder::default()
            .name("ripgrep")
            .environment("test-env", |b| b.install("brew install ripgrep"))
            .path(PathBuf::from("/packages/ripgrep.yml"))
            .build();

        let issues = package.validate_filename_consistency();
        assert!(issues.is_empty());
    }

    #[test]
    fn test_validate_filename_consistency_mismatch() {
        let package = PackageBuilder::default()
            .name("fisher")
            .environment("test-env", |b| b.install("fish -c 'fisher install'"))
            .path(PathBuf::from("/packages/fish-fisher.yml"))
            .build();

        let issues = package.validate_filename_consistency();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].level(), ValidationLevel::Warning);
        assert!(issues[0].message.contains("fisher"));
        assert!(issues[0].message.contains("fish-fisher.yml"));
    }
}
