use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_saphyr::Location;

use crate::validation::{ValidationErrorCategory, ValidationIssue, ValidationIssues};

use super::{ContentSource, DotfileEntry, Package};

/// A templated dotfile entry whose file has still to be read.
///
/// Produced by [`Package::template_dotfiles`] and consumed by
/// [`validate_template_vars`], which together let validation check a template's
/// placeholders without validation itself touching the file system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateReference<'a> {
    /// Field path naming the entry, e.g. `dotfiles[0]`.
    pub field: String,
    /// The template's path, relative to the package file's directory.
    pub source: &'a str,
    /// The entry's declared bindings.
    pub vars: &'a BTreeMap<String, String>,
}

/// Check that every declared variable is actually referenced by its template.
///
/// An unused name is an error rather than a warning because it is the only signal
/// available for a misspelling: a typo in the template leaves that placeholder
/// verbatim, and the correctly-spelled declared name then goes unused. See
/// ADR-0004.
///
/// Takes the template's contents rather than its path — reading it is the
/// caller's job, which keeps validation free of file system access.
#[must_use]
pub fn validate_template_vars(
    template: &str,
    reference: &TemplateReference<'_>,
) -> Vec<ValidationIssue> {
    let referenced = crate::dotfile_service::template::placeholders(template);

    reference
        .vars
        .keys()
        .filter(|name| !referenced.contains(*name))
        .map(|name| {
            ValidationIssue::error(
                ValidationErrorCategory::InvalidValue,
                &format!("{}.vars", reference.field),
                &format!(
                    "Dotfile var '{name}' is declared but never used in template '{}'",
                    reference.source
                ),
                Some("Check for a misspelling in the template or in the var name."),
            )
        })
        .collect()
}

/// Report a template that could not be read.
///
/// Separate from [`validate_template_vars`] so that the caller, which is the one
/// doing the reading, has a single place to turn a read failure into a
/// diagnostic.
#[must_use]
pub fn unreadable_template_issue(
    reference: &TemplateReference<'_>,
    error: &impl std::fmt::Display,
) -> ValidationIssue {
    ValidationIssue::error(
        ValidationErrorCategory::InvalidValue,
        &format!("{}.source", reference.field),
        &format!(
            "Dotfile template '{}' cannot be read: {error}",
            reference.source
        ),
        Some("Templates are read during validation to check their placeholders."),
    )
}

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

        // Shared (top-level) dotfiles.
        for (i, dotfile) in self.dotfiles.iter().enumerate() {
            issues.extend(Self::validate_dotfile_entry(
                dotfile,
                &format!("dotfiles[{i}]"),
            ));
        }

        // Environment-specific dotfiles (ADR-0001): the same structural checks,
        // plus a warning when an entry overrides a shared entry's target, so the
        // override is surfaced rather than applied silently.
        for (env_name, env) in self.environments() {
            for (i, dotfile) in env.dotfiles().iter().enumerate() {
                let field = format!("environments.{env_name}.dotfiles[{i}]");
                issues.extend(Self::validate_dotfile_entry(dotfile, &field));

                if self
                    .dotfiles
                    .iter()
                    .any(|shared| shared.target() == dotfile.target())
                {
                    issues.push(ValidationIssue::warning(
                        ValidationErrorCategory::InvalidValue,
                        &format!("{field}.target"),
                        &format!(
                            "Environment '{env_name}' overrides the shared dotfile for target '{}'",
                            dotfile.target()
                        ),
                        Some(
                            "This is allowed; the environment-specific source is used in that environment.",
                        ),
                    ));
                }
            }
        }

        issues.extend(self.report_apply_time_commands());

        issues
    }

    /// Note how many commands `selfie apply` will run for this package's dotfiles.
    ///
    /// Before provider-sourced dotfiles, `selfie apply` only copied files. It now
    /// runs commands taken from the package file, which changes the trust model
    /// for anyone treating a package directory as data — and matters more as
    /// package directories are shared or overlaid. That has to be visible rather
    /// than implicit.
    ///
    /// Informational, not a warning: a package using `vars` correctly would warn
    /// on every validation, which is how warnings come to be ignored.
    ///
    /// Counts the worst case across environments rather than summing
    /// [`Package::dotfiles_with_scope`], which lists a shared entry *and* each
    /// environment that overrides it. Summing that would report three commands
    /// for a single provider entry overridden on two machines, which overstates
    /// what any one `selfie apply` runs.
    fn report_apply_time_commands(&self) -> Vec<ValidationIssue> {
        let count_of = |entries: &[DotfileEntry]| -> usize {
            entries.iter().map(DotfileEntry::command_count).sum()
        };

        // The shared set is the effective set for an environment that declares no
        // dotfiles of its own, so it is a candidate in its own right.
        let command_count = self
            .environments()
            .keys()
            .map(|env| count_of(&self.dotfiles_for_environment(env)))
            .chain(std::iter::once(count_of(&self.dotfiles)))
            .max()
            .unwrap_or(0);

        if command_count == 0 {
            return Vec::new();
        }

        vec![ValidationIssue::info(
            ValidationErrorCategory::Advisory,
            "dotfiles",
            &format!(
                "'selfie apply' executes {command_count} command(s) for this package's dotfiles"
            ),
            Some(
                "Review the 'command' and 'vars' entries before applying a package you did not write.",
            ),
        )]
    }

    /// Every templated dotfile entry, paired with the field path that names it.
    ///
    /// Checking a template's placeholders requires reading it, which validation
    /// cannot do itself. This enumerates what needs reading so the service layer
    /// can fetch each file through the repository and hand the contents to
    /// [`validate_template_vars`].
    #[must_use]
    pub fn template_dotfiles(&self) -> Vec<TemplateReference<'_>> {
        let mut refs = Vec::new();

        for (i, entry) in self.dotfiles.iter().enumerate() {
            if let ContentSource::Template { source, vars } = entry.content_source() {
                refs.push(TemplateReference {
                    field: format!("dotfiles[{i}]"),
                    source,
                    vars,
                });
            }
        }

        for (env_name, env) in self.environments() {
            for (i, entry) in env.dotfiles().iter().enumerate() {
                if let ContentSource::Template { source, vars } = entry.content_source() {
                    refs.push(TemplateReference {
                        field: format!("environments.{env_name}.dotfiles[{i}]"),
                        source,
                        vars,
                    });
                }
            }
        }

        refs
    }

    /// Structural checks for a single dotfile entry. `field` prefixes the
    /// diagnostics (e.g. `dotfiles[0]` or `environments.work.dotfiles[1]`).
    fn validate_dotfile_entry(dotfile: &DotfileEntry, field: &str) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();
        let target = dotfile.target();

        // Content-source shape. All of these checks are offline: validation must
        // work without network access and must never trigger an authentication
        // prompt, so no binding is ever executed here.
        match (dotfile.source(), dotfile.command()) {
            (Some(_), Some(_)) => issues.push(ValidationIssue::error(
                ValidationErrorCategory::InvalidValue,
                field,
                "Dotfile sets both 'source' and 'command'; exactly one is required",
                Some(
                    "Use 'source' for a file in the repository, or 'command' for content produced \
                     by a command.",
                ),
            )),
            (None, None) => issues.push(ValidationIssue::error(
                ValidationErrorCategory::RequiredField,
                field,
                "Dotfile sets neither 'source' nor 'command'; exactly one is required",
                Some(
                    "Use 'source' for a file in the repository, or 'command' for content produced \
                     by a command.",
                ),
            )),
            (None, Some(_)) if !dotfile.vars().is_empty() => issues.push(ValidationIssue::error(
                ValidationErrorCategory::InvalidValue,
                &format!("{field}.vars"),
                "Dotfile sets both 'command' and 'vars', but there is no template to render",
                Some(
                    "Drop 'vars', or replace 'command' with a 'source' template that references \
                     them.",
                ),
            )),
            _ => {}
        }

        for name in dotfile.vars().keys() {
            if !crate::dotfile_service::template::is_valid_name(name) {
                issues.push(ValidationIssue::error(
                    ValidationErrorCategory::InvalidValue,
                    &format!("{field}.vars"),
                    &format!("Dotfile var name '{name}' is not valid"),
                    Some("Names must match [A-Za-z_][A-Za-z0-9_]*."),
                ));
            }
        }

        // Source-path checks apply only to entries that have a source. A provider
        // entry's content comes from a command, so there is no path to check.
        if let Some(source) = dotfile.source() {
            if source.is_empty() {
                issues.push(ValidationIssue::error(
                    ValidationErrorCategory::InvalidValue,
                    &format!("{field}.source"),
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
                    &format!("{field}.source"),
                    "Dotfile source path must not contain '..' (path traversal)",
                    Some("Use a relative path without parent directory references."),
                ));
            }

            if source.starts_with('/') || source.starts_with('~') {
                issues.push(ValidationIssue::error(
                    ValidationErrorCategory::InvalidValue,
                    &format!("{field}.source"),
                    "Dotfile source path must be relative",
                    Some("Use a path relative to the dotfiles directory, e.g., 'pkg/config.toml'."),
                ));
            }
        }

        if !target.starts_with('/') && !target.starts_with('~') {
            issues.push(ValidationIssue::error(
                ValidationErrorCategory::InvalidValue,
                &format!("{field}.target"),
                "Dotfile target path must be absolute or start with '~'",
                Some("Use an absolute path like '/etc/config' or '~/.config/file'."),
            ));
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

    fn entry_from_yaml(yaml: &str) -> DotfileEntry {
        serde_saphyr::from_str(yaml).expect("dotfile entry should parse")
    }

    fn entry_issues(yaml: &str) -> Vec<ValidationIssue> {
        Package::validate_dotfile_entry(&entry_from_yaml(yaml), "dotfiles[0]")
    }

    fn messages(issues: &[ValidationIssue]) -> String {
        issues
            .iter()
            .map(|i| format!("{}: {}", i.field(), i.message()))
            .collect::<Vec<_>>()
            .join(" | ")
    }

    #[test]
    fn rejects_entry_with_neither_source_nor_command() {
        let issues = entry_issues("target: ~/.x");
        assert!(
            issues
                .iter()
                .any(|i| i.message().contains("neither 'source' nor 'command'")),
            "got: {}",
            messages(&issues)
        );
    }

    #[test]
    fn rejects_entry_with_both_source_and_command() {
        let issues = entry_issues("source: a.tpl\ncommand: op read x\ntarget: ~/.x");
        assert!(
            issues
                .iter()
                .any(|i| i.message().contains("both 'source' and 'command'")),
            "got: {}",
            messages(&issues)
        );
    }

    #[test]
    fn rejects_command_combined_with_vars() {
        let issues = entry_issues("command: op read x\ntarget: ~/.x\nvars:\n  a: op read y");
        assert!(
            issues
                .iter()
                .any(|i| i.message().contains("no template to render")),
            "got: {}",
            messages(&issues)
        );
    }

    #[test]
    fn rejects_invalid_var_name() {
        let issues =
            entry_issues("source: a.tpl\ntarget: ~/.x\nvars:\n  \"not-a-name\": op read y");
        assert!(
            issues.iter().any(|i| i.message().contains("not-a-name")),
            "got: {}",
            messages(&issues)
        );
    }

    #[test]
    fn accepts_a_well_formed_provider_entry() {
        let issues = entry_issues("command: op read op://Private/key\ntarget: ~/.ssh/id_ed25519");
        assert!(issues.is_empty(), "got: {}", messages(&issues));
    }

    #[test]
    fn accepts_a_well_formed_template_entry() {
        let issues = entry_issues(
            "source: r/creds.tpl\ntarget: ~/.gem/credentials\nvars:\n  api_key: op read x",
        );
        assert!(issues.is_empty(), "got: {}", messages(&issues));
    }

    #[test]
    fn a_provider_entry_is_not_asked_for_a_relative_source_path() {
        // The source-path rules (non-empty, relative, no `..`) must not fire for an
        // entry that legitimately has no source.
        let issues = entry_issues("command: cat /etc/hosts\ntarget: ~/.x");
        assert!(
            !issues.iter().any(|i| i.field().ends_with(".source")),
            "got: {}",
            messages(&issues)
        );
    }

    #[test]
    fn rejects_declared_var_missing_from_template() {
        let vars = BTreeMap::from([
            ("api_key".to_string(), "op read a".to_string()),
            ("unused".to_string(), "op read b".to_string()),
        ]);
        let reference = TemplateReference {
            field: "dotfiles[0]".to_string(),
            source: "rubygems/credentials.tpl",
            vars: &vars,
        };

        let issues = validate_template_vars("key: {{ api_key }}", &reference);

        assert_eq!(issues.len(), 1, "got: {}", messages(&issues));
        assert!(issues[0].message().contains("unused"));
        assert!(issues[0].message().contains("rubygems/credentials.tpl"));
    }

    #[test]
    fn accepts_template_where_every_var_is_used() {
        let vars = BTreeMap::from([
            ("x".to_string(), "op read a".to_string()),
            ("y".to_string(), "op read b".to_string()),
        ]);
        let reference = TemplateReference {
            field: "dotfiles[0]".to_string(),
            source: "t.tpl",
            vars: &vars,
        };

        assert!(validate_template_vars("a: {{ x }}\nb: {{ y }}", &reference).is_empty());
    }

    #[test]
    fn template_dotfiles_enumerates_shared_and_environment_templates() {
        let yaml = r#"
name: creds
dotfiles:
  - source: shared.tpl
    target: ~/.shared
    vars:
      a: op read a
  - source: plain.conf
    target: ~/.plain
environments:
  work:
    install: echo i
    dotfiles:
      - source: work.tpl
        target: ~/.work
        vars:
          b: teller get B
"#;
        let package: Package = serde_saphyr::from_str(yaml).unwrap();

        let refs = package.template_dotfiles();
        let sources: Vec<_> = refs.iter().map(|r| r.source).collect();

        assert_eq!(sources, vec!["shared.tpl", "work.tpl"]);
        assert_eq!(refs[0].field, "dotfiles[0]");
        assert_eq!(refs[1].field, "environments.work.dotfiles[0]");
    }

    #[test]
    fn validation_reports_that_apply_executes_commands() {
        let yaml = r#"
name: creds
environments:
  test:
    install: echo i
dotfiles:
  - command: op read op://Private/key
    target: ~/.ssh/id_ed25519
  - source: creds/t.tpl
    target: ~/.gem/credentials
    vars:
      api_key: op read a
      corp: teller get B
"#;
        let package: Package = serde_saphyr::from_str(yaml).unwrap();

        let issues = package.validate_dotfiles();
        let notice = issues
            .iter()
            .find(|i| i.level() == ValidationLevel::Info)
            .expect("expected an informational notice");

        // One whole-file command plus two bindings.
        assert!(
            notice.message().contains("executes 3 command"),
            "got: {}",
            notice.message()
        );
    }

    #[test]
    fn the_apply_time_count_is_the_worst_single_environment_not_the_sum() {
        // One provider entry, overridden on two machines. Any single `selfie
        // apply` runs exactly one command; summing every scoped entry would
        // report three and overstate what the user is agreeing to.
        let yaml = r#"
name: creds
dotfiles:
  - command: op read shared
    target: ~/.creds
environments:
  home:
    install: echo i
    dotfiles:
      - command: op read home
        target: ~/.creds
  work:
    install: echo i
    dotfiles:
      - command: teller get work
        target: ~/.creds
"#;
        let package: Package = serde_saphyr::from_str(yaml).unwrap();

        let issues = package.validate_dotfiles();
        let notice = issues
            .iter()
            .find(|i| i.level() == ValidationLevel::Info)
            .expect("expected an informational notice");

        assert!(
            notice.message().contains("executes 1 command"),
            "got: {}",
            notice.message()
        );
    }

    #[test]
    fn a_package_with_no_apply_time_commands_gets_no_notice() {
        let package = PackageBuilder::default()
            .name("bat")
            .dotfiles(vec![DotfileEntry::new(
                "bat/config",
                "~/.config/bat/config",
            )])
            .environment("test", |b| b.install("echo i"))
            .build();

        assert!(
            package
                .validate_dotfiles()
                .iter()
                .all(|i| i.level() != ValidationLevel::Info),
        );
    }

    #[test]
    fn the_apply_time_notice_does_not_make_a_package_invalid() {
        let yaml = r#"
name: creds
environments:
  test:
    install: echo i
dotfiles:
  - command: op read op://Private/key
    target: ~/.ssh/id_ed25519
"#;
        let package: Package = serde_saphyr::from_str(yaml).unwrap();

        assert!(package.validate("test").issues().is_valid());
    }

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
    fn test_validate_env_dotfile_empty_source_errors() {
        let package = PackageBuilder::default()
            .name("bad-env-source")
            .environment("work", |b| {
                b.install("echo hi")
                    .dotfiles(vec![DotfileEntry::new("", "~/.config/x")])
            })
            .build();
        let result = package.validate("work");
        assert!(
            result.issues().has_errors(),
            "an environment-specific dotfile with an empty source must be an error"
        );
    }

    #[test]
    fn test_validate_env_dotfile_relative_target_errors() {
        let package = PackageBuilder::default()
            .name("bad-env-target")
            .environment("work", |b| {
                b.install("echo hi")
                    .dotfiles(vec![DotfileEntry::new("x/config", "relative/path")])
            })
            .build();
        let result = package.validate("work");
        assert!(
            result.issues().has_errors(),
            "an environment-specific dotfile with a relative target must be an error"
        );
    }

    #[test]
    fn test_validate_env_dotfile_override_warns_not_errors() {
        let package = PackageBuilder::default()
            .name("override-config")
            .dotfiles(vec![DotfileEntry::new(
                "bat/config",
                "~/.config/bat/config",
            )])
            .environment("work", |b| {
                b.install("echo hi").dotfiles(vec![DotfileEntry::new(
                    "bat/work.config",
                    "~/.config/bat/config",
                )])
            })
            .build();
        let result = package.validate("work");
        assert!(
            !result.issues().has_errors(),
            "overriding a shared dotfile is allowed, not an error"
        );
        assert!(
            result.issues().has_warnings(),
            "an environment overriding a shared dotfile target must be surfaced as a warning"
        );
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
