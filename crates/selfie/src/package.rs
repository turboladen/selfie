mod builder;
pub mod event;
pub mod port;
pub mod repository;
pub mod service;
pub mod validate;

/// Re-export read-only git status types used by the package service layer.
pub mod git {
    pub use crate::git::{GitDirectoryStatus, GitFileStatus, GitStatusError, GitStatusProvider};

    #[cfg(any(test, feature = "with_mocks"))]
    pub use crate::git::MockGitStatusProvider;
}

/// Re-export the concrete git adapter under its package-layer name.
pub mod git_adapter {
    pub use crate::git::GixGitAdapter as GixGitStatusProvider;
}

pub use self::builder::{EnvironmentConfigBuilder, PackageBuilder};
pub use self::service::{InstallOptions, PackageService, SpecService};

// Core package entity and related types
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};
use serde_saphyr::{Location, Spanned};

/// Package data for editing operations
///
/// Contains a package and its file metadata for editing workflows.
/// Can represent either an existing package loaded from the repository
/// or a new package template ready for creation.
#[derive(Debug, Clone)]
pub struct GetPackage {
    /// The package content (either loaded or template)
    pub(crate) package: Package,
    /// The file path where the package is/should be stored
    pub(crate) file_path: PathBuf,
    /// Whether this is a new package (true) or existing (false)
    pub(crate) is_new: bool,
}

impl GetPackage {
    /// Create a minimal package template to start a new package from.
    #[must_use]
    pub fn new(name: &str, packages_directory: &std::path::Path) -> Self {
        let file_path = packages_directory.join(format!("{name}.yml"));
        let package = Package::new_template(name);

        Self {
            package,
            file_path,
            is_new: true,
        }
    }

    /// Wrap a package already loaded from the repository, with the path it came
    /// from.
    #[must_use]
    pub fn from_existing(package: Package, file_path: PathBuf) -> Self {
        Self {
            package,
            file_path,
            is_new: false,
        }
    }

    /// Get a reference to the package content
    #[must_use]
    pub fn package(&self) -> &Package {
        &self.package
    }

    /// Get a mutable reference to the package content
    pub fn package_mut(&mut self) -> &mut Package {
        &mut self.package
    }

    /// Get the file path where the package is/should be stored
    #[must_use]
    pub fn file_path(&self) -> &std::path::Path {
        &self.file_path
    }

    /// Whether this is a new package (true) or existing (false)
    #[must_use]
    pub fn is_new(&self) -> bool {
        self.is_new
    }

    /// Consume the `GetPackage` and return the inner `Package`
    #[must_use]
    pub fn into_package(self) -> Package {
        self.package
    }
}

/// Where a dotfile's content comes from.
///
/// `Template` and `Provider` are secret-bearing: their content is produced by
/// running user-supplied commands at apply time, is held only in memory, and is
/// never recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentSource<'a> {
    /// A file in the package repository, copied as-is.
    RepoFile(&'a str),
    /// A repository file rendered by substituting named values.
    Template {
        source: &'a str,
        vars: &'a BTreeMap<String, String>,
    },
    /// A command whose standard output is the entire content.
    Provider(&'a str),
}

/// The single wording for "where this content comes from", shared by every
/// consumer that has to say it: apply's events, `selfie dotfiles list`, and the
/// MCP server's human-readable fallback.
///
/// Renders references only — a repository path, a command string, var *names* —
/// never a resolved value. Nothing here runs a command or reads a template, so
/// describing an entry can neither leak a secret nor trigger an authentication
/// prompt.
impl std::fmt::Display for ContentSource<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RepoFile(source) => f.write_str(source),
            Self::Template { source, vars } => {
                let names: Vec<&str> = vars.keys().map(String::as_str).collect();
                write!(f, "{source} (vars: {})", names.join(", "))
            }
            Self::Provider(command) => write!(f, "command: {command}"),
        }
    }
}

/// Why an entry has no content source, and therefore cannot be deployed.
///
/// A malformed entry reaches the deploy path intact, because **apply does not run
/// validation**: package loading splits on parse failures alone
/// (`ListPackagesOutput::valid_packages` is `filter_map(Result::ok)`) and `selfie
/// spec validate` is a separate, advisory command.
///
/// Refusing is the caller's decision and nothing here can force it — `let Ok(x) = …
/// else { continue }` compiles — so **report the refusal**: dropping one leaves a
/// dotfile that never deploys and no diagnostic. Each consumer has a test for that.
///
/// Carries the reason, so a caller can name the key or var at fault. Borrows from
/// the entry: describing a refusal must not allocate, and the strings outlive the
/// call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvalidEntry<'a> {
    /// Neither `source` nor `command` is set, both are, or `command` is
    /// combined with `vars`.
    Shape,
    /// The entry carries keys this type does not model — a misspelling such as
    /// `var:` for `vars:`, or an anchor definition whose name collides with a
    /// real field. See [`DotfileEntry::unknown_keys`].
    UnknownKeys(&'a [String]),
    /// A `vars` name that [`template::render`](crate::dotfile_service) can never
    /// substitute, so the placeholder would deploy verbatim.
    VarName(&'a str),
}

impl std::fmt::Display for InvalidEntry<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Shape => f.write_str(
                "set exactly one of 'source' or 'command', with 'vars' only alongside 'source'",
            ),
            Self::UnknownKeys(keys) => {
                let described: Vec<String> = keys.iter().map(|k| describe_unknown_key(k)).collect();
                f.write_str(&described.join("; "))
            }
            Self::VarName(name) => write!(
                f,
                "dotfile var name '{name}' cannot be substituted, so the placeholder would deploy \
                 verbatim; names must match [A-Za-z_][A-Za-z0-9_]*"
            ),
        }
    }
}

/// Whether `key` is an underscore-prefixed anchor definition whose name
/// collides with one of `known`.
///
/// `_`-prefixed keys are otherwise legal anywhere — that is what lets a package
/// file define YAML anchors without `deny_unknown_fields` rejecting them
/// The collision is the exception: `_vars:` is indistinguishable
/// from a typo for `vars:`, and reading it as an anchor deploys the template
/// *unrendered* with the bindings silently absent.
///
/// Parameterized over the field list rather than hard-coded, because the same
/// rule applies at two levels against **different** lists, and which list is in
/// force is the entire reason a top-level `_target:` is legal while an entry's
/// `_target:` is not.
fn shadows_field(key: &str, known: &[&str]) -> bool {
    key.strip_prefix('_')
        .is_some_and(|rest| known.contains(&rest))
}

/// [`shadows_field`] against a dotfile entry's own fields.
///
/// Applied only inside a dotfile entry. A package's top-level `_target: &target
/// …` is an ordinary anchor — `docs/package-files.md` uses exactly that — and is
/// unaffected by this, because `target` is not a top-level field.
pub(crate) fn shadows_dotfile_field(key: &str) -> bool {
    shadows_field(key, KNOWN_DOTFILE_FIELDS)
}

/// [`shadows_field`] against a package's top-level fields.
///
/// `_dotfiles:` is the case that matters: read as an anchor, the package has no
/// dotfiles at all, so `selfie apply` deploys nothing and reports success. The
/// remedy is the same as for an entry — rename the anchor, or drop the
/// underscore.
///
/// Scoped by [`KNOWN_PACKAGE_FIELDS`](crate::package::validate::KNOWN_PACKAGE_FIELDS),
/// which deliberately **excludes** `target`. That exclusion is what keeps the
/// documented `_target: &target …` anchor legal, and it is load-bearing: adding
/// `target` to that list refuses every package file using the documented
/// pattern.
pub(crate) fn shadows_package_field(key: &str) -> bool {
    shadows_field(key, validate::KNOWN_PACKAGE_FIELDS)
}

/// Say what is wrong with one unrecognized key, and what to do about it.
///
/// Shared by [`InvalidEntry`]'s `Display`,
/// `Package::validate_unknown_dotfile_fields` and
/// `Package::validate_unknown_fields`, so apply and `selfie spec validate`
/// cannot describe the same key differently.
///
/// The collision message must hold for **both** readings of the key, because
/// selfie cannot tell them apart — that ambiguity is the entire reason the key is
/// refused. In particular it must not say the colliding field is unset: that is
/// true of a misspelling, and false of a genuine `_target: &t` anchor aliased by
/// `target: *t`, whose author would be told something untrue about their own file.
/// So it names the ambiguity and gives the remedy for each reading: rename it if
/// it is an anchor, spell it correctly if it is not.
///
/// `known` is the field list in force at the key's level, and it appears in the
/// message, so the reader is told which namespace they are in.
pub(crate) fn describe_unknown_key_in(key: &str, known: &[&str]) -> String {
    if let Some(field) = key.strip_prefix('_').filter(|_| shadows_field(key, known)) {
        format!(
            "'{key}' cannot be told apart from a misspelling of the '{field}' field; \
             rename it, or correct it to '{field}'"
        )
    } else {
        format!(
            "unknown field '{key}'; expected one of: {}",
            known.join(", ")
        )
    }
}

/// [`describe_unknown_key_in`] for a key inside a dotfile entry.
pub(crate) fn describe_unknown_key(key: &str) -> String {
    describe_unknown_key_in(key, KNOWN_DOTFILE_FIELDS)
}

/// Top-level keys of `raw_yaml` whose `_` prefix hides a real field name.
///
/// Returns empty for YAML that is absent or does not parse, matching
/// `validate_unknown_fields`: a file that cannot be parsed is reported as a
/// parse error by the loader, and guessing at its keys here would report the
/// same problem twice in different words.
///
/// Sorted, because `serde_saphyr` hands back a `HashMap` and two runs over the
/// same file must name the keys in the same order — otherwise the warning text
/// varies between runs on Linux, where hash order is not insertion order.
fn shadowing_top_level_keys(raw_yaml: &str) -> Vec<String> {
    if raw_yaml.is_empty() {
        return Vec::new();
    }

    let Ok(raw) = serde_saphyr::from_str::<HashMap<String, serde_json::Value>>(raw_yaml) else {
        return Vec::new();
    };

    let mut keys: Vec<String> = raw
        .into_keys()
        .filter(|key| shadows_package_field(key))
        .collect();
    keys.sort();
    keys
}

/// A dotfile mapping from a content source to a deployment target.
///
/// Exactly one of `source` or `command` is valid, and `vars` accompanies only
/// `source`. [`content_source`](Self::content_source) is the abstraction over
/// them.
///
/// An unrecognized key is recorded in [`unknown_keys`](Self::unknown_keys) rather
/// than dropped, which makes `content_source` report
/// [`InvalidEntry::UnknownKeys`] so apply refuses the entry.
// The fields stay `Option`s rather than collapsing into an enum because `Package`
// is deserialized straight from YAML, and validation has to observe "both set"
// and "neither set" to report them.
//
// Recording unknown keys matters because every field bar `target` is optional, so
// a misspelled one is silently dropped. `var:` for `vars:` would deploy a
// template unrendered -- literal `{{ api_key }}` -- over a credentials target.
//
// `deny_unknown_fields` cannot do this: it rejects `_`-prefixed anchors, and a
// rejected key fails the whole package file to parse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DotfileEntry {
    // No `#[serde(default)]`: `Deserialize` is hand-written below and supplies its
    // own defaults, so the attribute would be inert. `skip_serializing_if` still
    // applies — it is read by the derived `Serialize`.
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    vars: BTreeMap<String, String>,
    target: String,

    /// Keys present in the YAML that are not fields of this struct, excluding
    /// `_`-prefixed anchor definitions. Not serialized: a saved package is
    /// written from the struct, so an unknown key is dropped rather than
    /// round-tripped back into the file.
    #[serde(skip)]
    unknown_keys: Vec<String>,
}

/// Field names a dotfile entry accepts, in the order they are reported.
///
/// The single source of truth for both the deserializer below and
/// `Package::validate_unknown_dotfile_fields`; `test_known_dotfile_fields_matches_struct`
/// checks it against what `DotfileEntry` actually serializes.
pub(crate) const KNOWN_DOTFILE_FIELDS: &[&str] = &["source", "command", "vars", "target"];

/// Hand-written so unrecognized keys can be *recorded* rather than rejected.
///
/// A derived impl offers only "ignore silently" or `deny_unknown_fields`, and
/// both are wrong here — see the type's documentation. Written as a visitor
/// rather than `#[serde(flatten)]` into a catch-all map because flatten forces
/// serde's buffering path, which would foreclose putting `Spanned<T>` on these
/// fields to give dotfile diagnostics source locations; flatten on
/// `Package` had to be backed out for that exact reason in d96b82c.
impl<'de> Deserialize<'de> for DotfileEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{Error, IgnoredAny, MapAccess, Visitor};

        struct EntryVisitor;

        impl<'de> Visitor<'de> for EntryVisitor {
            type Value = DotfileEntry;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a dotfile entry")
            }

            fn visit_map<M>(self, mut map: M) -> Result<DotfileEntry, M::Error>
            where
                M: MapAccess<'de>,
            {
                // Each slot's outer `Option` records only whether the key was
                // seen, so the inner type stays exactly what the field declares.
                // `source` and `command` are therefore `Option<Option<String>>`:
                // deserializing them as bare `String` would reject `source: ~`,
                // which the derived impl accepted as `None`.
                let mut source: Option<Option<String>> = None;
                let mut command: Option<Option<String>> = None;
                let mut vars: Option<BTreeMap<String, String>> = None;
                let mut target: Option<String> = None;
                let mut unknown_keys = Vec::new();

                while let Some(key) = map.next_key::<String>()? {
                    /// Take a field, refusing a second occurrence.
                    ///
                    /// serde-saphyr rejects duplicate mapping keys in the parser
                    /// under its default `DuplicateKeyPolicy::Error`, so this
                    /// branch is unreachable today. It is kept because
                    /// `MapAccess` does not itself promise unique keys: were that
                    /// policy ever relaxed, silently taking the last value is the
                    /// one outcome a dotfile entry must not have.
                    macro_rules! once {
                        ($slot:ident, $name:literal) => {{
                            if $slot.is_some() {
                                return Err(M::Error::duplicate_field($name));
                            }
                            $slot = Some(map.next_value()?);
                        }};
                    }

                    match key.as_str() {
                        "source" => once!(source, "source"),
                        "command" => once!(command, "command"),
                        "vars" => once!(vars, "vars"),
                        "target" => once!(target, "target"),
                        _ => {
                            map.next_value::<IgnoredAny>()?;
                            // `_`-prefixed keys are YAML anchor definitions, not
                            // data. Allowing them is why this is not
                            // `deny_unknown_fields`, matching the rule already
                            // applied to top-level keys.
                            //
                            // The exception is an anchor colliding with a field of
                            // this entry: `_vars:` cannot be told apart from a typo
                            // for `vars:`, and treating it as an anchor deploys the
                            // template unrendered. See `shadows_dotfile_field`.
                            if !key.starts_with('_') || shadows_dotfile_field(&key) {
                                unknown_keys.push(key);
                            }
                        }
                    }
                }

                Ok(DotfileEntry {
                    source: source.flatten(),
                    command: command.flatten(),
                    vars: vars.unwrap_or_default(),
                    target: target.ok_or_else(|| M::Error::missing_field("target"))?,
                    unknown_keys,
                })
            }
        }

        deserializer.deserialize_map(EntryVisitor)
    }
}

impl DotfileEntry {
    /// Create a plain repository-file entry.
    pub fn new(source: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            source: Some(source.into()),
            command: None,
            vars: BTreeMap::new(),
            target: target.into(),
            unknown_keys: Vec::new(),
        }
    }

    /// Keys the YAML carried that this type does not recognize.
    ///
    /// Empty for a programmatically built entry. `_`-prefixed anchor definitions
    /// are not included — they are legal — unless the name collides with a field
    /// of this entry, which is indistinguishable from a misspelling of it. See
    /// `shadows_dotfile_field`.
    pub fn unknown_keys(&self) -> &[String] {
        &self.unknown_keys
    }

    /// Get the repository source path, if this entry has one.
    ///
    /// `None` for a provider entry, whose content comes from a command rather
    /// than from a file in the repository.
    pub fn source(&self) -> Option<&str> {
        self.source.as_deref()
    }

    /// Get the whole-file provider command, if this entry has one.
    pub fn command(&self) -> Option<&str> {
        self.command.as_deref()
    }

    /// Get the variable bindings (name to command). Empty unless this entry is a
    /// template.
    pub fn vars(&self) -> &BTreeMap<String, String> {
        &self.vars
    }

    /// Get the target path (deployment destination, may use `~` for home directory).
    pub fn target(&self) -> &str {
        &self.target
    }

    /// Where this entry's content comes from, or why it has nowhere.
    ///
    /// The **only** accessor the deploy path uses, which is what makes one
    /// fallible constructor enough to put every consumer in front of the invalid
    /// case. See [`InvalidEntry`] for what that does and does not guarantee.
    ///
    /// `source()`, `command()` and `vars()` are not a way around it.
    /// `Package::validate_dotfile_entry` is their only production caller and it
    /// reads them deliberately: reporting "sets both" and "sets neither" needs to
    /// observe the fields separately, which is why they stay `Option`s rather
    /// than collapsing into an enum. Nothing that *deploys* an entry reads them.
    ///
    /// Every check here is offline: it reads the entry and nothing else. Asking
    /// where content comes from must never run a command or touch a file, or
    /// merely listing dotfiles could raise an authentication prompt.
    pub fn content_source(&self) -> Result<ContentSource<'_>, InvalidEntry<'_>> {
        // An unrecognized key means a field was meant and did not land: `var:`
        // for `vars:` leaves a template indistinguishable from a plain repository
        // file, so deploying it would write the *unrendered* template over the
        // target. Refuse the entry rather than deploy the wrong content.
        if !self.unknown_keys.is_empty() {
            return Err(InvalidEntry::UnknownKeys(&self.unknown_keys));
        }

        // A name the renderer cannot substitute makes the entry undeployable
        // whatever else is right about it: `template::render` copies the
        // placeholder verbatim, so the file lands with `{{ not-a-name }}` where
        // the credential belongs. Refusing here rather than in the deploy path
        // means the binding's command — which is a real credential fetch, and can
        // raise a biometric prompt — never runs for a value that provably cannot
        // be used. See selfie-3c5a.
        if let Some(name) = self
            .vars
            .keys()
            .find(|name| !crate::dotfile_service::template::is_valid_name(name))
        {
            return Err(InvalidEntry::VarName(name));
        }

        match (self.source.as_deref(), self.command.as_deref()) {
            (Some(source), None) if !self.vars.is_empty() => Ok(ContentSource::Template {
                source,
                vars: &self.vars,
            }),
            (Some(source), None) => Ok(ContentSource::RepoFile(source)),
            // `vars` with `command` is rejected rather than ignored: silently
            // discarding bindings the user wrote would deploy a file they did not
            // ask for.
            (None, Some(command)) if self.vars.is_empty() => Ok(ContentSource::Provider(command)),
            _ => Err(InvalidEntry::Shape),
        }
    }

    /// How many commands `selfie apply` runs to produce this entry's content.
    ///
    /// Zero for a repository file. One for a provider. One per binding for a
    /// template. Used to report apply-time execution and to say what a dry run is
    /// declining to do.
    pub fn command_count(&self) -> usize {
        match self.content_source() {
            Ok(ContentSource::Provider(_)) => 1,
            Ok(ContentSource::Template { vars, .. }) => vars.len(),
            Ok(ContentSource::RepoFile(_)) => 0,
            // An entry that cannot deploy runs nothing, so the advisory notice
            // counting apply-time commands correctly omits it.
            Err(_) => 0,
        }
    }
}

/// Default value for environments field when missing from YAML.
fn default_environments() -> Spanned<HashMap<String, EnvironmentConfig>> {
    unspanned(HashMap::new())
}

/// Deserialize `Spanned<HashMap<...>>` with a fallback to an empty map when the key is missing.
fn deserialize_environments<'de, D>(
    deserializer: D,
) -> Result<Spanned<HashMap<String, EnvironmentConfig>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<Spanned<HashMap<String, EnvironmentConfig>>>::deserialize(deserializer)
        .map(|opt| opt.unwrap_or_else(|| unspanned(HashMap::new())))
}

/// Create a `Spanned<T>` with `Location::UNKNOWN` for programmatically created values.
pub(crate) fn unspanned<T>(value: T) -> Spanned<T> {
    Spanned {
        value,
        referenced: Location::UNKNOWN,
        defined: Location::UNKNOWN,
    }
}

/// Core package entity representing a package definition
///
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Package {
    /// Package name
    pub(crate) name: Spanned<String>,

    /// Optional homepage URL
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) homepage: Option<Spanned<String>>,

    /// Optional package description
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<Spanned<String>>,

    /// Dotfile mappings (source → target); applies regardless of environment
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) dotfiles: Vec<DotfileEntry>,

    /// Optional note displayed to the user after a fresh install
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) post_install_note: Option<Spanned<String>>,

    /// Map of environment configurations
    #[serde(
        deserialize_with = "deserialize_environments",
        default = "default_environments"
    )]
    pub(crate) environments: Spanned<HashMap<String, EnvironmentConfig>>,

    /// Path to the package file (not serialized/deserialized)
    #[serde(skip)]
    pub(crate) path: PathBuf,

    /// Raw YAML content for validation (e.g., unknown field detection).
    /// Set after deserialization, not serialized. Same pattern as `path`.
    ///
    /// Private rather than `pub(crate)` so a new site cannot assign it and skip
    /// [`set_source`](Self::set_source), which is what derives
    /// `shadowing_top_level_keys` from it. Be precise about what that buys:
    /// Rust private fields are visible to this module **and its descendants**,
    /// so `package::repository::yaml` could still bypass it. It is a compile
    /// error for anything outside `package` — which is where the second
    /// assignment lived — not a guarantee.
    #[serde(skip)]
    raw_yaml: String,

    /// Top-level keys whose `_` prefix hides a misspelling of a real field.
    ///
    /// Derived from `raw_yaml` at load time rather than during deserialization,
    /// which would mean hand-writing `Deserialize` for this whole struct — its
    /// `Spanned` fields and custom environment deserializer included — for a
    /// list two call sites read.
    ///
    /// Empty for a programmatically built package, which has no raw YAML. Same
    /// limit `validate_unknown_fields` already has.
    #[serde(skip)]
    shadowing_top_level_keys: Vec<String>,
}

/// Compare only values, ignoring YAML source locations.
impl PartialEq for Package {
    fn eq(&self, other: &Self) -> bool {
        self.name.value == other.name.value
            && self.homepage.as_ref().map(|s| &s.value) == other.homepage.as_ref().map(|s| &s.value)
            && self.description.as_ref().map(|s| &s.value)
                == other.description.as_ref().map(|s| &s.value)
            && self.dotfiles == other.dotfiles
            && self.post_install_note.as_ref().map(|s| &s.value)
                == other.post_install_note.as_ref().map(|s| &s.value)
            && self.environments.value == other.environments.value
            && self.path == other.path
    }
}

/// Configuration for a specific environment
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvironmentConfig {
    /// Command to install the package
    pub(crate) install: String,

    /// Optional command to check if the package is already installed
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) check: Option<String>,

    /// Optional command to audit the package's installation sources
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) audit: Option<String>,

    /// Dependencies that must be installed before this package
    #[serde(default)]
    pub(crate) dependencies: Vec<String>,

    /// Soft dependencies that are installed after this package but don't cascade failure
    ///
    /// Unlike `dependencies`, a failed recommend does not cause the parent package to fail.
    /// Recommends are installed sequentially after the parent succeeds, with individual
    /// success/failure reported via events.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) recommends: Vec<String>,

    /// Dotfiles deployed only in this environment. An entry whose `target`
    /// matches a shared (top-level) dotfile overrides it; one with a new
    /// `target` is added.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) dotfiles: Vec<DotfileEntry>,
}

impl EnvironmentConfig {
    /// Create a new environment configuration
    #[must_use]
    pub fn new(
        install: String,
        check: Option<String>,
        audit: Option<String>,
        dependencies: Vec<String>,
        recommends: Vec<String>,
    ) -> Self {
        Self {
            install,
            check,
            audit,
            dependencies,
            recommends,
            dotfiles: Vec::new(),
        }
    }

    /// Get the install command for this environment
    #[must_use]
    pub fn install(&self) -> &str {
        &self.install
    }

    /// Get the optional check command for this environment
    #[must_use]
    pub fn check(&self) -> Option<&str> {
        self.check.as_deref()
    }

    /// Get the optional audit command for this environment
    #[must_use]
    pub fn audit(&self) -> Option<&str> {
        self.audit.as_deref()
    }

    /// Get the list of dependencies for this environment
    #[must_use]
    pub fn dependencies(&self) -> &[String] {
        &self.dependencies
    }

    /// Get the list of recommended (soft) dependencies for this environment
    #[must_use]
    pub fn recommends(&self) -> &[String] {
        &self.recommends
    }

    /// Get the environment-specific dotfile mappings
    #[must_use]
    pub fn dotfiles(&self) -> &[DotfileEntry] {
        &self.dotfiles
    }
}

impl Package {
    /// Create a new package with the specified attributes. See `PackageBuilder`.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: String,
        homepage: Option<String>,
        description: Option<String>,
        dotfiles: Vec<DotfileEntry>,
        post_install_note: Option<String>,
        environments: HashMap<String, EnvironmentConfig>,
        path: PathBuf,
    ) -> Self {
        Self {
            name: unspanned(name),
            homepage: homepage.map(unspanned),
            description: description.map(unspanned),
            dotfiles,
            post_install_note: post_install_note.map(unspanned),
            environments: unspanned(environments),
            path,
            raw_yaml: String::new(),
            shadowing_top_level_keys: Vec::new(),
        }
    }

    /// Create a minimal package template: basic metadata and one placeholder
    /// environment.
    #[must_use]
    pub fn new_template(name: &str) -> Self {
        let mut environments = HashMap::new();
        environments.insert(
            "default".to_string(),
            EnvironmentConfig {
                install: format!("# TODO: Add install command for {name}"),
                check: Some(format!("# TODO: Add check command for {name}")),
                audit: None,
                dependencies: Vec::new(),
                recommends: Vec::new(),
                dotfiles: Vec::new(),
            },
        );

        Self {
            name: unspanned(name.to_string()),
            homepage: None,
            description: None,
            dotfiles: Vec::new(),
            post_install_note: None,
            environments: unspanned(environments),
            path: PathBuf::new(), // Will be set by GetPackage::new
            raw_yaml: String::new(),
            shadowing_top_level_keys: Vec::new(),
        }
    }

    /// Record where this package was loaded from, and what its raw YAML says.
    ///
    /// The one way to populate `raw_yaml`, so the derived
    /// [`shadowing_top_level_keys`](Self::shadowing_top_level_keys) cannot fall
    /// out of step with it. Both loaders — the YAML repository and sync's own
    /// parse — go through here.
    ///
    /// Deriving the keys here rather than on demand keeps the YAML parse at load
    /// time: `handle_apply` consults the list once per package inside its loop,
    /// and re-parsing the file there would repeat work the loader already did.
    pub(crate) fn set_source(&mut self, path: PathBuf, raw_yaml: String) {
        self.shadowing_top_level_keys = shadowing_top_level_keys(&raw_yaml);
        self.path = path;
        self.raw_yaml = raw_yaml;
    }

    /// Top-level keys whose `_` prefix hides a misspelling of a real field.
    ///
    /// Non-empty means `selfie apply` refuses the whole package: the file cannot
    /// be read unambiguously, and the keys it does carry may not be the ones its
    /// author meant. See `shadows_package_field`.
    #[must_use]
    pub fn shadowing_top_level_keys(&self) -> &[String] {
        &self.shadowing_top_level_keys
    }

    /// Get the package name
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name.value
    }

    /// Get the optional homepage URL
    #[must_use]
    pub fn homepage(&self) -> Option<&str> {
        self.homepage.as_ref().map(|s| s.value.as_str())
    }

    /// Get the optional package description
    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_ref().map(|s| s.value.as_str())
    }

    /// Get the list of dotfile mappings for this package
    #[must_use]
    pub fn dotfiles(&self) -> &[DotfileEntry] {
        &self.dotfiles
    }

    /// Compute the effective dotfiles for `environment`: the shared (top-level)
    /// entries, with any whose `target` matches an environment-specific entry
    /// replaced by it, plus environment-specific entries with new targets.
    #[must_use]
    pub fn dotfiles_for_environment(&self, environment: &str) -> Vec<DotfileEntry> {
        let env_dotfiles = self
            .environments()
            .get(environment)
            .map(EnvironmentConfig::dotfiles)
            .unwrap_or_default();

        // Shared entries, each replaced by an environment-specific entry with
        // the same target (override) when one exists.
        let mut effective: Vec<DotfileEntry> = self
            .dotfiles
            .iter()
            .map(|shared| {
                env_dotfiles
                    .iter()
                    .find(|env| env.target() == shared.target())
                    .unwrap_or(shared)
                    .clone()
            })
            .collect();

        // Environment-specific entries introducing a new target (presence).
        for env in env_dotfiles {
            if !self.dotfiles.iter().any(|s| s.target() == env.target()) {
                effective.push(env.clone());
            }
        }

        effective
    }

    /// Every dotfile entry this package defines, paired with its scope: `None`
    /// for a shared (top-level) entry, `Some(environment_name)` for an
    /// environment-specific one. Unlike [`Self::dotfiles_for_environment`], this
    /// lists all entries across every environment (overrides appear alongside the
    /// shared entry they override) — useful for inventory, listing, and cleanup.
    #[must_use]
    pub fn dotfiles_with_scope(&self) -> Vec<(Option<&str>, &DotfileEntry)> {
        let mut out: Vec<(Option<&str>, &DotfileEntry)> =
            self.dotfiles.iter().map(|d| (None, d)).collect();

        // Environments in a stable (name-sorted) order for deterministic output.
        let mut env_names: Vec<&String> = self.environments().keys().collect();
        env_names.sort();
        for name in env_names {
            if let Some(env) = self.environments().get(name) {
                for d in env.dotfiles() {
                    out.push((Some(name.as_str()), d));
                }
            }
        }

        out
    }

    /// Add a dotfile mapping to this package.
    ///
    /// Skips the entry if a dotfile with the same target already exists,
    /// preventing duplicate entries from repeated `track-dotfile` calls.
    pub fn add_dotfile(&mut self, entry: DotfileEntry) {
        let already_tracked = self
            .dotfiles
            .iter()
            .any(|existing| existing.target() == entry.target());
        if !already_tracked {
            self.dotfiles.push(entry);
        }
    }

    /// Get the optional post-install note for this package
    #[must_use]
    pub fn post_install_note(&self) -> Option<&str> {
        self.post_install_note.as_ref().map(|s| s.value.as_str())
    }

    /// Get the environment configurations
    #[must_use]
    pub fn environments(&self) -> &HashMap<String, EnvironmentConfig> {
        &self.environments.value
    }

    /// Get the package file path
    #[must_use]
    pub fn path(&self) -> &PathBuf {
        &self.path
    }
}

#[cfg(test)]
mod package_tests {
    use std::path::PathBuf;

    use builder::PackageBuilder;

    use crate::package::port::PackageError;

    use super::*;

    fn entry_from_yaml(yaml: &str) -> DotfileEntry {
        serde_saphyr::from_str(yaml).expect("dotfile entry should parse")
    }

    #[test]
    fn plain_source_entry_is_a_repo_file() {
        let entry = DotfileEntry::new("fnm/init.fish", "~/.config/fish/conf.d/fnm.fish");
        assert_eq!(
            entry.content_source(),
            Ok(ContentSource::RepoFile("fnm/init.fish"))
        );
    }

    #[test]
    fn source_with_vars_is_a_template() {
        let entry = entry_from_yaml(
            r#"
source: rubygems/credentials.tpl
target: ~/.gem/credentials
vars:
  api_key: op read op://Private/rubygems/token
"#,
        );

        match entry.content_source() {
            Ok(ContentSource::Template { source, vars }) => {
                assert_eq!(source, "rubygems/credentials.tpl");
                assert_eq!(vars["api_key"], "op read op://Private/rubygems/token");
            }
            other => panic!("expected Template, got {other:?}"),
        }
    }

    #[test]
    fn command_only_entry_is_a_provider() {
        let entry = entry_from_yaml(
            r#"
command: op read op://Private/ssh-key/private
target: ~/.ssh/id_ed25519
"#,
        );

        assert_eq!(
            entry.content_source(),
            Ok(ContentSource::Provider(
                "op read op://Private/ssh-key/private"
            ))
        );
    }

    #[test]
    fn entry_with_both_source_and_command_is_invalid() {
        let entry = entry_from_yaml(
            r#"
source: a.tpl
command: op read x
target: ~/.x
"#,
        );

        assert_eq!(entry.content_source(), Err(InvalidEntry::Shape));
    }

    #[test]
    fn entry_with_neither_source_nor_command_is_invalid() {
        let entry = entry_from_yaml("target: ~/.x");
        assert_eq!(entry.content_source(), Err(InvalidEntry::Shape));
    }

    #[test]
    fn command_combined_with_vars_is_invalid_rather_than_dropping_the_vars() {
        let entry = entry_from_yaml(
            r#"
command: op read x
target: ~/.x
vars:
  api_key: op read y
"#,
        );

        assert_eq!(
            entry.content_source(),
            Err(InvalidEntry::Shape),
            "bindings must not be silently discarded"
        );
    }

    #[test]
    fn empty_vars_map_leaves_an_entry_a_plain_repo_file() {
        let entry = entry_from_yaml(
            r#"
source: bat/config
target: ~/.config/bat/config
vars: {}
"#,
        );

        assert_eq!(
            entry.content_source(),
            Ok(ContentSource::RepoFile("bat/config"))
        );
    }

    #[test]
    fn a_misspelled_dotfile_key_is_rejected_rather_than_silently_dropped() {
        // Every field but `target` is optional, so a dropped key leaves a valid-
        // looking entry: `var:` for `vars:` turns a template into a plain
        // repository file and deploys it *unrendered* — literal `{{ api_key }}` —
        // over a credentials target.
        //
        // The refusal has to live here, on the entry, rather than only in
        // `Package::validate`: apply never runs validation, so a validation-only
        // check would let the unrendered template deploy.
        let entry = entry_from_yaml(
            "source: creds.tpl\ntarget: ~/.gem/credentials\nvar:\n  api_key: op read x\n",
        );

        assert_eq!(entry.unknown_keys(), ["var"]);
        assert_eq!(
            entry.content_source(),
            Err(InvalidEntry::UnknownKeys(&["var".to_string()])),
            "a misspelled key must not leave a deployable-looking entry"
        );
    }

    #[test]
    fn an_underscore_prefixed_key_is_an_anchor_definition_not_a_misspelling() {
        // The control for the test above, differing only in the leading `_`:
        // `_anchor: &a …` is how a package defines a YAML anchor, and rejecting
        // it is the regression selfie-6lz4 records. Consuming the alias proves
        // the key was really parsed rather than merely tolerated.
        let entry =
            entry_from_yaml("_anchor: &a creds.tpl\nsource: *a\ntarget: ~/.gem/credentials\n");

        assert!(
            entry.unknown_keys().is_empty(),
            "got: {:?}",
            entry.unknown_keys()
        );
        assert_eq!(
            entry.content_source(),
            Ok(ContentSource::RepoFile("creds.tpl")),
            "the alias must resolve to the anchored value"
        );
    }

    #[test]
    fn an_anchor_named_like_a_dotfile_field_is_refused() {
        // `_vars:` cannot be told apart from a typo for `vars:`, and reading it as
        // an anchor leaves a template looking like a plain repository file — which
        // deploys it *unrendered*, placeholders and all, over the target. The
        // whole set of colliding names is checked rather than `_vars` alone: a
        // fix hard-coded to one of them passes a one-name test. See selfie-kj5y.
        for field in KNOWN_DOTFILE_FIELDS {
            let key = format!("_{field}");
            let entry = entry_from_yaml(&format!(
                "{key}: &a creds.tpl\nsource: creds.tpl\ntarget: ~/.gem/credentials\n"
            ));

            assert_eq!(
                entry.unknown_keys(),
                std::slice::from_ref(&key),
                "for {key}"
            );
            assert_eq!(
                entry.content_source(),
                Err(InvalidEntry::UnknownKeys(std::slice::from_ref(&key))),
                "an anchor colliding with '{field}' must not leave a deployable entry"
            );
            assert!(
                entry
                    .content_source()
                    .unwrap_err()
                    .to_string()
                    .contains("cannot be told apart from a misspelling"),
                "the message must name the ambiguity, which is the one thing true of \
                 both readings of the key"
            );
            // A genuine anchor really does set the field it collides with, so a
            // message asserting otherwise would tell that user something false
            // about their own file.
            assert!(
                !entry
                    .content_source()
                    .unwrap_err()
                    .to_string()
                    .contains("is not set"),
                "the refusal must not claim the field is unset"
            );
        }
    }

    #[test]
    fn the_refusal_holds_for_a_genuine_anchor_that_really_does_set_its_field() {
        // The case that constitutes the behavior break, and the one a message
        // asserting "'target' is not set" got wrong: here `_target` is a real
        // anchor and `target: *t` really does set the field from it. Refusing is
        // still correct — nothing in the file distinguishes this from a typo —
        // but the diagnostic must not tell this user something false about their
        // own YAML, or every other diagnostic reads as guesswork.
        let entry = entry_from_yaml(
            "_target: &t \"~/.config/bat/config\"\nsource: bat/config\ntarget: *t\n",
        );

        assert_eq!(
            entry.target(),
            "~/.config/bat/config",
            "the alias must have resolved — otherwise this fixture proves nothing"
        );

        let refusal = entry.content_source().unwrap_err().to_string();
        assert!(
            refusal.contains("cannot be told apart from a misspelling"),
            "got: {refusal}"
        );
        assert!(
            !refusal.contains("is not set"),
            "the field IS set here; claiming otherwise contradicts the user's file: {refusal}"
        );
    }

    #[test]
    fn an_underscore_key_not_named_like_a_dotfile_field_stays_legal() {
        // The control for the test above, and the one that keeps anchors working:
        // only a *collision* is refused. `__vars` is included because its
        // remainder is `_vars`, not `vars` — a rule that strips underscores
        // repeatedly, or matches on "contains", would wrongly refuse it.
        for key in ["_anchor", "_brew", "_targets", "__vars", "_"] {
            let entry = entry_from_yaml(&format!(
                "{key}: &a creds.tpl\nsource: *a\ntarget: ~/.gem/credentials\n"
            ));

            assert!(
                entry.unknown_keys().is_empty(),
                "'{key}' is an ordinary anchor, got: {:?}",
                entry.unknown_keys()
            );
            assert_eq!(
                entry.content_source(),
                Ok(ContentSource::RepoFile("creds.tpl")),
                "'{key}' must still deploy, and its alias must resolve"
            );
        }
    }

    #[test]
    fn a_var_name_that_cannot_be_substituted_is_refused() {
        // `template::render` copies a placeholder it cannot parse verbatim, so
        // this entry would deploy with `{{ not-a-name }}` where the credential
        // belongs — after running the binding's command to fetch a value that is
        // then discarded. Refused here so no command runs. See selfie-3c5a.
        let entry = entry_from_yaml(
            "source: creds.tpl\ntarget: ~/.gem/credentials\nvars:\n  not-a-name: op read x\n",
        );

        assert_eq!(
            entry.content_source(),
            Err(InvalidEntry::VarName("not-a-name"))
        );
        assert_eq!(
            entry.command_count(),
            0,
            "apply runs nothing for it, so the advisory must not promise otherwise"
        );
    }

    #[test]
    fn a_valid_var_name_is_still_a_template() {
        // The control: same shape, one character different in the name. Without
        // it, the test above could pass because the fixture never reached the
        // var-name check at all.
        let entry = entry_from_yaml(
            "source: creds.tpl\ntarget: ~/.gem/credentials\nvars:\n  not_a_name: op read x\n",
        );

        assert!(matches!(
            entry.content_source(),
            Ok(ContentSource::Template { .. })
        ));
        assert_eq!(entry.command_count(), 1);
    }

    #[test]
    fn var_name_rule_matches_the_content_source_refusal() {
        // `Package::validate_dotfile_entry` reports a bad name and
        // `content_source` refuses it; both call `is_valid_name`, and the two must
        // agree or `selfie spec validate` and `selfie apply` would disagree about
        // which names are usable. Checks the *decision*, not the predicate, so
        // moving either check still holds them together.
        for name in ["ok", "_ok", "Ok9", "not-a-name", "1st", "", "a b", "a.b"] {
            let entry = entry_from_yaml(&format!(
                "source: creds.tpl\ntarget: ~/.x\nvars:\n  \"{name}\": op read x\n"
            ));
            let refused_by_apply = matches!(entry.content_source(), Err(InvalidEntry::VarName(_)));

            let package: Package = serde_saphyr::from_str(&format!(
                "name: creds\nenvironments:\n  test:\n    install: echo i\ndotfiles:\n  \
                 - source: creds.tpl\n    target: ~/.x\n    vars:\n      \"{name}\": op read x\n"
            ))
            .expect("fixture must parse");
            let reported_by_validate = package
                .validate("test")
                .issues()
                .all_issues()
                .iter()
                .any(|i| i.message().contains("is not valid"));

            assert_eq!(
                refused_by_apply, reported_by_validate,
                "'{name}': apply refuses={refused_by_apply}, validate reports={reported_by_validate}"
            );
        }
    }

    #[test]
    fn a_plain_entry_round_trips_without_gaining_empty_fields() {
        let entry = DotfileEntry::new("bat/config", "~/.config/bat/config");
        let yaml = serde_saphyr::to_string(&entry).unwrap();

        assert!(!yaml.contains("command"), "got: {yaml}");
        assert!(!yaml.contains("vars"), "got: {yaml}");
        assert_eq!(entry, serde_saphyr::from_str(&yaml).unwrap());
    }

    #[test]
    fn a_provider_entry_round_trips() {
        let entry = entry_from_yaml("command: op read x\ntarget: ~/.x");
        let yaml = serde_saphyr::to_string(&entry).unwrap();

        assert!(!yaml.contains("source"), "got: {yaml}");
        assert_eq!(entry, serde_saphyr::from_str(&yaml).unwrap());
    }

    #[test]
    fn an_explicitly_null_source_parses_rather_than_failing_the_file() {
        // `source: ~` is degenerate but legal, and the derived impl accepted it
        // as `None`. Deserializing the slot as a bare `String` would reject it —
        // and because an entry lives inside a package, that rejection would fail
        // the *whole file*, which is the harm selfie-6lz4 is about.
        for yaml in [
            "source: ~\ntarget: ~/.x\n",
            "source:\ntarget: ~/.x\n",
            "command: ~\ntarget: ~/.x\n",
        ] {
            let entry = entry_from_yaml(yaml);
            assert_eq!(entry.source(), None, "got: {yaml}");
            assert_eq!(entry.command(), None, "got: {yaml}");
            assert_eq!(
                entry.content_source(),
                Err(InvalidEntry::Shape),
                "a null source is still not deployable, got: {yaml}"
            );
        }
    }

    #[test]
    fn a_null_target_is_still_rejected() {
        // The control for the test above: `target` is required and non-optional,
        // so relaxing `source`/`command` must not relax it too.
        assert!(
            serde_saphyr::from_str::<DotfileEntry>("source: a\ntarget: ~\n").is_err(),
            "a null target must not deserialize"
        );
    }

    #[test]
    fn an_unknown_key_is_not_written_back_when_an_entry_is_saved() {
        // `save_package` re-serializes from the struct, so an unrecognized key is
        // dropped rather than round-tripped into the user's file. Asserted so the
        // capture field cannot start leaking into saved packages unnoticed.
        let entry = entry_from_yaml("source: a\ntarget: ~/.x\nvar:\n  k: v\n");
        let yaml = serde_saphyr::to_string(&entry).unwrap();

        assert!(!yaml.contains("var"), "got: {yaml}");
        assert!(
            !yaml.contains("unknown_keys"),
            "the capture field must never serialize, got: {yaml}"
        );
    }

    // Keeps `KNOWN_DOTFILE_FIELDS` in sync with what `DotfileEntry` serializes.
    //
    // Two fixtures, not one: every field but `target` is `skip_serializing_if`, so
    // a single entry can never emit all four keys and a one-fixture version would
    // silently stop covering `command` or `vars`.
    //
    // `docs/package-files.md` documents `_target: &target …` as the way to share a
    // path between entries, and it stays legal only because `target` is not a
    // top-level field. Adding it to `KNOWN_PACKAGE_FIELDS` breaks every such file.
    #[test]
    fn a_top_level_anchor_is_refused_only_when_it_shadows_a_package_field() {
        assert!(shadows_package_field("_dotfiles"));
        assert!(shadows_package_field("_environments"));
        assert!(shadows_package_field("_name"));

        assert!(
            !shadows_package_field("_target"),
            "the documented top-level `_target: &target` anchor must stay legal"
        );
        assert!(!shadows_package_field("_brew"));
        assert!(
            !shadows_package_field("dotfiles"),
            "a real field is not an anchor shadowing itself"
        );
    }

    // The two levels have different field lists, and each says which it means.
    //
    // `_target` shadows inside an entry and does not at the top level; the same
    // key, opposite answers. A single shared list would make one of the two
    // wrong.
    #[test]
    fn the_two_field_levels_answer_differently_for_the_same_key() {
        assert!(shadows_dotfile_field("_target"));
        assert!(!shadows_package_field("_target"));

        assert!(shadows_package_field("_dotfiles"));
        assert!(!shadows_dotfile_field("_dotfiles"));
    }

    // A shadowing key gets the ambiguity message; a plain one gets the field
    // list for its own level.
    #[test]
    fn an_unknown_key_is_described_against_the_list_in_force() {
        let shadowed =
            describe_unknown_key_in("_dotfiles", crate::package::validate::KNOWN_PACKAGE_FIELDS);
        assert!(
            shadowed.contains("cannot be told apart from a misspelling of the 'dotfiles' field"),
            "got: {shadowed}"
        );
        assert!(shadowed.contains("rename it"), "got: {shadowed}");

        let plain =
            describe_unknown_key_in("configs", crate::package::validate::KNOWN_PACKAGE_FIELDS);
        assert!(plain.contains("unknown field 'configs'"), "got: {plain}");
        assert!(
            plain.contains("dotfiles"),
            "the expected-keys list belongs in the message: {plain}"
        );
        assert!(
            !plain.contains("source, command"),
            "a top-level key must not be described against the dotfile field list: {plain}"
        );
    }

    // Derived from raw YAML, and quiet when there is none.
    #[test]
    fn shadowing_keys_are_read_from_the_raw_yaml() {
        let yaml = r#"name: myapp
_dotfiles:
  - source: "a"
    target: "~/a"
_target: &t "~/b"
environments:
  test:
    install: "echo hi"
"#;
        assert_eq!(
            shadowing_top_level_keys(yaml),
            vec!["_dotfiles".to_string()],
            "only the shadowing key, and the legal anchor left alone"
        );

        assert!(shadowing_top_level_keys("").is_empty());
        assert!(
            shadowing_top_level_keys("name: [unclosed").is_empty(),
            "unparsable YAML is the loader's error to report, not this one's"
        );
    }

    // `set_source` keeps the derived list in step with the YAML it came from.
    #[test]
    fn set_source_derives_the_shadowing_keys() {
        let mut package = Package::new_template("myapp");
        assert!(package.shadowing_top_level_keys().is_empty());

        package.set_source(
            PathBuf::from("/packages/myapp.yml"),
            "name: myapp\n_dotfiles: []\n".to_string(),
        );

        assert_eq!(package.shadowing_top_level_keys(), ["_dotfiles"]);
        assert_eq!(package.path(), std::path::Path::new("/packages/myapp.yml"));
    }

    #[test]
    fn test_known_dotfile_fields_matches_struct() {
        let template = entry_from_yaml("source: a.tpl\ntarget: ~/.a\nvars:\n  k: op read x\n");
        let provider = entry_from_yaml("command: op read x\ntarget: ~/.b");

        let mut seen = std::collections::BTreeSet::new();
        for entry in [&template, &provider] {
            let yaml = serde_saphyr::to_string(entry).unwrap();
            let raw: HashMap<String, serde_json::Value> = serde_saphyr::from_str(&yaml).unwrap();
            seen.extend(raw.into_keys());
        }

        for key in &seen {
            assert!(
                KNOWN_DOTFILE_FIELDS.contains(&key.as_str()),
                "DotfileEntry serialized '{key}', which is not in KNOWN_DOTFILE_FIELDS — \
                 add it there and to the deserializer's match arms"
            );
        }
        for known in KNOWN_DOTFILE_FIELDS {
            assert!(
                seen.contains(*known),
                "KNOWN_DOTFILE_FIELDS lists '{known}' but no fixture emits it; \
                 the list or the fixtures are stale"
            );
        }
    }

    #[test]
    fn dotfiles_for_environment_overrides_shared_and_adds_env_specific() {
        let package = PackageBuilder::default()
            .name("x")
            .dotfiles(vec![DotfileEntry::new(
                "bat/config",
                "~/.config/bat/config",
            )])
            .environment("work", |b| {
                b.install("brew install x").dotfiles(vec![
                    // same target as the shared entry -> overrides it (variant)
                    DotfileEntry::new("bat/work.config", "~/.config/bat/config"),
                    // new target -> added for this environment only (presence)
                    DotfileEntry::new("zscaler/work.conf", "~/.config/zscaler/config"),
                ])
            })
            .build();

        let effective = package.dotfiles_for_environment("work");

        assert_eq!(
            effective.len(),
            2,
            "shared bat is overridden in place (not duplicated) and zscaler is added"
        );
        let bat = effective
            .iter()
            .find(|e| e.target() == "~/.config/bat/config")
            .expect("bat entry present");
        assert_eq!(
            bat.source(),
            Some("bat/work.config"),
            "environment-specific source overrides the shared one"
        );
        assert!(
            effective
                .iter()
                .any(|e| e.target() == "~/.config/zscaler/config"
                    && e.source() == Some("zscaler/work.conf")),
            "environment-only dotfile is added"
        );
    }

    #[test]
    fn dotfiles_with_scope_lists_shared_and_environment_entries_labeled() {
        let package = PackageBuilder::default()
            .name("x")
            .dotfiles(vec![DotfileEntry::new(
                "bat/config",
                "~/.config/bat/config",
            )])
            .environment("work", |b| {
                b.install("echo i").dotfiles(vec![
                    // override of the shared target
                    DotfileEntry::new("bat/work.config", "~/.config/bat/config"),
                    // environment-only
                    DotfileEntry::new("zscaler/w.conf", "~/.config/zscaler/config"),
                ])
            })
            .build();

        let scoped = package.dotfiles_with_scope();

        // shared bat + work's override + work's zscaler = 3 entries, all listed
        assert_eq!(scoped.len(), 3);
        assert!(
            scoped
                .iter()
                .any(|(scope, e)| scope.is_none() && e.source() == Some("bat/config")),
            "shared entry labeled as shared (None)"
        );
        assert!(
            scoped
                .iter()
                .any(|(scope, e)| *scope == Some("work") && e.source() == Some("bat/work.config")),
            "environment override labeled with its environment"
        );
        assert!(
            scoped
                .iter()
                .any(|(scope, e)| *scope == Some("work") && e.source() == Some("zscaler/w.conf")),
            "environment-only entry labeled with its environment"
        );
    }

    #[test]
    fn dotfiles_for_environment_unknown_env_returns_shared_only() {
        let package = PackageBuilder::default()
            .name("x")
            .dotfiles(vec![DotfileEntry::new(
                "bat/config",
                "~/.config/bat/config",
            )])
            .environment("home", |b| b.install("brew install x"))
            .build();

        let effective = package.dotfiles_for_environment("nonexistent");

        assert_eq!(effective.len(), 1);
        assert_eq!(effective[0].source(), Some("bat/config"));
    }

    #[test]
    fn test_create_package_node() {
        let package = PackageBuilder::default()
            .name("test-package")
            .environment("test-env", |b| b.install("test install"))
            .build();

        assert_eq!(package.name(), "test-package");
        assert_eq!(package.environments().len(), 1);
        assert_eq!(
            package.environments().get("test-env").unwrap().install,
            "test install"
        );
    }

    #[test]
    fn test_create_package_with_metadata() {
        let package = PackageBuilder::default()
            .name("test-package")
            .homepage("https://example.com")
            .description("Test package description")
            .environment("test-env", |b| b.install("test install"))
            .build();

        assert_eq!(package.name(), "test-package");
        assert_eq!(package.homepage(), Some("https://example.com"));
        assert_eq!(package.description(), Some("Test package description"));
        assert_eq!(package.environments().len(), 1);
    }

    #[test]
    fn test_package_not_found_error_contains_context() {
        let error = PackageError::PackageNotFound {
            name: "test-package".to_string(),
            packages_path: PathBuf::from("/home/user/.config/selfie/packages"),
            files_examined: 15,
            search_patterns: vec![
                "test-package.yml".to_string(),
                "test-package.yaml".to_string(),
            ],
        };

        // Test that the error message contains the package name and path
        let error_message = error.to_string();
        assert!(error_message.contains("test-package"));
        assert!(error_message.contains("/home/user/.config/selfie/packages"));

        // Test that context fields are accessible for debugging
        match error {
            PackageError::PackageNotFound {
                name,
                packages_path,
                files_examined,
                search_patterns,
            } => {
                assert_eq!(name, "test-package");
                assert_eq!(
                    packages_path,
                    PathBuf::from("/home/user/.config/selfie/packages")
                );
                assert_eq!(files_examined, 15);
                assert_eq!(
                    search_patterns,
                    vec!["test-package.yml", "test-package.yaml"]
                );
            }
            _ => panic!("Expected PackageNotFound error"),
        }
    }

    #[test]
    fn test_multiple_packages_found_error_contains_conflicting_paths() {
        let conflicting_paths = vec![
            PathBuf::from("/packages/test-package.yml"),
            PathBuf::from("/packages/test-package.yaml"),
        ];

        let error = PackageError::MultiplePackagesFound {
            name: "test-package".to_string(),
            packages_path: PathBuf::from("/packages"),
            conflicting_paths: conflicting_paths.clone(),
            files_examined: 10,
            search_patterns: vec![
                "test-package.yml".to_string(),
                "test-package.yaml".to_string(),
            ],
        };

        // Test that context information is preserved
        match error {
            PackageError::MultiplePackagesFound {
                name,
                conflicting_paths: paths,
                files_examined,
                search_patterns,
                ..
            } => {
                assert_eq!(name, "test-package");
                assert_eq!(paths, conflicting_paths);
                assert_eq!(paths.len(), 2);
                assert_eq!(files_examined, 10);
                assert_eq!(search_patterns.len(), 2);
            }
            _ => panic!("Expected MultiplePackagesFound error"),
        }
    }

    #[test]
    fn test_environment_not_found_error_provides_suggestions() {
        let available_environments = vec![
            "macos".to_string(),
            "linux".to_string(),
            "windows".to_string(),
        ];

        let error = PackageError::EnvironmentNotFound {
            package_name: "test-package".to_string(),
            environment: "freebsd".to_string(),
            available_environments: available_environments.clone(),
            package_file: PathBuf::from("/packages/test-package.yml"),
        };

        // Test error message content
        let error_message = error.to_string();
        assert!(error_message.contains("freebsd"));
        assert!(error_message.contains("test-package"));

        // Test that available environments are accessible for user suggestions
        match error {
            PackageError::EnvironmentNotFound {
                package_name,
                environment,
                available_environments: envs,
                package_file,
            } => {
                assert_eq!(package_name, "test-package");
                assert_eq!(environment, "freebsd");
                assert_eq!(envs, available_environments);
                assert!(envs.contains(&"macos".to_string()));
                assert!(envs.contains(&"linux".to_string()));
                assert!(envs.contains(&"windows".to_string()));
                assert_eq!(package_file, PathBuf::from("/packages/test-package.yml"));
            }
            _ => panic!("Expected EnvironmentNotFound error"),
        }
    }

    #[test]
    fn test_no_check_command_error_shows_alternatives() {
        let other_envs_with_check = vec!["linux".to_string(), "windows".to_string()];

        let error = PackageError::NoCheckCommand {
            package_name: "test-package".to_string(),
            environment: "macos".to_string(),
            package_file: PathBuf::from("/packages/test-package.yml"),
            other_envs_with_check: other_envs_with_check.clone(),
        };

        // Test error message
        let error_message = error.to_string();
        assert!(error_message.contains("macos"));
        assert!(error_message.contains("test-package"));

        // Test that alternative environments are available for suggestions
        match error {
            PackageError::NoCheckCommand {
                package_name,
                environment,
                package_file,
                other_envs_with_check: envs,
            } => {
                assert_eq!(package_name, "test-package");
                assert_eq!(environment, "macos");
                assert_eq!(package_file, PathBuf::from("/packages/test-package.yml"));
                assert_eq!(envs, other_envs_with_check);
                assert!(envs.contains(&"linux".to_string()));
                assert!(envs.contains(&"windows".to_string()));
            }
            _ => panic!("Expected NoCheckCommand error"),
        }
    }

    #[test]
    fn test_parse_error_contains_file_metadata() {
        use crate::package::port::PackageParseError;
        use std::sync::Arc;

        // Create a realistic parse error
        let parse_error = PackageParseError::YamlParse {
            package_path: PathBuf::from("/packages/broken.yml"),
            source: Arc::new(
                serde_saphyr::from_str::<serde_json::Value>("invalid: yaml: [unclosed")
                    .unwrap_err(),
            ),
        };

        let error = PackageError::ParseError {
            name: "broken-package".to_string(),
            packages_path: PathBuf::from("/packages"),
            failed_file: PathBuf::from("/packages/broken.yml"),
            source: parse_error,
        };

        // Test that file context is available
        match error {
            PackageError::ParseError {
                name,
                packages_path,
                failed_file,
                source,
            } => {
                assert_eq!(name, "broken-package");
                assert_eq!(packages_path, PathBuf::from("/packages"));
                assert_eq!(failed_file, PathBuf::from("/packages/broken.yml"));

                // Verify the parse error is preserved
                match source {
                    PackageParseError::YamlParse { package_path, .. } => {
                        assert_eq!(package_path, PathBuf::from("/packages/broken.yml"));
                    }
                    _ => panic!("Expected YamlParse error"),
                }
            }
            _ => panic!("Expected ParseError"),
        }
    }

    #[test]
    fn test_error_context_can_be_extracted_for_debugging() {
        let error = PackageError::PackageNotFound {
            name: "debug-package".to_string(),
            packages_path: PathBuf::from("/debug/packages"),
            files_examined: 42,
            search_patterns: vec!["debug-package.yml".to_string()],
        };

        // Demonstrate how to extract context for debugging/logging
        let debug_info = match &error {
            PackageError::PackageNotFound {
                name,
                packages_path,
                files_examined,
                search_patterns,
            } => {
                format!(
                    "Package '{}' not found in '{}' after examining {} files with patterns: {:?}",
                    name,
                    packages_path.display(),
                    files_examined,
                    search_patterns
                )
            }
            _ => panic!("Expected PackageNotFound error"),
        };

        assert!(debug_info.contains("debug-package"));
        assert!(debug_info.contains("42 files"));
        assert!(debug_info.contains("debug-package.yml"));
    }

    #[test]
    fn test_error_debug_output_includes_all_context() {
        let error = PackageError::MultiplePackagesFound {
            name: "test-multi".to_string(),
            packages_path: PathBuf::from("/test"),
            conflicting_paths: vec![
                PathBuf::from("/test/test-multi.yml"),
                PathBuf::from("/test/test-multi.yaml"),
            ],
            files_examined: 5,
            search_patterns: vec!["test-multi.yml".to_string(), "test-multi.yaml".to_string()],
        };

        let debug_output = format!("{error:?}");

        // Verify that all context fields appear in debug output
        assert!(debug_output.contains("test-multi"));
        assert!(debug_output.contains("files_examined: 5"));
        assert!(debug_output.contains("search_patterns"));
        assert!(debug_output.contains("conflicting_paths"));
    }
}
