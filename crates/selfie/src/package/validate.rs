use std::path::PathBuf;

use crate::validation::{ValidationErrorCategory, ValidationIssue, ValidationIssues};

use super::Package;

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
        issues.extend(self.validate_urls());
        issues.extend(self.validate_environments_contents(current_env));
        issues.extend(self.validate_command_syntax());
        issues.extend(self.validate_configs());

        ValidationResult {
            package_name: self.name.clone(),
            package_path: Some(self.path.clone()),
            issues: issues.into(),
        }
    }

    /// Validate that all required fields are present and properly formatted
    ///
    /// Checks for the presence and validity of essential package fields:
    /// - Package name (non-empty, valid characters)
    /// - Package version (semantic version format)
    /// - At least one environment configuration
    ///
    /// All validation issues are collected and returned as a vector of `ValidationIssue`.
    pub(crate) fn validate_required_fields(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();

        // Check name
        if let Err(issue) = self.validate_name() {
            issues.push(issue);
        }

        // Check version
        if let Err(issue) = self.validate_version() {
            issues.push(issue);
        }

        // Check environments
        if let Err(issue) = self.validate_environments_exists() {
            issues.push(issue);
        }

        issues
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

        if self.name.is_empty() {
            return Err(ValidationIssue::error(
                ValidationErrorCategory::RequiredField,
                "name",
                "Package name is required",
                Some("Add 'name: your-package-name' to the package file."),
            ));
        } else if !is_valid_package_name(&self.name) {
            return Err(ValidationIssue::error(
                ValidationErrorCategory::InvalidValue,
                "name",
                "Package name contains invalid characters",
                Some("Use only alphanumeric characters, hyphens, and underscores."),
            ));
        }

        Ok(())
    }

    /// Validate the package version format
    ///
    /// Checks that the version follows the SemVer 2.0.0 spec (major.minor.patch,
    /// with optional pre-release and build metadata). Uses the `semver` crate.
    ///
    /// # Errors
    ///
    /// Returns a `ValidationIssue` if:
    /// - Version is empty (error)
    /// - Version doesn't follow semantic versioning format (warning)
    fn validate_version(&self) -> Result<(), ValidationIssue> {
        if self.version.is_empty() {
            return Err(ValidationIssue::error(
                ValidationErrorCategory::RequiredField,
                "version",
                "Package version is required",
                Some("Add 'version: \"0.1.0\"' to the package file."),
            ));
        } else if semver::Version::parse(&self.version).is_err() {
            return Err(ValidationIssue::warning(
                ValidationErrorCategory::InvalidValue,
                "version",
                "Package version should follow semantic versioning",
                Some("Consider using a semantic version like '1.0.0'."),
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
        if self.environments.is_empty() {
            Err(ValidationIssue::error(
                ValidationErrorCategory::RequiredField,
                "environments",
                "At least one environment must be defined",
                Some("Add an 'environments' section with at least one environment."),
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
        if !current_env.is_empty() && !self.environments.contains_key(current_env) {
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
        for (env_name, env_config) in &self.environments {
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
        if let Some(homepage) = &self.homepage {
            match url::Url::parse(homepage) {
                Ok(url) => {
                    // Check scheme
                    if url.scheme() != "http" && url.scheme() != "https" {
                        issues.push(ValidationIssue::warning(
                            ValidationErrorCategory::UrlFormat,
                            "homepage",
                            &format!(
                                "URL should use http or https scheme, found: {}",
                                url.scheme()
                            ),
                            Some("Use https:// prefix for the URL."),
                        ));
                    }
                }
                Err(err) => {
                    issues.push(ValidationIssue::error(
                        ValidationErrorCategory::UrlFormat,
                        "homepage",
                        &format!("Invalid URL format: {err}"),
                        Some("Provide a valid URL with http:// or https:// prefix."),
                    ));
                }
            }
        }

        issues
    }
    /// Basic command syntax validation that doesn't require external dependencies
    pub(crate) fn validate_command_syntax(&self) -> Vec<ValidationIssue> {
        self.validate_command_syntax_for(self.environments.keys().map(String::as_str))
    }

    /// Validate command syntax for only the specified environments
    pub(crate) fn validate_command_syntax_for<'a>(
        &self,
        env_names: impl Iterator<Item = &'a str>,
    ) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();

        for env_name in env_names {
            let Some(env_config) = self.environments.get(env_name) else {
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

    /// Validate config entry definitions
    ///
    /// Checks each config entry for:
    /// - Empty source path (error)
    /// - Path traversal in source via `..` (error)
    /// - Target path that is not absolute and doesn't start with `~` (warning)
    pub(crate) fn validate_configs(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();

        for (i, config) in self.configs.iter().enumerate() {
            let source = config.source();
            let target = config.target();

            if source.is_empty() {
                issues.push(ValidationIssue::error(
                    ValidationErrorCategory::InvalidValue,
                    &format!("configs[{i}].source"),
                    "Config source path cannot be empty",
                    Some("Provide a relative path to the config file within the repository."),
                ));
            }

            if source.contains("..") {
                issues.push(ValidationIssue::error(
                    ValidationErrorCategory::InvalidValue,
                    &format!("configs[{i}].source"),
                    "Config source path must not contain '..' (path traversal)",
                    Some("Use a relative path without parent directory references."),
                ));
            }

            if !target.starts_with('/') && !target.starts_with('~') {
                issues.push(ValidationIssue::warning(
                    ValidationErrorCategory::InvalidValue,
                    &format!("configs[{i}].target"),
                    "Config target path should be absolute or start with '~'",
                    Some("Use an absolute path like '/etc/config' or '~/.config/file'."),
                ));
            }
        }

        issues
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        package::{ConfigEntry, EnvironmentConfig, builder::PackageBuilder},
        validation::ValidationLevel,
    };

    use super::*;

    #[test]
    fn test_validate_valid_package() {
        let package = PackageBuilder::default()
            .name("test-package")
            .version("1.0.0")
            .environment("test-env", |b| b.install("test install"))
            .build();

        assert!(package.validate("test-env").issues().is_valid());
    }

    #[test]
    fn test_validate_urls() {
        // Test invalid URL
        let package = PackageBuilder::default()
            .name("test-package")
            .version("1.0.0")
            .homepage("not-a-valid-url")
            .environment("test-env", |b| b.install("test install"))
            .build();

        let issues = package.validate_urls();
        assert_eq!(issues.len(), 1);
        assert!(issues[0].category == ValidationErrorCategory::UrlFormat);

        // Test valid URL but wrong scheme (ftp)
        let package = PackageBuilder::default()
            .name("test-package")
            .version("1.0.0")
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
            .version("1.0.0")
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
            .version("1.0.0")
            .environment("other-env", |b| b.install("test install"))
            .build();

        let issues = package.validate_environments_contents("test-env");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].level(), ValidationLevel::Warning);
        assert!(issues[0].message.contains("not configured"));

        // Test empty install command
        let mut package = PackageBuilder::default()
            .name("test-package")
            .version("1.0.0")
            .build();

        let env_config = EnvironmentConfig {
            install: String::new(),
            check: None,
            audit: None,
            dependencies: vec![],
            recommends: vec![],
        };

        package
            .environments
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
            .version("1.0.0")
            .environment("test-env", |b| b.install("echo 'unmatched"))
            .build();

        let issues = package.validate_command_syntax();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].level(), ValidationLevel::Error);
        assert!(issues[0].message.contains("Unmatched single quote"));

        // Test invalid pipe
        let package = PackageBuilder::default()
            .name("test-package")
            .version("1.0.0")
            .environment("test-env", |b| b.install("echo test | | grep test"))
            .build();

        let issues = package.validate_command_syntax();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].level(), ValidationLevel::Error);
        assert!(issues[0].message.contains("Invalid pipe usage"));

        // Test backticks (warning)
        let package = PackageBuilder::default()
            .name("test-package")
            .version("1.0.0")
            .environment("test-env", |b| b.install("echo `date`"))
            .build();

        let issues = package.validate_command_syntax();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].level(), ValidationLevel::Warning);
        assert!(issues[0].message.contains("backticks"));
    }

    #[test]
    fn test_validate_version_valid_semver() {
        for version in ["1.0.0", "0.1.0", "1.2.3-beta", "1.0.0+build.123"] {
            let package = PackageBuilder::default()
                .name("test-package")
                .version(version)
                .environment("test-env", |b| b.install("echo test"))
                .build();

            let issues = package.validate_required_fields();
            let version_issues: Vec<_> = issues.iter().filter(|i| i.field == "version").collect();
            assert!(
                version_issues.is_empty(),
                "Expected no issues for valid version '{version}'"
            );
        }
    }

    #[test]
    fn test_validate_version_invalid_warns() {
        for version in ["1.0", "1", "abc", "1.2.3.4"] {
            let package = PackageBuilder::default()
                .name("test-package")
                .version(version)
                .environment("test-env", |b| b.install("echo test"))
                .build();

            let issues = package.validate_required_fields();
            let version_issues: Vec<_> = issues.iter().filter(|i| i.field == "version").collect();
            assert_eq!(
                version_issues.len(),
                1,
                "Expected one warning for invalid version '{version}'"
            );
            assert_eq!(version_issues[0].level(), ValidationLevel::Warning);
        }
    }

    #[test]
    fn test_validate_version_empty_errors() {
        let package = PackageBuilder::default()
            .name("test-package")
            .version("")
            .environment("test-env", |b| b.install("echo test"))
            .build();

        let issues = package.validate_required_fields();
        let version_issues: Vec<_> = issues.iter().filter(|i| i.field == "version").collect();
        assert_eq!(version_issues.len(), 1);
        assert_eq!(version_issues[0].level(), ValidationLevel::Error);
        assert!(version_issues[0].message.contains("required"));
    }

    #[test]
    fn test_full_validate() {
        // Test a valid package
        let package = PackageBuilder::default()
            .name("test-package")
            .version("1.0.0")
            .homepage("https://example.com")
            .description("A test package")
            .environment("test-env", |b| b.install("echo test"))
            .build();

        let result = package.validate("test-env");
        assert!(!result.issues().has_issues());

        // Test an invalid package with multiple issues
        let package = PackageBuilder::default()
            .name("")
            .version("")
            .homepage("invalid-url")
            .environment("other-env", |b| b.install("echo `test`"))
            .build();

        let result = package.validate("test-env");
        assert!(result.issues().all_issues().len() >= 4); // At least 4 issues should be found
    }

    #[test]
    fn test_validate_config_relative_target_warns() {
        let package = PackageBuilder::default()
            .name("bad-config")
            .version("1.0.0")
            .configs(vec![ConfigEntry::new("src/file.txt", "relative/path.txt")])
            .build();
        let result = package.validate("test-env");
        assert!(result.issues().has_warnings() || result.issues().has_errors());
    }

    #[test]
    fn test_validate_config_absolute_target_passes() {
        let package = PackageBuilder::default()
            .name("good-config")
            .version("1.0.0")
            .configs(vec![ConfigEntry::new("src/file.txt", "~/.config/file.txt")])
            .build();
        let result = package.validate("test-env");
        // No config-related issues (there will be an env warning since "test-env" isn't configured)
        assert!(!result.issues().has_errors());
    }

    #[test]
    fn test_validate_config_empty_source_errors() {
        let package = PackageBuilder::default()
            .name("bad-source")
            .version("1.0.0")
            .configs(vec![ConfigEntry::new("", "~/.config/file.txt")])
            .build();
        let result = package.validate("test-env");
        assert!(result.issues().has_errors());
    }

    #[test]
    fn test_validate_config_path_traversal_errors() {
        let package = PackageBuilder::default()
            .name("bad-traversal")
            .version("1.0.0")
            .configs(vec![ConfigEntry::new(
                "../etc/passwd",
                "~/.config/file.txt",
            )])
            .build();
        let result = package.validate("test-env");
        assert!(result.issues().has_errors());
    }
}
