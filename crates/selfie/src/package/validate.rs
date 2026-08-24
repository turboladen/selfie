use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_saphyr::Location;

use crate::validation::{ValidationErrorCategory, ValidationIssue, ValidationIssues};

use super::{
    DotfileEntry, EnvironmentField, Package, PackageField, describe_unknown_key, parse_top_level,
    shadows_dotfile_field, unknown_key,
};

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
/// verbatim, and the correctly-spelled declared name then goes unused.
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

/// Flag unrecognized keys at the top level of the parsed mapping.
fn unknown_top_level_keys(
    raw: &std::collections::HashMap<String, serde_json::Value>,
) -> Vec<ValidationIssue> {
    raw.keys()
        .filter_map(|k| {
            let unknown = unknown_key::<PackageField>(k)?;

            // A collision needs different advice from a plain misspelling,
            // for the reason `unknown_dotfile_keys` gives: the key may have
            // been named deliberately, and which remedy applies depends on
            // which reading was meant.
            Some(ValidationIssue::error(
                ValidationErrorCategory::InvalidValue,
                k,
                &unknown.message,
                unknown.shadows.then_some(
                    "Anchors are legal here; only a name matching a top-level field is \
                     refused, because it cannot be told apart from a misspelling of that \
                     field.",
                ),
            ))
        })
        .collect()
}

/// Flag unrecognized keys inside each `environments.<env>` mapping.
///
/// Reads the already-parsed top-level map rather than parsing again, so a file
/// this could not re-read is reported once by the caller instead of twice.
///
/// An environment whose value is not a mapping is left alone: deserializing the
/// package would have failed first, so reaching here means it is one.
fn unknown_environment_keys(
    raw: &std::collections::HashMap<String, serde_json::Value>,
) -> Vec<ValidationIssue> {
    let Some(serde_json::Value::Object(envs)) = raw.get("environments") else {
        return vec![];
    };

    let mut issues = Vec::new();

    for (env_name, env) in envs {
        let Some(env) = env.as_object() else { continue };

        for key in env.keys() {
            let Some(unknown) = unknown_key::<EnvironmentField>(key) else {
                continue;
            };

            issues.push(ValidationIssue::error(
                ValidationErrorCategory::InvalidValue,
                &format!("environments.{env_name}.{key}"),
                &unknown.message,
                unknown.shadows.then_some(
                    "Anchors are legal here; only a name matching a field of this environment \
                     is refused, because it cannot be told apart from a misspelling of that \
                     field.",
                ),
            ));
        }
    }

    issues
}

/// Flag unrecognized keys in a `dotfiles` list, naming the entry carrying each.
///
/// Reads the keys off the entries rather than re-parsing the raw YAML, so this
/// works for a programmatically built `Package` too — unlike the top-level check
/// above, which has no source to read for one.
///
/// `path` names the list (`dotfiles` or `environments.<env>.dotfiles`), matching
/// the field paths `validate_dotfiles` already reports.
fn unknown_dotfile_keys(entries: &[DotfileEntry], path: &str) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();

    for (i, entry) in entries.iter().enumerate() {
        for key in entry.unknown_keys() {
            // A collision needs different advice from a plain misspelling. The
            // key is not unknown — it may have been named deliberately — and
            // which remedy applies depends on which the user meant, so the
            // suggestion explains the rule rather than prescribing one fix.
            let suggestion = if shadows_dotfile_field(key) {
                "Anchors are legal here; only a name matching a field of this entry is refused, \
                 because it cannot be told apart from a misspelling of that field."
            } else {
                "This entry is skipped by 'selfie apply' until the key is corrected or removed."
            };

            issues.push(ValidationIssue::error(
                ValidationErrorCategory::InvalidValue,
                &format!("{path}[{i}].{key}"),
                &describe_unknown_key(key),
                Some(suggestion),
            ));
        }
    }

    issues
}

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
    /// Check the package against every known rule: required fields, URL formats,
    /// environment configurations, command syntax.
    ///
    /// `current_env` is the environment the checks are made relative to. Every
    /// issue found is collected rather than stopping at the first.
    #[must_use]
    pub fn validate(&self, current_env: &str) -> ValidationResult {
        let mut issues = Vec::new();

        issues.extend(self.validate_required_fields());
        issues.extend(self.validate_unknown_fields());
        issues.extend(self.validate_unknown_dotfile_fields());
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
    ///
    /// The exception is an anchor whose name collides with a real top-level
    /// field: `_dotfiles:` cannot be told apart from a misspelling of
    /// `dotfiles:`, and reading it as an anchor leaves the package with no
    /// dotfiles at all. `target` is not a top-level field, so the
    /// documented `_target: &target …` anchor is unaffected.
    ///
    /// Reporting it here is only half the fix. Apply does not run validation, so
    /// the refusal that matters is `handle_apply`'s, which reads
    /// `Package::top_level_keys`. This is what `selfie spec validate`
    /// says about the same file, worded identically through
    /// [`describe_unknown_key_in`](super::describe_unknown_key_in).
    pub(crate) fn validate_unknown_fields(&self) -> Vec<ValidationIssue> {
        // No YAML at all means a programmatically built package, which has no
        // keys a user could misspell. An empty *file* is a different case and
        // still parses, so it does not land here.
        let Some(raw_yaml) = self.raw_yaml() else {
            return vec![];
        };

        // The same parse apply's check is derived from, so the two cannot
        // disagree about whether this file could be read back.
        let raw = match parse_top_level(raw_yaml) {
            Ok(raw) => raw,
            // The package parsed once already, so this re-parse failing means the
            // two views disagree -- a YAML scalar `serde_json::Value` cannot hold,
            // for instance. Say the check did not run; staying quiet here reports
            // a file as clean when nothing looked at it.
            Err(e) => {
                // `Info`, not a warning: nothing is known to be wrong with the
                // file, only unchecked, and `sync push` refuses a package
                // carrying any warning. Blocking a push over a check that did
                // not run would be a worse outcome than the gap it reports.
                return vec![ValidationIssue::info(
                    ValidationErrorCategory::Advisory,
                    "package",
                    &format!("could not re-read the package file to check for unknown keys: {e}"),
                    Some(
                        "Neither the top-level keys nor the keys inside each environment were \
                         checked for this package.",
                    ),
                )];
            }
        };

        let mut issues = unknown_top_level_keys(&raw);
        issues.extend(unknown_environment_keys(&raw));

        // `raw` is a `HashMap`, so the iteration order is not the file's. Sort by
        // the field name to keep the report stable between runs.
        issues.sort_by(|a, b| a.field().cmp(b.field()));
        issues
    }

    /// The unrecognized keys at the file's **top level only**.
    ///
    /// Separate from [`validate_unknown_fields`](Self::validate_unknown_fields),
    /// which also walks each environment. A caller that acts on the level -- the
    /// writer picks a different refusal per level -- must not be handed both, or
    /// an environment's key is reported as a top-level one.
    ///
    /// Empty for a package with no source, and for one whose YAML cannot be read
    /// back: that is reported by `validate_unknown_fields`, and a writer must not
    /// refuse over a check that did not run.
    pub(crate) fn unknown_top_level_field_issues(&self) -> Vec<ValidationIssue> {
        let Some(raw_yaml) = self.raw_yaml() else {
            return vec![];
        };
        let Ok(raw) = parse_top_level(raw_yaml) else {
            return vec![];
        };

        let mut issues = unknown_top_level_keys(&raw);
        issues.sort_by(|a, b| a.field().cmp(b.field()));
        issues
    }

    /// Flag unrecognized keys inside dotfile entries, shared and per-environment.
    ///
    /// Separate from [`validate_unknown_fields`](Self::validate_unknown_fields)
    /// because the two read from different places: top-level keys come from the
    /// raw YAML, dotfile keys from the entries themselves.
    pub(crate) fn validate_unknown_dotfile_fields(&self) -> Vec<ValidationIssue> {
        let mut issues = unknown_dotfile_keys(&self.dotfiles, "dotfiles");

        for (env_name, env) in self.environments_sorted() {
            issues.extend(unknown_dotfile_keys(
                env.dotfiles(),
                &format!("environments.{env_name}.dotfiles"),
            ));
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
        /// Whether `name` is usable as a package name.
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

    /// Check that environments are properly configured: that `current_env` is
    /// defined, and that install commands are present.
    ///
    /// Every issue found is collected rather than stopping at the first.
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
        for (env_name, env_config) in self.environments_sorted() {
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
        // Sorted for the same reason the other walks are: every environment is
        // checked, and hash order would vary the report between runs.
        self.validate_command_syntax_for(
            self.environments_sorted().into_iter().map(|(name, _)| name),
        )
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
    /// - Target path that is not absolute and doesn't start with `~/` (error)
    /// - Target path using the `~user/…` form, which selfie does not resolve (error)
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
        for (env_name, env) in self.environments_sorted() {
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

    /// Note how many commands `selfie apply` will run for this package's
    /// dotfiles.
    ///
    /// Informational rather than a warning, and it counts the worst case across
    /// environments rather than the sum.
    // `selfie apply` executes commands taken from the package file rather than
    // only copying data out of it, which makes a package directory code and not
    // data for anyone deciding whether to trust one.
    //
    // Informational, because a package using `vars` correctly would warn on every
    // validation, which is how warnings come to be ignored.
    //
    // Worst case, not a sum over `dotfiles_with_scope`, which would report three
    // commands for one provider entry overridden on two machines. Counts what
    // each entry declares, not `command_count`, which answers apply's question.
    fn report_apply_time_commands(&self) -> Vec<ValidationIssue> {
        // What the entry declares: a whole-file `command`, plus one per binding.
        // A malformed entry carrying both is counted honestly as both.
        fn declared(entry: &DotfileEntry) -> usize {
            usize::from(entry.command().is_some()) + entry.vars().len()
        }

        let count_of = |entries: &[DotfileEntry]| -> usize { entries.iter().map(declared).sum() };

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
    ///
    /// Selects on what the entry looks like: a `source` to read plus at least one
    /// name to check. Unknown keys and a redundant `command` are not
    /// disqualifying. An entry with `command` and `vars` but no `source` is
    /// excluded, having no template to read, as is one whose bindings all arrived
    /// under a colliding anchor and left `vars` empty.
    // Reads those fields directly. Do not route this through
    // `DotfileEntry::content_source`: that answers apply's question, may this
    // entry be deployed, so one defect making an entry undeployable would
    // suppress this check for every other var in the same entry and report one
    // problem in a file that has two. Validation has to report everything wrong
    // at once — a validator revealing defects a round at a time invites the
    // half-finished edit it exists to catch, in a tool that writes to real
    // environment configs. `validate_dotfile_entry` reads the same fields
    // directly, for the same reason.
    #[must_use]
    pub fn template_dotfiles(&self) -> Vec<TemplateReference<'_>> {
        // A `source` to read plus at least one name to check.
        fn reference<'a>(entry: &'a DotfileEntry, field: String) -> Option<TemplateReference<'a>> {
            let source = entry.source()?;
            (!entry.vars().is_empty()).then(|| TemplateReference {
                field,
                source,
                vars: entry.vars(),
            })
        }

        let mut refs = Vec::new();

        for (i, entry) in self.dotfiles.iter().enumerate() {
            refs.extend(reference(entry, format!("dotfiles[{i}]")));
        }

        for (env_name, env) in self.environments_sorted() {
            for (i, entry) in env.dotfiles().iter().enumerate() {
                refs.extend(reference(
                    entry,
                    format!("environments.{env_name}.dotfiles[{i}]"),
                ));
            }
        }

        refs
    }

    /// Structural checks for a single dotfile entry. `field` prefixes the
    /// diagnostics (e.g. `dotfiles[0]` or `environments.work.dotfiles[1]`).
    fn validate_dotfile_entry(dotfile: &DotfileEntry, field: &str) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();
        let target = dotfile.target();

        // Each diagnostic points at the field it is about, so a package with
        // several entries does not make the reader count list items. `target` is
        // the fallback: it is the one field every entry has.
        let target_loc = location_string(dotfile.target_location());
        let source_loc = dotfile.source_location().and_then(location_string);
        let command_loc = dotfile.command_location().and_then(location_string);
        let entry_loc = source_loc
            .clone()
            .or_else(|| command_loc.clone())
            .or_else(|| target_loc.clone());

        // Content-source shape. All of these checks are offline: validation must
        // work without network access and must never trigger an authentication
        // prompt, so no binding is ever executed here.
        match (dotfile.source(), dotfile.command()) {
            (Some(_), Some(_)) => issues.push(ValidationIssue::error_at(
                ValidationErrorCategory::InvalidValue,
                field,
                "Dotfile sets both 'source' and 'command'; exactly one is required",
                Some(
                    "Use 'source' for a file in the repository, or 'command' for content produced \
                     by a command.",
                ),
                command_loc.clone(),
            )),
            (None, None) => issues.push(ValidationIssue::error_at(
                ValidationErrorCategory::RequiredField,
                field,
                "Dotfile sets neither 'source' nor 'command'; exactly one is required",
                Some(
                    "Use 'source' for a file in the repository, or 'command' for content produced \
                     by a command.",
                ),
                entry_loc.clone(),
            )),
            (None, Some(_)) if !dotfile.vars().is_empty() => {
                issues.push(ValidationIssue::error_at(
                    ValidationErrorCategory::InvalidValue,
                    &format!("{field}.vars"),
                    "Dotfile sets both 'command' and 'vars', but there is no template to render",
                    Some(
                        "Drop 'vars', or replace 'command' with a 'source' template that \
                         references them.",
                    ),
                    command_loc.clone(),
                ));
            }
            _ => {}
        }

        // The same predicate `DotfileEntry::content_source` refuses on, reported
        // here with a field path and a suggestion. Both must stay in step, or
        // `selfie spec validate` and `selfie apply` would disagree about which
        // names are usable; `var_name_rule_matches_the_content_source_refusal`
        // holds them together.
        for name in dotfile.vars().keys() {
            if !crate::dotfile_service::template::is_valid_name(name) {
                issues.push(ValidationIssue::error_at(
                    ValidationErrorCategory::InvalidValue,
                    &format!("{field}.vars"),
                    &format!("Dotfile var name '{name}' is not valid"),
                    Some("Names must match [A-Za-z_][A-Za-z0-9_]*."),
                    // `entry_loc`, not `command_loc`: a `vars` block belongs to a
                    // template, which has a `source` and no `command`, so the
                    // command location is `None` for every entry this fires on.
                    entry_loc.clone(),
                ));
            }
        }

        // Source-path checks apply only to entries that have a source. A provider
        // entry's content comes from a command, so there is no path to check.
        if let Some(source) = dotfile.source() {
            if source.is_empty() {
                issues.push(ValidationIssue::error_at(
                    ValidationErrorCategory::InvalidValue,
                    &format!("{field}.source"),
                    "Dotfile source path cannot be empty",
                    Some("Provide a relative path to the dotfile within the repository."),
                    source_loc.clone(),
                ));
            }

            if std::path::Path::new(source)
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                issues.push(ValidationIssue::error_at(
                    ValidationErrorCategory::InvalidValue,
                    &format!("{field}.source"),
                    "Dotfile source path must not contain '..' (path traversal)",
                    Some("Use a relative path without parent directory references."),
                    source_loc.clone(),
                ));
            }

            if source.starts_with('/') || source.starts_with('~') {
                issues.push(ValidationIssue::error_at(
                    ValidationErrorCategory::InvalidValue,
                    &format!("{field}.source"),
                    "Dotfile source path must be relative",
                    Some("Use a path relative to the dotfiles directory, e.g., 'pkg/config.toml'."),
                    source_loc.clone(),
                ));
            }
        }

        // The textual half of the rule `deploy_target` applies, reported here with
        // a field path and a suggestion. One function decides, so a spec cannot
        // pass validation with a target apply refuses *on the text* -- which is
        // what let `~alice/.gemrc` pass and be silently skipped (selfie-jlum).
        //
        // Only the textual half: `TargetRejection::NoHome` is machine state, so
        // `~/x` validates here and can still be refused at deploy time on a
        // machine with no determinable home directory.
        // `the_validator_matches_the_textual_rule` holds the two in step.
        if let Some(rejection) = crate::fs::TargetRejection::of(target) {
            issues.push(ValidationIssue::error_at(
                ValidationErrorCategory::InvalidValue,
                &format!("{field}.target"),
                &format!("Dotfile {}", rejection.message()),
                Some(rejection.suggestion()),
                target_loc.clone(),
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
        package::{DotfileEntry, EnvironmentConfig, KnownFields, builder::PackageBuilder},
        validation::ValidationLevel,
    };

    use super::*;

    fn entry_from_yaml(yaml: &str) -> DotfileEntry {
        serde_saphyr::from_str(yaml).expect("dotfile entry should parse")
    }

    fn entry_issues(yaml: &str) -> Vec<ValidationIssue> {
        Package::validate_dotfile_entry(&entry_from_yaml(yaml), "dotfiles[0]")
    }

    // Parse a whole package file the way the repository does.
    //
    // Deliberately not `PackageBuilder`: a built package has no unrecognized
    // keys to find, so a builder-based fixture would pass whatever the check did.
    fn package_from_yaml(yaml: &str) -> Package {
        serde_saphyr::from_str(yaml).expect("package should parse")
    }

    fn unknown_dotfile_fields(yaml: &str) -> Vec<(String, String)> {
        package_from_yaml(yaml)
            .validate_unknown_dotfile_fields()
            .into_iter()
            .map(|i| (i.field.clone(), i.message.clone()))
            .collect()
    }

    #[test]
    fn a_misspelled_key_in_a_shared_dotfile_is_a_field_level_error() {
        // The package still parses: the typo costs the user one entry, not the
        // whole file. Before selfie-6lz4 was fixed, `var:` failed the entire
        // package to parse and took the second dotfile and the install command
        // with it.
        let package = package_from_yaml(
            "name: creds\nenvironments:\n  test:\n    install: echo i\ndotfiles:\n  \
             - source: creds.tpl\n    target: ~/.creds\n    var:\n      k: op read x\n  \
             - source: bat/config\n    target: ~/.config/bat/config\n",
        );

        assert_eq!(package.dotfiles.len(), 2, "the good entry must survive");
        assert_eq!(
            package.environments.value["test"].install(),
            "echo i",
            "the rest of the package must survive"
        );

        let issues = package.validate_unknown_dotfile_fields();
        assert_eq!(issues.len(), 1, "got: {issues:?}");
        assert_eq!(issues[0].field, "dotfiles[0].var");
        assert!(issues[0].message.contains("var"), "got: {issues:?}");
    }

    #[test]
    fn an_anchor_key_in_a_shared_dotfile_is_not_an_error() {
        // Control for the test above: byte-identical shape, one leading `_`.
        // Without the pair, "no issues" could pass because the walk never ran.
        let package = package_from_yaml(
            "name: creds\nenvironments:\n  test:\n    install: echo i\ndotfiles:\n  \
             - _anchor: &a creds.tpl\n    source: *a\n    target: ~/.creds\n",
        );

        assert_eq!(
            package.dotfiles[0].source(),
            Some("creds.tpl"),
            "the alias must resolve, proving the anchor key was parsed"
        );
        assert!(
            package.validate_unknown_dotfile_fields().is_empty(),
            "got: {:?}",
            package.validate_unknown_dotfile_fields()
        );
    }

    #[test]
    fn a_misspelled_key_in_an_environment_dotfile_is_reported_with_its_environment() {
        // A single environment on purpose: `environments` is a HashMap, so two
        // would make the issue order nondeterministic. Assert on content, never
        // on order, and do not add a second environment here.
        let issues = unknown_dotfile_fields(
            "name: creds\nenvironments:\n  test:\n    install: echo i\n    dotfiles:\n      \
             - source: e.tpl\n        target: ~/.e\n        vasr: 1\n",
        );

        assert_eq!(issues.len(), 1, "got: {issues:?}");
        assert_eq!(issues[0].0, "environments.test.dotfiles[0].vasr");
    }

    #[test]
    fn unknown_dotfile_keys_are_reported_for_a_package_that_was_never_parsed_from_a_file() {
        // `validate_unknown_fields` reads `raw_yaml` and has nothing to read for a
        // package with no source. This check reads the entries instead, so it
        // still works for a package assembled in memory.
        let package = PackageBuilder::default()
            .name("creds")
            .dotfiles(vec![entry_from_yaml(
                "source: creds.tpl\ntarget: ~/.creds\nvar:\n  k: op read x\n",
            )])
            .environment("test", |b| b.install("echo i"))
            .build();

        assert!(
            package.raw_yaml().is_none(),
            "fixture must have no raw YAML"
        );

        let issues = package.validate_unknown_dotfile_fields();
        assert_eq!(issues.len(), 1, "got: {issues:?}");
        assert_eq!(issues[0].field, "dotfiles[0].var");
    }

    #[test]
    fn validate_surfaces_an_unknown_dotfile_key_as_an_error() {
        // Guards the wiring: the check is useless if `validate` does not call it.
        let package = package_from_yaml(
            "name: creds\nenvironments:\n  test:\n    install: echo i\ndotfiles:\n  \
             - source: creds.tpl\n    target: ~/.creds\n    var:\n      k: op read x\n",
        );

        let result = package.validate("test");

        assert!(
            result
                .issues()
                .all_issues()
                .iter()
                .any(|i| i.field == "dotfiles[0].var" && i.level() == ValidationLevel::Error),
            "got: {:?}",
            result.issues()
        );
    }

    #[test]
    fn validate_names_an_anchor_colliding_with_a_dotfile_field() {
        // The kj5y case: `selfie spec validate` reported *nothing* for this, so
        // the entry deployed unrendered with no diagnostic anywhere. The message
        // has to say "rename", not "unknown field" — the key is not unknown, it
        // was named deliberately and happens to collide.
        let package = package_from_yaml(
            "name: creds\nenvironments:\n  test:\n    install: echo i\ndotfiles:\n  \
             - source: creds.tpl\n    target: ~/.creds\n    _vars:\n      k: op read x\n",
        );

        let result = package.validate("test");
        let issue = result
            .issues()
            .all_issues()
            .iter()
            .find(|i| i.field == "dotfiles[0]._vars")
            .unwrap_or_else(|| panic!("got: {:?}", result.issues()));

        assert_eq!(issue.level(), ValidationLevel::Error);
        assert!(
            issue
                .message()
                .contains("rename it, or correct it to 'vars'"),
            "the message must offer the remedy for each reading, got: {}",
            issue.message()
        );
        // The collision is refused precisely because selfie cannot tell an anchor
        // from a typo, so the message must not assert a consequence that holds
        // for only one of them. A genuine `_vars: &v` anchor leaves `vars` unset;
        // a genuine `_target: &t` anchor aliased by `target: *t` does not.
        assert!(
            !issue.message().contains("is not set"),
            "the message must hold for both readings of the key, got: {}",
            issue.message()
        );
    }

    #[test]
    fn validate_names_a_colliding_anchor_in_an_environment_scoped_entry() {
        // Kept separate from the shared case for the same reason the save-refusal
        // tests are: dropping the environments loop from
        // `validate_unknown_dotfile_fields` leaves the shared test green.
        let package = package_from_yaml(
            "name: creds\nenvironments:\n  test:\n    install: echo i\n    dotfiles:\n      \
             - source: creds.tpl\n        target: ~/.creds\n        _source:\n          k: v\n",
        );

        let result = package.validate("test");

        assert!(
            result
                .issues()
                .all_issues()
                .iter()
                .any(|i| i.field == "environments.test.dotfiles[0]._source"
                    && i.level() == ValidationLevel::Error),
            "got: {:?}",
            result.issues()
        );
    }

    #[test]
    fn validate_leaves_an_anchor_not_named_like_a_field_alone() {
        // The control that keeps YAML anchors working. Without it, a fix that
        // flags every `_` key inside an entry passes every test above.
        let package = package_from_yaml(
            "name: creds\nenvironments:\n  test:\n    install: echo i\ndotfiles:\n  \
             - _brew: &a creds.tpl\n    source: *a\n    target: ~/.creds\n",
        );

        let result = package.validate("test");

        assert!(
            !result
                .issues()
                .all_issues()
                .iter()
                .any(|i| i.field.contains("_brew")),
            "an ordinary anchor must not be flagged, got: {:?}",
            result.issues()
        );
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
    fn a_bad_var_name_does_not_suppress_the_unused_var_report() {
        // Two defects in one entry, and validation must report both. If
        // `template_dotfiles` selected on `content_source()`, this entry would not
        // count as a template and the cross-check would never run for the *other*
        // var — so fixing the name and re-running would be the only way to
        // discover the second problem, which is the mistake this tool exists to
        // catch rather than to stage.
        let package = package_from_yaml(
            "name: creds\nenvironments:\n  test:\n    install: echo i\ndotfiles:\n  \
             - source: creds/t.tpl\n    target: ~/.creds\n    vars:\n      \
             \"not-a-name\": op read a\n      forgotten: op read b\n",
        );

        // The entry is still enumerated for reading, despite being undeployable.
        let refs = package.template_dotfiles();
        assert_eq!(refs.len(), 1, "got: {refs:?}");
        assert_eq!(refs[0].source, "creds/t.tpl");

        // The bad name is reported on its own account.
        let issues = package.validate("test");
        assert!(
            issues
                .issues()
                .all_issues()
                .iter()
                .any(|i| i.message().contains("'not-a-name' is not valid")),
            "got: {:?}",
            issues.issues()
        );

        // And the cross-check still runs, so the forgotten var is reported too.
        let unused = validate_template_vars("key: {{ api_key }}\n", &refs[0]);
        let messages = messages(&unused);
        assert!(
            messages.contains("'forgotten' is declared but never used"),
            "the other var's problem must not be hidden by the bad name, got: {messages}"
        );
    }

    #[test]
    fn a_valid_but_unused_var_is_still_reported() {
        // The plain case, which already worked and must keep working: a var
        // declared, correctly named, and never referenced — someone who meant to
        // use it and did not finish.
        let package = package_from_yaml(
            "name: creds\nenvironments:\n  test:\n    install: echo i\ndotfiles:\n  \
             - source: creds/t.tpl\n    target: ~/.creds\n    vars:\n      \
             forgotten: op read b\n",
        );

        let refs = package.template_dotfiles();
        assert_eq!(refs.len(), 1);
        let messages = messages(&validate_template_vars("nothing here\n", &refs[0]));

        assert!(
            messages.contains("'forgotten' is declared but never used"),
            "got: {messages}"
        );
    }

    #[test]
    fn an_entry_with_no_template_to_read_is_not_enumerated() {
        // The boundary of "looks like a template". `command` with `vars` has no
        // source to cross-check against, and `source` whose only bindings arrived
        // under a colliding anchor has no names to check — `_vars` leaves `vars`
        // empty. Both are reported elsewhere; neither belongs here.
        for yaml in [
            "name: c\nenvironments:\n  test:\n    install: echo i\ndotfiles:\n  \
             - command: op read a\n    target: ~/.c\n    vars:\n      a: op read a\n",
            "name: c\nenvironments:\n  test:\n    install: echo i\ndotfiles:\n  \
             - source: c/t.tpl\n    target: ~/.c\n    _vars:\n      a: op read a\n",
            "name: c\nenvironments:\n  test:\n    install: echo i\ndotfiles:\n  \
             - source: c/plain.conf\n    target: ~/.c\n",
        ] {
            assert!(
                package_from_yaml(yaml).template_dotfiles().is_empty(),
                "should not be enumerated: {yaml}"
            );
        }
    }

    #[test]
    fn an_entry_with_a_redundant_command_is_still_cross_checked() {
        // `source` + `command` is a shape error, reported separately. There is
        // still a template and still names to check, so suppressing the
        // cross-check would hide a third defect behind the second.
        let package = package_from_yaml(
            "name: c\nenvironments:\n  test:\n    install: echo i\ndotfiles:\n  \
             - source: c/t.tpl\n    command: op read a\n    target: ~/.c\n    vars:\n      \
             forgotten: op read b\n",
        );

        let refs = package.template_dotfiles();
        assert_eq!(refs.len(), 1, "got: {refs:?}");
        assert!(
            messages(&validate_template_vars("nothing\n", &refs[0]))
                .contains("'forgotten' is declared but never used")
        );
    }

    #[test]
    fn the_apply_time_command_notice_survives_a_bad_var_name() {
        // The notice exists so someone deciding whether to trust a package they
        // did not write can see that applying it runs code. Counting via the
        // deploy-path classification gave this package no notice at all, even
        // though it contains a command and would run it the moment the name is
        // corrected.
        let package = package_from_yaml(
            "name: creds\nenvironments:\n  test:\n    install: echo i\ndotfiles:\n  \
             - source: creds/t.tpl\n    target: ~/.creds\n    vars:\n      \
             \"not-a-name\": curl example.com | sh\n",
        );

        let infos = package.validate("test");
        let infos = infos.issues();
        assert!(
            infos
                .infos()
                .iter()
                .any(|i| i.message().contains("executes 1 command(s)")),
            "got: {:?}",
            infos.all_issues()
        );
    }

    #[test]
    fn declared_and_deployable_command_counts_agree() {
        // `report_apply_time_commands` counts declared commands; `command_count`
        // counts what apply would run. They are allowed to differ only for an
        // entry apply refuses — for every valid shape they must agree, or the
        // notice would misreport a package that is perfectly fine.
        for (yaml, expected) in [
            ("source: a.tpl\ntarget: ~/.a\n", 0),
            ("command: op read a\ntarget: ~/.a\n", 1),
            ("source: a.tpl\ntarget: ~/.a\nvars:\n  a: op read a\n", 1),
            (
                "source: a.tpl\ntarget: ~/.a\nvars:\n  a: op read a\n  b: op read b\n",
                2,
            ),
        ] {
            let entry = entry_from_yaml(yaml);
            let declared = usize::from(entry.command().is_some()) + entry.vars().len();

            assert_eq!(entry.command_count(), expected, "for {yaml}");
            assert_eq!(declared, expected, "declared count disagrees for {yaml}");
        }
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
            unknown_keys: Vec::new(),
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

    // selfie-jlum: `~alice/.gemrc` passed `selfie spec validate` and was then
    // silently skipped by apply, because the validator tested `starts_with('~')`
    // and `shellexpand` leaves `~user` alone.
    //
    // Asserts the message names the unsupported form rather than merely that
    // some error exists: this entry has a valid source, so an error here can
    // only come from the target rule, but a future fixture might not be so
    // clean and "has_errors" would then pass for the wrong reason.
    #[test]
    fn a_named_user_target_is_a_validation_error() {
        for target in ["~alice/.gemrc", "~alice"] {
            let package = PackageBuilder::default()
                .name("bad-target")
                .environment("test-env", |b| b.install("echo hi"))
                .dotfiles(vec![DotfileEntry::new("src/file.txt", target)])
                .build();

            let result = package.validate("test-env");
            let issues = result.issues().all_issues();
            let named = issues.iter().any(|issue| issue.message().contains("~user"));

            assert!(
                named,
                "the diagnostic for {target:?} must name the '~user' form, got: {issues:?}"
            );
        }
    }

    // The validator's decision must be the one `TargetRejection::of` gives, or a
    // spec passes validation and then cannot deploy.
    //
    // Named for the *textual* rule: `TargetRejection::NoHome` is machine state
    // rather than spec state, so the validator can only match this half.
    //
    // Catches a call site that stops delegating and reimplements the rule inline,
    // the regression that produced selfie-jlum. It cannot catch a bug inside `of`,
    // because both sides call it -- `a_named_user_target_is_a_validation_error`
    // and the `fs::target` unit tests hold that.
    #[test]
    fn the_validator_matches_the_textual_rule() {
        for target in [
            "/etc/x", "~/x", "~", "~alice", "~alice/x", "rel/x", "", "./x", "../x",
        ] {
            let package = PackageBuilder::default()
                .name("target-rule")
                .environment("test-env", |b| b.install("echo hi"))
                .dotfiles(vec![DotfileEntry::new("src/file.txt", target)])
                .build();

            let refused_by_validate = package
                .validate("test-env")
                .issues()
                .all_issues()
                .iter()
                .any(|issue| issue.field() == "dotfiles[0].target");

            assert_eq!(
                refused_by_validate,
                crate::fs::TargetRejection::of(target).is_some(),
                "validate and the deploy rule disagree about {target:?}"
            );
        }
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

    // A package built from raw YAML, so `raw_yaml` is populated.
    //
    // `PackageBuilder` cannot be used for these: it never sets `raw_yaml`, so
    // `validate_unknown_fields` returns early and the test passes without
    // looking at anything.
    fn package_with_raw_yaml(yaml: &str) -> Package {
        let mut package: Package = serde_saphyr::from_str(yaml).expect("fixture must parse");
        package.set_source(
            std::path::PathBuf::from("/packages/myapp.yml"),
            yaml.to_string(),
        );
        package
    }

    // A dotfile diagnostic points at the line the field was written on. Before
    // this, every one of them rendered `-` in the Location column, so a package
    // with several entries left the reader counting list items.
    #[test]
    fn a_dotfile_diagnostic_carries_the_line_of_the_field_it_is_about() {
        let package = package_with_raw_yaml(
            r#"name: myapp
environments:
  test:
    install: "true"
dotfiles:
  - source: "../escape.conf"
    target: "~/.a"
"#,
        );

        let result = package.validate("test");
        let errors = result.issues().errors();
        let traversal = errors
            .iter()
            .find(|i| i.field() == "dotfiles[0].source")
            .expect("the traversal error must be reported");

        // The exact line, not merely "some location": an off-by-one here sends
        // the reader to the wrong entry, which is worse than no location.
        assert_eq!(
            traversal.location(),
            Some("line 6, column 13"),
            "got: {:?}",
            traversal.location()
        );
    }

    // A `vars` block belongs to a template, which has a `source` and no
    // `command`, so keying this rule off the command location rendered `-` for
    // every entry it can actually fire on.
    #[test]
    fn a_var_name_error_points_at_the_entry_not_a_command_it_does_not_have() {
        let package = package_with_raw_yaml(
            r#"name: myapp
environments:
  test:
    install: "true"
dotfiles:
  - source: creds.tpl
    vars:
      not-a-name: "true"
    target: "~/.creds"
"#,
        );

        let result = package.validate("test");
        let errors = result.issues().errors();
        let var_error = errors
            .iter()
            .find(|i| i.field() == "dotfiles[0].vars")
            .expect("the var-name error must be reported");

        assert_eq!(
            var_error.location(),
            Some("line 6, column 13"),
            "got: {:?}",
            var_error.location()
        );
    }

    // An entry built in memory has no file position, and must render as unknown
    // rather than claiming line 0.
    #[test]
    fn a_built_dotfile_entry_reports_no_location() {
        let package = PackageBuilder::default()
            .name("built")
            .environment("test", |b| b.install("true"))
            .dotfiles(vec![DotfileEntry::new("../escape.conf", "~/.a")])
            .build();

        let result = package.validate("test");
        let errors = result.issues().errors();
        let traversal = errors
            .iter()
            .find(|i| i.field() == "dotfiles[0].source")
            .expect("the traversal error must be reported");

        assert_eq!(traversal.location(), None);
    }

    // A misspelled *optional* key is the silent case. Misspelling `install`
    // instead fails the parse, because that field is required.
    #[test]
    fn a_misspelled_environment_key_is_reported() {
        // Two environments, and the typo is in the second: a walk that stopped at
        // the first would pass a one-environment fixture.
        let package = package_with_raw_yaml(
            r#"name: myapp
environments:
  home:
    install: "echo i"
    audit: "echo a"
  work:
    install: "echo i"
    audt: "echo a"
"#,
        );

        let issues = package.validate_unknown_fields();
        assert_eq!(issues.len(), 1, "got: {issues:?}");
        // Names the environment as well as the key, so a file defining several
        // does not leave the user grepping for which one.
        assert_eq!(issues[0].field, "environments.work.audt");
        // The message has to offer this environment's fields, not the top-level
        // ones: the level is a turbofish at the call site, so the wrong one still
        // compiles, reads as a plausible message and changes no field path.
        assert_eq!(
            issues[0].message,
            "unknown field 'audt'; expected one of: install, check, audit, dependencies, \
             recommends, dotfiles"
        );
    }

    // An anchor under an environment stays legal, and only a name that collides
    // with a real field is refused -- the rule the top level already applies.
    #[test]
    fn an_environment_anchor_is_allowed_unless_it_shadows_a_field() {
        // `_target` is the case that separates the three field lists: legal here
        // and at the top level, refused inside a dotfile entry.
        let package = package_with_raw_yaml(
            r#"name: myapp
environments:
  work:
    install: "echo i"
    _shared: "anything"
    _target: "~/.config/myapp"
    _check: "echo c"
"#,
        );

        let issues = package.validate_unknown_fields();
        assert_eq!(issues.len(), 1, "got: {issues:?}");
        assert_eq!(issues[0].field, "environments.work._check");
        // Both readings, and the remedy for each. Saying the field is unset would
        // be false of a genuine anchor aliased by `check: *c`.
        assert_eq!(
            issues[0].message,
            "'_check' cannot be told apart from a misspelling of the 'check' field; rename it, \
             or correct it to 'check'"
        );
    }

    // A package with no file behind it has no keys to misspell, so the check is
    // silent rather than complaining about a package it cannot read.
    #[test]
    fn a_package_with_no_source_reports_nothing() {
        let package = PackageBuilder::default()
            .name("built")
            .environment("work", |b| b.install("echo i"))
            .build();

        assert!(package.raw_yaml().is_none());
        assert!(package.validate_unknown_fields().is_empty());
    }

    // The check must say when it could not run. Silence here reported a file as
    // clean while nothing had looked at it.
    #[test]
    fn a_package_whose_yaml_cannot_be_re_read_says_so() {
        // A real file, not a hand-set field: the explicit-key mapping under a key
        // `Package` ignores deserializes fine into the struct and not at all into
        // `serde_json::Value`, which has no key for it. `package_with_raw_yaml`
        // parses first, so this fixture proves the two views can disagree.
        let package = package_with_raw_yaml(
            r#"name: myapp
extra:
  ? [a, b]
  : v
environments:
  work:
    install: "echo i"
"#,
        );

        let issues = package.validate_unknown_fields();
        assert_eq!(issues.len(), 1, "got: {issues:?}");
        assert!(
            issues[0].message.contains("could not re-read"),
            "got: {}",
            issues[0].message
        );
        // Not silence, and not an error either: nothing is known to be wrong
        // with the file, only unchecked.
        assert_eq!(issues[0].level, ValidationLevel::Info);
    }

    // Every walk over `environments()` reports in name order. A `HashMap`
    // iterated directly does not: hash order is randomized per process, so the
    // same file lists its problems differently between two runs.
    #[test]
    fn environment_diagnostics_are_reported_in_name_order() {
        let package = package_from_yaml(
            r#"name: myapp
environments:
  charlie:
    install: "echo i"
    dotfiles:
      - source: c.tpl
        target: relative/c
        vars:
          k: echo c
        var:
          k: echo c
  alpha:
    install: "echo i"
    dotfiles:
      - source: a.tpl
        target: relative/a
        vars:
          k: echo a
        var:
          k: echo a
  bravo:
    install: "echo i"
    dotfiles:
      - source: b.tpl
        target: relative/b
        vars:
          k: echo b
        var:
          k: echo b
"#,
        );

        let unknown: Vec<String> = package
            .validate_unknown_dotfile_fields()
            .iter()
            .map(|i| i.field.clone())
            .collect();
        assert_eq!(
            unknown,
            [
                "environments.alpha.dotfiles[0].var",
                "environments.bravo.dotfiles[0].var",
                "environments.charlie.dotfiles[0].var",
            ]
        );

        let templates: Vec<String> = package
            .template_dotfiles()
            .iter()
            .map(|r| r.field.clone())
            .collect();
        assert_eq!(
            templates,
            [
                "environments.alpha.dotfiles[0]",
                "environments.bravo.dotfiles[0]",
                "environments.charlie.dotfiles[0]",
            ]
        );

        // `validate_dotfiles` walks the same map a third time.
        let entries: Vec<String> = package
            .validate_dotfiles()
            .iter()
            .map(|i| i.field.clone())
            .filter(|f| f.starts_with("environments."))
            .collect();
        assert!(
            entries.windows(2).all(|w| w[0] <= w[1]),
            "expected name-sorted fields, got: {entries:?}"
        );
        assert_eq!(entries.len(), 3, "got: {entries:?}");
    }

    // `_dotfiles:` is reported, and the message says what to do about it.
    #[test]
    fn a_top_level_key_shadowing_a_field_is_reported() {
        let package = package_with_raw_yaml(
            r#"name: myapp
_dotfiles:
  - source: "a"
    target: "~/a"
environments:
  test:
    install: "echo hi"
"#,
        );

        let issues = package.validate_unknown_fields();
        assert_eq!(issues.len(), 1, "got: {issues:?}");
        assert_eq!(issues[0].field(), "_dotfiles");
        assert!(
            issues[0]
                .message()
                .contains("cannot be told apart from a misspelling of the 'dotfiles' field"),
            "got: {}",
            issues[0].message()
        );
        assert!(
            issues[0]
                .suggestion()
                .is_some_and(|s| s.contains("Anchors are legal here")),
            "a collision needs the anchors-are-legal advice: {:?}",
            issues[0].suggestion()
        );
    }

    // The documented top-level anchor stays silent.
    //
    // This is the control for the test above: a check that flagged every
    // `_`-prefixed top-level key would satisfy that one and fail this.
    #[test]
    fn a_top_level_anchor_that_shadows_nothing_is_left_alone() {
        let package = package_with_raw_yaml(
            r#"_brew: &brew "brew install ripgrep"
_target: &target "~/.config/bat/config"
name: myapp
environments:
  test:
    install: *brew
dotfiles:
  - source: "bat/config"
    target: *target
"#,
        );

        assert_eq!(
            package.validate_unknown_fields(),
            vec![],
            "the documented `_target: &target` anchor must stay legal"
        );
    }

    // A plain unknown key is still reported, described against the top-level
    // field list rather than a dotfile entry's.
    #[test]
    fn a_plain_unknown_top_level_key_is_still_reported() {
        let package = package_with_raw_yaml(
            r#"name: myapp
configs:
  - a
environments:
  test:
    install: "echo hi"
"#,
        );

        let issues = package.validate_unknown_fields();
        assert_eq!(issues.len(), 1, "got: {issues:?}");
        assert_eq!(issues[0].field(), "configs");
        assert!(
            issues[0].message().contains("post_install_note"),
            "the expected list must be the top-level one: {}",
            issues[0].message()
        );
        assert!(
            issues[0].suggestion().is_none(),
            "a plain misspelling gets no anchor advice"
        );
    }

    // Ensures `PackageField` stays in sync with the `Package` struct.
    //
    // Serializes a fully populated Package to YAML, re-parses as a raw map,
    // and asserts every key has a variant. If this test fails, a field was added
    // to Package without a matching `PackageField` variant.
    #[test]
    fn test_known_fields_matches_package_struct() {
        // Every optional field populated. Most are `skip_serializing_if`, so a
        // field left unset here is a field the reverse direction below cannot
        // see, and a stale entry in the list would survive.
        let full = PackageBuilder::default()
            .name("sync-test")
            .homepage("https://example.com")
            .description("A test")
            .post_install_note("note")
            .dotfiles(vec![DotfileEntry::new("src", "~/.target")])
            .environment("test-env", |b| b.install("echo hi"))
            .build();

        let yaml = serde_saphyr::to_string(&full).unwrap();
        let emitted: std::collections::BTreeSet<String> =
            serde_saphyr::from_str::<std::collections::HashMap<String, serde_json::Value>>(&yaml)
                .unwrap()
                .into_keys()
                .collect();

        // Both directions. Serialized-but-unknown loses detection for that key;
        // known-but-never-serialized leaves a stale entry that quietly excuses a
        // misspelling forever.
        for key in &emitted {
            assert!(
                PackageField::accepts(key),
                "Package serialized '{key}', which is not in PackageField — add the variant"
            );
        }
        for known in PackageField::NAMES {
            assert!(
                emitted.contains(*known),
                "PackageField has '{known}', which no fixture serializes — remove the variant, or widen a fixture to emit it"
            );
        }
    }

    #[test]
    fn known_environment_fields_matches_the_struct() {
        // `check`, `audit`, `recommends` and `dotfiles` are all
        // `skip_serializing_if`, so every one has to be populated or the reverse
        // direction below cannot see it. Built literally rather than through
        // `new`, which forces `dotfiles` empty and so could not emit that key.
        let full = EnvironmentConfig {
            install: "echo install".to_string(),
            check: Some("echo check".to_string()),
            audit: Some("echo audit".to_string()),
            dependencies: vec!["dep".to_string()],
            recommends: vec!["rec".to_string()],
            dotfiles: vec![DotfileEntry::new("src", "~/.t")],
            unknown_keys: Vec::new(),
        };

        let yaml = serde_saphyr::to_string(&full).unwrap();
        let emitted: std::collections::BTreeSet<String> =
            serde_saphyr::from_str::<std::collections::HashMap<String, serde_json::Value>>(&yaml)
                .unwrap()
                .into_keys()
                .collect();

        for key in &emitted {
            assert!(
                EnvironmentField::accepts(key),
                "EnvironmentConfig serialized '{key}', which is not in EnvironmentField"
            );
        }
        for known in EnvironmentField::NAMES {
            assert!(
                emitted.contains(*known),
                "EnvironmentField has '{known}', which no fixture serializes"
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
