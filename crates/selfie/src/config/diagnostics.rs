//! Configuration keys selfie read but did not use.
//!
//! Loading ignores unrecognized keys, so a frontend's section can share the file
//! with the library's settings. A typo or a renamed key is ignored the same way,
//! which is why they are reported.

/// Sections of the configuration file that belong to a frontend rather than to
/// the library, and so are not the library's to report.
///
/// Each frontend reports unknown keys inside its own section; a key here that
/// no frontend claims is invisible, which is the price of the split.
// Exact match, not a prefix walk, and that is only correct because
// `serde_ignored` stops descending once a subtree is ignored: `cli: {a: {b: 1}}`
// reports as the single path `cli`, never `cli.a.b`. Verified by running it, not
// by reading the deserializer. If that ever changes, this becomes a prefix check
// -- and until then, making it one would be a guess dressed as caution.
pub(crate) const FRONTEND_SECTIONS: &[&str] = &["cli"];

/// Renamed keys, and what replaced them.
///
/// Naming the replacement turns a warning someone has to investigate into one
/// they can act on.
// Matched case-insensitively, so `Configs_Directory` gets the same hint. The
// user's own spelling is echoed back rather than the canonical form, so they can
// find the line in their file.
const RENAMED_KEYS: &[(&str, &str)] = &[("configs_directory", "dotfiles_directory")];

/// A key in the configuration file that selfie read and did not use.
///
/// Carries no `Display`: the CLI renders it as a warning with a suggestion, the
/// MCP server as a pair of JSON fields, and `selfie config validate` as a table
/// row. Read it through [`message`](Self::message) and
/// [`suggestion`](Self::suggestion) so those three cannot drift apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoredKey {
    key: String,
    renamed_to: Option<&'static str>,
}

impl IgnoredKey {
    /// Classify `key` as renamed or simply unrecognized.
    #[must_use]
    fn new(key: String) -> Self {
        let lowered = key.to_ascii_lowercase();
        let renamed_to = RENAMED_KEYS
            .iter()
            .find(|(from, _)| *from == lowered)
            .map(|(_, to)| *to);

        Self { key, renamed_to }
    }

    /// The key as it appears in the file, in the user's own spelling.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// What was ignored, and what replaced it when that is known.
    #[must_use]
    pub fn message(&self) -> String {
        match self.renamed_to {
            Some(to) => format!(
                "`{}` was renamed to `{to}` and is no longer read. Selfie ignored it.",
                self.key
            ),
            None => format!(
                "`{}` is not a recognized setting. Selfie ignored it.",
                self.key
            ),
        }
    }

    /// What to do about it.
    #[must_use]
    pub fn suggestion(&self) -> String {
        match self.renamed_to {
            Some(to) => format!("Rename `{}` to `{to}`.", self.key),
            None => format!(
                "Remove `{}`, or check it against the settings in the configuration guide.",
                self.key
            ),
        }
    }
}

/// The keys on the way to an ignored value, outermost first.
///
/// `Option` and newtype wrappers add no key, so they add no segment: a key
/// inside an optional section reads `["cli", "verbos"]`.
// The structured path and not its `Display`: that joins keys with `.`, so a
// top-level key spelled `cli.verbose` and a `verbose` inside `cli:` would
// render the same string.
pub(crate) fn ignored_segments(path: &serde_ignored::Path<'_>) -> Vec<String> {
    use serde_ignored::Path;

    match path {
        Path::Root => Vec::new(),
        Path::Seq { parent, index } => {
            let mut segments = ignored_segments(parent);
            segments.push(index.to_string());
            segments
        }
        Path::Map { parent, key } => {
            let mut segments = ignored_segments(parent);
            segments.push(key.clone());
            segments
        }
        Path::Some { parent }
        | Path::NewtypeStruct { parent }
        | Path::NewtypeVariant { parent } => ignored_segments(parent),
    }
}

/// Turn the paths `serde_ignored` collected into the ones the library owns.
///
/// Drops frontend sections and anything nested inside one — those belong to the
/// frontend that reads them, and reporting them here would fire on `cli:` for
/// every user on every run.
pub(crate) fn library_ignored_keys<I>(paths: I) -> Vec<IgnoredKey>
where
    I: IntoIterator<Item = Vec<String>>,
{
    paths
        .into_iter()
        .filter(|segments| {
            // A nested path cannot occur today -- an ignored subtree is reported
            // as its root -- so this is belt and braces rather than a live case.
            segments
                .first()
                .is_some_and(|top_level| !FRONTEND_SECTIONS.contains(&top_level.as_str()))
        })
        .map(|segments| IgnoredKey::new(segments.join(".")))
        .collect()
}

/// A frontend's own section of the configuration file, plus the keys in it that
/// the frontend did not read.
#[derive(Debug, Clone)]
pub struct SectionLoad<T> {
    value: T,
    ignored_keys: Vec<IgnoredKey>,
}

impl<T> SectionLoad<T> {
    /// The deserialized section.
    pub fn value(self) -> T {
        self.value
    }

    /// Keys inside the section that `T` did not consume.
    #[must_use]
    pub fn ignored_keys(&self) -> &[IgnoredKey] {
        &self.ignored_keys
    }
}

/// A configuration file that loaded, plus anything in it selfie did not use.
///
/// Loading returns diagnostics rather than emitting them: it happens before any
/// event stream exists, and the library has no terminal to write to. Each
/// frontend decides how to show them.
#[derive(Clone)]
pub struct LoadedConfig {
    config: super::SelfieConfig,
    ignored_keys: Vec<IgnoredKey>,
    // The text rather than a parsed tree. A frontend's section is parsed from it
    // through the same entry point as the library's settings, so the two parses
    // cannot disagree about anchors, merge keys or scalars.
    text: String,
}

// Written by hand to leave out the file's text: a debug print of this must not
// put every value in the user's configuration on stderr.
impl std::fmt::Debug for LoadedConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedConfig")
            .field("config", &self.config)
            .field("ignored_keys", &self.ignored_keys)
            .finish_non_exhaustive()
    }
}

// The `cli:` section alone. Every other key is ignored, and filtered out of
// what the frontend is told.
#[derive(serde::Deserialize)]
struct CliOnly<T> {
    cli: Option<T>,
}

impl LoadedConfig {
    pub(crate) fn new(
        config: super::SelfieConfig,
        ignored_keys: Vec<IgnoredKey>,
        text: String,
    ) -> Self {
        Self {
            config,
            ignored_keys,
            text,
        }
    }

    /// Deserialize the `cli:` section.
    ///
    /// `None` when the file has no such section, or the section is empty. The
    /// keys `T` did not consume come back alongside it, so the CLI can report a
    /// misspelling inside its own section the way the library reports one at the
    /// top level.
    ///
    /// # Errors
    ///
    /// [`ParseFailure`](crate::yaml::ParseFailure) if the section is present but
    /// is not shaped like `T` — `cli: true` where a mapping belongs. The rest of
    /// the file is still usable, so a frontend should report this and fall back
    /// to its defaults rather than abort.
    pub fn cli_section<T>(&self) -> Result<Option<SectionLoad<T>>, crate::yaml::ParseFailure>
    where
        T: serde::de::DeserializeOwned,
    {
        // Keys are reported relative to the section, so a key inside it arrives
        // as `verbos` rather than `cli.verbos`. A top-level key spelled
        // `cli.verbose` is one segment, not two, so it is not this section's.
        let mut ignored_keys = Vec::new();
        let parsed: CliOnly<T> = crate::yaml::parse_reporting_ignored(&self.text, |path| {
            if let Some((section, key)) = ignored_segments(path).split_first()
                && section == "cli"
                && !key.is_empty()
            {
                ignored_keys.push(IgnoredKey::new(key.join(".")));
            }
        })?;

        // A bare `cli:` parses as null, which is `None` here: writing an empty
        // section is a legitimate thing to do. `cli: true` still fails above.
        Ok(parsed.cli.map(|value| SectionLoad {
            value,
            ignored_keys,
        }))
    }

    /// The settings themselves.
    #[must_use]
    pub fn config(&self) -> &super::SelfieConfig {
        &self.config
    }

    /// Keys selfie read and did not use. Empty for a clean file.
    #[must_use]
    pub fn ignored_keys(&self) -> &[IgnoredKey] {
        &self.ignored_keys
    }

    /// Take the settings, discarding the diagnostics.
    ///
    /// For a caller that has already reported them, or one that has no way to.
    #[must_use]
    pub fn into_config(self) -> super::SelfieConfig {
        self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each key is a one-segment path; `cli.verbose` here is a key spelled with a
    // dot, not a key inside `cli:`.
    fn top_level(keys: &[&str]) -> Vec<Vec<String>> {
        keys.iter().map(|key| vec![(*key).to_string()]).collect()
    }

    #[test]
    fn an_unrecognized_key_is_reported_generically() {
        let keys = library_ignored_keys(top_level(&["totally_bogus"]));

        assert_eq!(keys.len(), 1);
        assert!(keys[0].message().contains("totally_bogus"));
        assert!(keys[0].message().contains("not a recognized setting"));
        // It must not invent a rename for a key that has none.
        assert!(!keys[0].message().contains("renamed"));
        assert!(!keys[0].suggestion().contains("Rename"));
    }

    #[test]
    fn a_renamed_key_names_its_replacement() {
        let keys = library_ignored_keys(top_level(&["configs_directory"]));

        assert_eq!(keys.len(), 1);
        assert!(keys[0].message().contains("configs_directory"));
        assert!(keys[0].message().contains("dotfiles_directory"));
        assert!(keys[0].suggestion().contains("Rename"));
        // The whole point of the rename table is that this is *not* the generic
        // message, so assert it is absent rather than only that the hint is there.
        assert!(!keys[0].message().contains("not a recognized setting"));
    }

    #[test]
    fn a_renamed_key_is_matched_regardless_of_case() {
        let keys = library_ignored_keys(top_level(&["Configs_Directory"]));

        assert_eq!(keys.len(), 1);
        assert!(
            keys[0].message().contains("dotfiles_directory"),
            "got: {}",
            keys[0].message()
        );
    }

    // The user has to find the line in their own file, so the message quotes
    // what they typed, not the canonical spelling it matched.
    #[test]
    fn a_renamed_key_message_quotes_the_users_own_spelling() {
        let keys = library_ignored_keys(top_level(&["Configs_Directory"]));

        assert_eq!(keys[0].key(), "Configs_Directory");
        assert!(keys[0].message().contains("Configs_Directory"));
        assert!(keys[0].suggestion().contains("Configs_Directory"));
    }

    // Validation must not stop at the first problem.
    #[test]
    fn two_ignored_keys_are_both_reported() {
        let keys = library_ignored_keys(top_level(&["configs_directory", "nonsense"]));

        assert_eq!(keys.len(), 2);
    }

    // The trap this module exists to avoid: `cli:` is a section another frontend
    // reads, so the library must not call it unknown. Without this filter the
    // warning fires for every user on every run.
    #[test]
    fn a_frontend_section_is_not_reported() {
        let keys = library_ignored_keys(top_level(&["cli"]));

        assert!(keys.is_empty());
    }

    #[test]
    fn a_key_nested_inside_a_frontend_section_is_not_reported() {
        let keys = library_ignored_keys(vec![vec!["cli".to_string(), "verbose".to_string()]]);

        assert!(keys.is_empty());
    }

    // A top-level key spelled with a dot belongs to no frontend. Matching on the
    // joined string would read it as `verbose` inside `cli:` and drop it.
    #[test]
    fn a_top_level_key_spelled_with_a_dot_is_reported() {
        let keys = library_ignored_keys(top_level(&["cli.verbose"]));

        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].key(), "cli.verbose");
        assert!(keys[0].message().contains("not a recognized setting"));
    }

    // A misspelled *section* is nobody's, so it is reported — otherwise `clu:`
    // would be silently dropped by both the library and the CLI.
    #[test]
    fn a_misspelled_frontend_section_is_reported() {
        let keys = library_ignored_keys(top_level(&["clu"]));

        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].key(), "clu");
    }

    // Data-driven over the table so a future entry is checked without anyone
    // remembering to add a test: a replacement that is itself unknown would send
    // the user to a key selfie also ignores.
    #[test]
    fn every_rename_target_differs_from_its_source() {
        for (from, to) in RENAMED_KEYS {
            assert_ne!(from, to, "`{from}` cannot be renamed to itself");
            assert!(
                !FRONTEND_SECTIONS.contains(to),
                "`{to}` is a frontend section, not a library setting"
            );
        }
    }
}
