//! Whether selfie refuses a whole package before it acts on any part of it.

use std::fmt;

use super::{
    DotfileEntry, EnvironmentField, Package, PackageField, SpecOrigin, TopLevelKeys, UnknownKey,
};
use crate::validation::ValidationIssue;

/// Why a package cannot be deployed as it stands.
///
/// [`Display`](fmt::Display) renders the reason as a clause, which a caller
/// naming the package puts after its colon. The fields are carried rather than
/// pre-rendered so a caller reporting structured diagnostics can name the keys.
#[derive(Debug, Clone)]
pub(crate) enum SpecRefusal {
    /// Top-level keys a package does not accept.
    UnknownTopLevelKeys(Vec<UnknownKey>),
    /// Unrecognized keys inside one environment's mapping, with the environment
    /// that carries them.
    UnknownEnvironmentKeys {
        environment: String,
        keys: Vec<UnknownKey>,
    },
    /// The top level could not be read back, carrying the parse failure.
    UncheckedTopLevel(String),
    /// No environment is declared, carrying the issue `spec validate` raises for
    /// it so a caller reporting structured diagnostics states one category and
    /// one field rather than inventing its own.
    NoEnvironments(ValidationIssue),
}

impl SpecRefusal {
    /// The field paths `selfie spec validate` reports this refusal's problems at,
    /// one per problem, so a caller holding its issues can tell whether each is
    /// already reported.
    pub(crate) fn fields(&self) -> Vec<String> {
        match self {
            Self::UnknownTopLevelKeys(keys) => keys.iter().map(|key| key.key.clone()).collect(),
            Self::UnknownEnvironmentKeys { environment, keys } => keys
                .iter()
                .map(|key| super::environment_field(environment, &key.key))
                .collect(),
            // Validate reports an unread top level against the package as a whole.
            Self::UncheckedTopLevel(_) => vec!["package".to_string()],
            Self::NoEnvironments(issue) => vec![issue.field().to_string()],
        }
    }
}

impl fmt::Display for SpecRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTopLevelKeys(keys) => {
                f.write_str(&super::describe_unknown_keys::<PackageField>(keys))
            }
            Self::UnknownEnvironmentKeys { environment, keys } => {
                write!(
                    f,
                    "in environment '{environment}': {}",
                    super::describe_unknown_keys::<EnvironmentField>(keys)
                )
            }
            // The parse failure ends the sentence because it is several lines of
            // source snippet, and anything after it reads as part of it.
            Self::UncheckedTopLevel(error) => write!(
                f,
                "its top-level keys could not be checked, so an unrecognized one -- a \
                 misspelling, or an anchor named after a real field -- cannot be ruled out. The \
                 re-read failed with: {error}"
            ),
            // The remedy travels with the message. Every other reason here ends
            // with what to do about it, and a bare "At least one environment must
            // be defined" leaves a reader to work out where the section goes.
            Self::NoEnvironments(issue) => match issue.suggestion() {
                Some(suggestion) => write!(f, "{}. {suggestion}", issue.message()),
                None => f.write_str(issue.message()),
            },
        }
    }
}

/// An unrecognized key inside a dotfile entry, and where in the file it sits.
#[derive(Debug, Clone)]
pub(crate) struct UnknownEntryKey {
    /// The path naming the key, such as `dotfiles[0]._target` or
    /// `environments.work.dotfiles[1].vrs`.
    pub(crate) field: String,
    /// The key, judged and worded against a dotfile entry's own fields.
    pub(crate) unknown: UnknownKey,
}

impl Package {
    /// Every unrecognized key inside a dotfile entry, at every scope.
    ///
    /// The shared `dotfiles` list first, then each environment's own in name
    /// order. Empty for a programmatically built package, whose entries carry no
    /// keys to judge.
    ///
    /// Reads the entries rather than the raw YAML, so it answers for a package
    /// with no file behind it.
    pub(crate) fn unknown_entry_keys(&self) -> Vec<UnknownEntryKey> {
        // Two consumers share this, which is the point: one reports each key as a
        // diagnostic, and one refuses to rewrite the file. A rewrite serializes
        // from the struct and so destroys every key here, whichever scope holds
        // it -- which is why the walk covers all of them rather than one
        // environment.
        //
        // In name order, because `environments` is a `HashMap` whose iteration
        // order is randomized per process, and both consumers render these in the
        // order they arrive.
        let mut keys = Self::entry_keys_in(&self.dotfiles, None);

        for (name, env) in self.environments_sorted() {
            keys.extend(Self::entry_keys_in(env.dotfiles(), Some(name)));
        }

        keys
    }

    // `environment` names the scope the list sits in, `None` for the shared list.
    fn entry_keys_in(entries: &[DotfileEntry], environment: Option<&str>) -> Vec<UnknownEntryKey> {
        entries
            .iter()
            .enumerate()
            .flat_map(|(i, entry)| entry.unknown_keys().iter().map(move |key| (i, key)))
            // The entry recorded each key already judged, so every one yields an
            // entry here with nothing left to word.
            .map(|(i, unknown)| UnknownEntryKey {
                field: {
                    let entry = super::dotfile_field(i);
                    let entry = match environment {
                        Some(environment) => super::environment_field(environment, &entry),
                        None => entry,
                    };
                    format!("{entry}.{}", unknown.key)
                },
                unknown: unknown.clone(),
            })
            .collect()
    }

    /// Why this file cannot be trusted at any scope, when it cannot.
    ///
    /// The top level, plus the unknown keys in any environment rather than one
    /// named environment.
    ///
    /// Says nothing about the individual entries, which are refused one at a
    /// time where they are read, and nothing about whether an environment is
    /// declared at all, which is a question about deploying.
    pub(crate) fn listing_refusal(&self) -> Option<SpecRefusal> {
        // One answer serves a listing and a rewrite because neither knows which
        // environment matters, so both ask about all of them rather than one.
        //
        // It names the first offending environment in name order, not every one. A
        // file with keys in two environments is refused twice over, once per fix.
        //
        // The two top-level rules stay split around the environment. Grouping them
        // ahead of it reports the unread top level for a file whose environment
        // names the key to fix.
        self.unknown_top_level_keys()
            .or_else(|| self.any_unknown_environment_keys())
            .or_else(|| self.unchecked_top_level())
    }

    // In name order. `environments` is a `HashMap` and its iteration order is
    // randomized per process, so taking whichever came first names a different
    // environment between runs of the same file.
    fn any_unknown_environment_keys(&self) -> Option<SpecRefusal> {
        self.environments_sorted()
            .into_iter()
            .find_map(|(name, env)| {
                let unknown = env.unknown_keys();
                (!unknown.is_empty()).then(|| SpecRefusal::UnknownEnvironmentKeys {
                    environment: name.to_string(),
                    keys: unknown.to_vec(),
                })
            })
    }

    /// Why selfie refuses this whole package in `environment`, when it does.
    ///
    /// `Some` carries the reason to report, and what it costs is the caller's to
    /// decide.
    ///
    /// `None` says this file's top level and the environment's own keys are
    /// usable; it says nothing about the individual entries, which are refused
    /// one at a time where they are read.
    ///
    /// Reads only the package, so a caller that never touches the file system
    /// can ask.
    pub(crate) fn spec_refusal(&self, environment: &str) -> Option<SpecRefusal> {
        // Every reason here lies in the file's top level or in an environment
        // mapping, where there is no entry to attach it to -- which is why the
        // answer covers the package rather than a dotfile.
        //
        // Ordered as a command reads a file: the top level, the environment about
        // to be used, then the top level it could not read back at all. The two
        // top-level rules must stay split around the environment. Grouping them
        // ahead of it puts the unread top level first, which changes the reason a
        // file carrying both reports to the one a reader cannot act on.
        self.unknown_top_level_keys()
            .or_else(|| self.unknown_environment_keys(environment))
            .or_else(|| self.unchecked_top_level())
            .or_else(|| self.no_environments())
    }

    /// Whether [`Package::spec_refusal`] refuses this package in `environment`,
    /// for a caller that needs only the answer: it builds no reason.
    pub(crate) fn is_refused(&self, environment: &str) -> bool {
        // Mirrors `spec_refusal`'s four rules; a test holds the two together.
        matches!(self.top_level_keys(), TopLevelKeys::Checked(keys) if !keys.is_empty())
            || self
                .environments()
                .get(environment)
                .is_some_and(|env| !env.unknown_keys().is_empty())
            || matches!(self.top_level_keys(), TopLevelKeys::Unchecked(_))
            || (self.origin() == SpecOrigin::PackageDirectory
                && self.validate_environments_exists().is_err())
    }

    // Last, so a file that is both missing an environment and carrying a bad key
    // reports the key -- the reason a reader can act on without first guessing
    // which of the two selfie objected to.
    //
    // Only a package spec must declare one. A standalone dotfile spec has no
    // environments because `dotfiles track` writes it that way: it deploys from
    // the shared list and an environment would have nothing to say about it.
    // Refusing those would refuse every dotfile anyone has tracked, and no test
    // in this workspace fails when it does -- the rule is keyed on the origin
    // for that reason and not as a convenience.
    fn no_environments(&self) -> Option<SpecRefusal> {
        if self.origin() != SpecOrigin::PackageDirectory {
            return None;
        }
        self.validate_environments_exists()
            .err()
            .map(SpecRefusal::NoEnvironments)
    }

    // A `configs:` or a `_dotfiles:` anchor leaves the list selfie read empty or
    // short, so a caller checking only for entries would pass over the package in
    // silence (selfie-g199, selfie-jt6m). This is the set `selfie spec validate`
    // errors on, so the two commands answer alike.
    fn unknown_top_level_keys(&self) -> Option<SpecRefusal> {
        match self.top_level_keys() {
            TopLevelKeys::Checked(keys) if !keys.is_empty() => {
                Some(SpecRefusal::UnknownTopLevelKeys(keys.clone()))
            }
            _ => None,
        }
    }

    // Scoped to one environment, because a typo in one a run does not touch
    // cannot affect what it deploys. An unknown key here is not merely ignored:
    // `_dotfiles:` leaves this environment's list empty, so
    // `dotfiles_for_environment` falls back to the shared entry and deploys a
    // file this machine was meant to override.
    fn unknown_environment_keys(&self, environment: &str) -> Option<SpecRefusal> {
        let unknown = self.environments().get(environment)?.unknown_keys();

        (!unknown.is_empty()).then(|| SpecRefusal::UnknownEnvironmentKeys {
            environment: environment.to_string(),
            keys: unknown.to_vec(),
        })
    }

    // A top level nothing looked at can hide either of the keys above, and both
    // change what deploys rather than merely adding noise: a shadowed `dotfiles:`
    // empties the list, so the package deploys nothing while reporting success
    // (selfie-c28, selfie-g199), and a shadowed `environments:` costs the mapping,
    // so a shared entry lands on the target an override was written for
    // (selfie-flsi).
    //
    // Neither can be ruled out here, and what the package still appears to have to
    // deploy does not distinguish them -- the flsi reproducer carries entries and
    // a decoy `environments:` both.
    fn unchecked_top_level(&self) -> Option<SpecRefusal> {
        match self.top_level_keys() {
            TopLevelKeys::Unchecked(error) => Some(SpecRefusal::UncheckedTopLevel(error.clone())),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::Package;
    use crate::package::SpecOrigin;

    // The construct that fails the re-read while the package itself still
    // parses: `serde_json::Value` has no key for a mapping keyed by a sequence.
    const UNREADABLE_TOP_LEVEL: &str = "extra:\n  ? [a, b]\n  : v\n";

    // Loaded the way the repository loads a file, because the answer is derived
    // at `set_source` and a package built any other way carries no keys to judge.
    fn package_from(yaml: &str) -> Package {
        let mut package: Package = crate::yaml::parse(yaml).expect("fixture must parse");
        package.set_source(
            PathBuf::from("/packages/myapp.yml"),
            yaml.to_string(),
            SpecOrigin::PackageDirectory,
        );
        package
    }

    // `is_refused` answers what `spec_refusal` does, rule by rule, for every
    // reason it gives and for a file it accepts.
    #[test]
    fn is_refused_agrees_with_spec_refusal() {
        let env = "    install: \"echo i\"\n";
        let fixtures = [
            format!("name: myapp\nenvironments:\n  work:\n{env}"),
            format!("name: myapp\nconfigs: []\nenvironments:\n  work:\n{env}"),
            format!("name: myapp\nenvironments:\n  work:\n{env}    audt: x\n"),
            format!("name: myapp\nenvironments:\n  home:\n{env}    audt: x\n  work:\n{env}"),
            format!("name: myapp\n{UNREADABLE_TOP_LEVEL}environments:\n  work:\n{env}"),
            "name: myapp\nenvironments: {}\n".to_string(),
        ];
        let mut refused = 0;
        for yaml in &fixtures {
            let package = package_from(yaml);
            let expected = package.spec_refusal("work").is_some();
            assert_eq!(package.is_refused("work"), expected, "{yaml}");
            refused += usize::from(expected);
        }
        assert_eq!(refused, 4, "four fixtures, one per rule, are refused");
        let standalone = standalone_from(TRACKED_STANDALONE);
        assert!(!standalone.is_refused("work") && standalone.spec_refusal("work").is_none());
    }

    // The same file, loaded as `dotfiles track` writes it: no environments, and
    // read back from the dotfiles directory rather than the package directory.
    fn standalone_from(yaml: &str) -> Package {
        let mut package: Package = crate::yaml::parse(yaml).expect("fixture must parse");
        package.set_source(
            PathBuf::from("/dotfiles/gemrc.yml"),
            yaml.to_string(),
            SpecOrigin::DotfilesDirectory,
        );
        package
    }

    // The rendered clause, which is what apply and drift put after their colon.
    fn reason(yaml: &str, environment: &str) -> Option<String> {
        package_from(yaml)
            .spec_refusal(environment)
            .map(|refusal| refusal.to_string())
    }

    const CLEAN: &str = r#"name: myapp
dotfiles:
  - source: myapp/config.toml
    target: ~/.config/myapp/config.toml
environments:
  test:
    install: "echo i"
"#;

    #[test]
    fn a_usable_package_is_not_refused() {
        assert_eq!(reason(CLEAN, "test"), None);
    }

    // `target` is a field of a dotfile entry and not of a package, so the
    // documented `_target: &target …` anchor hides nothing. A check that refused
    // every `_`-prefixed key would break every file using it.
    #[test]
    fn the_documented_target_anchor_is_not_refused() {
        let yaml = r#"_target: &target "~/.config/myapp/config.toml"
name: myapp
dotfiles:
  - source: myapp/config.toml
    target: *target
environments:
  test:
    install: "echo i"
"#;
        assert_eq!(reason(yaml, "test"), None);
    }

    #[test]
    fn a_top_level_key_shadowing_a_real_field_is_refused() {
        let yaml = r#"name: myapp
_dotfiles:
  - source: myapp/config.toml
    target: ~/.config/myapp/config.toml
environments:
  test:
    install: "echo i"
"#;
        let reason = reason(yaml, "test").expect("a key hiding 'dotfiles' must be refused");
        assert!(reason.contains("_dotfiles"), "got: {reason}");
        assert!(reason.contains("misspelling"), "got: {reason}");
    }

    #[test]
    fn a_plain_unrecognized_top_level_key_is_refused() {
        let yaml = r#"name: myapp
configs:
  - source: myapp/config.toml
    target: ~/.config/myapp/config.toml
environments:
  test:
    install: "echo i"
"#;
        let reason = reason(yaml, "test").expect("a key nothing reads must be refused");
        assert!(reason.contains("configs"), "got: {reason}");
    }

    #[test]
    fn a_shadowing_key_in_the_applied_environment_is_refused() {
        let yaml = r#"name: myapp
environments:
  test:
    install: "echo i"
    _dotfiles:
      - source: myapp/config.toml
        target: ~/.config/myapp/config.toml
"#;
        let reason =
            reason(yaml, "test").expect("a key hiding this environment's dotfiles must be refused");
        assert!(
            reason.starts_with("in environment 'test':"),
            "got: {reason}"
        );
        assert!(reason.contains("_dotfiles"), "got: {reason}");
    }

    // A typo in an environment this run does not touch cannot affect what it
    // deploys, so it is not this environment's refusal.
    #[test]
    fn a_shadowing_key_in_another_environment_is_not_refused() {
        let yaml = r#"name: myapp
environments:
  test:
    install: "echo i"
  work:
    install: "echo i"
    _dotfiles:
      - source: myapp/config.toml
        target: ~/.config/myapp/config.toml
"#;
        assert_eq!(reason(yaml, "test"), None);
    }

    #[test]
    fn an_unchecked_file_with_nothing_to_deploy_is_refused() {
        let yaml = format!(
            "name: myapp\n{UNREADABLE_TOP_LEVEL}environments:\n  test:\n    install: \"echo i\"\n"
        );
        let reason =
            reason(&yaml, "test").expect("a file whose top level was never read must be refused");
        assert!(reason.contains("cannot be ruled out"), "got: {reason}");
        assert!(reason.contains("The re-read failed with:"), "got: {reason}");
    }

    // Having entries to deploy does not make the unread top level safe: the key
    // it may hide is what decides whether those entries are the right ones.
    #[test]
    fn an_unchecked_file_with_dotfiles_is_refused() {
        let yaml = format!(
            "name: myapp\n{UNREADABLE_TOP_LEVEL}dotfiles:\n  - source: myapp/config.toml\n    \
             target: ~/.config/myapp/config.toml\nenvironments:\n  test:\n    install: \"echo i\"\n"
        );
        let reason = reason(&yaml, "test")
            .expect("a file with entries and an unread top level must be refused");
        assert!(reason.contains("cannot be ruled out"), "got: {reason}");
    }

    // A package with no file behind it has no key a user could misspell. Left
    // out, the whole answer would depend on a state nothing pins.
    #[test]
    fn a_package_built_in_memory_is_not_refused() {
        assert!(
            Package::new_template("myapp")
                .spec_refusal("test")
                .is_none()
        );
    }

    // What `selfie dotfiles track` writes: a target and a source, and nothing
    // else. It deploys from the shared list, so it declares no environment and
    // never needed one.
    const TRACKED_STANDALONE: &str = r#"name: gemrc
dotfiles:
  - source: gemrc/.gemrc
    target: ~/.gemrc
environments: {}
"#;

    // The control for the origin rule, and the reason the rule is keyed on the
    // origin at all. Refusing this refuses every dotfile anyone has tracked --
    // and when the rule was written without the origin check, the whole
    // workspace suite still passed while the binary refused a real one.
    #[test]
    fn a_tracked_standalone_spec_declaring_no_environment_is_not_refused() {
        let refusal = standalone_from(TRACKED_STANDALONE).spec_refusal("work");
        assert!(
            refusal.is_none(),
            "a spec from the dotfiles directory has no environments by design, got: {}",
            refusal.map_or_else(|| "None".to_string(), |r| r.to_string())
        );
    }

    // The same bytes from the other directory. A package spec that declares no
    // environment cannot be installed in one, which is why `spec validate`
    // already errors on it -- this is apply agreeing with it.
    #[test]
    fn a_package_spec_declaring_no_environment_is_refused() {
        let mut package: Package =
            crate::yaml::parse(TRACKED_STANDALONE).expect("fixture must parse");
        package.set_source(
            PathBuf::from("/packages/gemrc.yml"),
            TRACKED_STANDALONE.to_string(),
            SpecOrigin::PackageDirectory,
        );

        let refusal = package
            .spec_refusal("work")
            .expect("a package spec must declare an environment");
        assert!(
            refusal.to_string().contains("environment"),
            "the reason must name what is missing, got: {refusal}"
        );
    }

    // The remedy travels with the reason, as it does for every other one. A
    // reader told only that an environment must be defined still has to find out
    // where the section goes.
    #[test]
    fn the_missing_environment_carries_its_remedy() {
        let mut package: Package =
            crate::yaml::parse(TRACKED_STANDALONE).expect("fixture must parse");
        package.set_source(
            PathBuf::from("/packages/gemrc.yml"),
            TRACKED_STANDALONE.to_string(),
            SpecOrigin::PackageDirectory,
        );

        let refusal = package
            .spec_refusal("work")
            .expect("a package spec must declare an environment")
            .to_string();
        assert!(
            refusal.contains("Add an 'environments' section"),
            "the reason must say what to do about it, got: {refusal}"
        );
    }

    // An unread top level wins over the missing environment, so a file carrying
    // both reports the one a reader can act on without first working out which
    // of the two selfie objected to.
    #[test]
    fn an_unread_top_level_is_reported_ahead_of_a_missing_environment() {
        let yaml = format!("name: myapp\nenvironments: {{}}\n{UNREADABLE_TOP_LEVEL}");
        let refusal = package_from(&yaml)
            .spec_refusal("work")
            .expect("both rules apply");
        assert!(
            refusal.to_string().contains("could not be checked"),
            "the unread top level must be reported first, got: {refusal}"
        );
    }

    // The order `spec_refusal` composes by hand, and the only pair that makes
    // the hand-composition observable: the two top-level rules cannot both hold
    // of one file, so an unread top level against an environment's own key is
    // where grouping the two top-level rules ahead of the environment changes the
    // answer.
    //
    // Without this, that grouping compiles, passes, and quietly reports the
    // unread top level for a file whose environment names the key to fix.
    #[test]
    fn an_environment_key_is_reported_ahead_of_an_unread_top_level() {
        let yaml = format!(
            "name: myapp\n{UNREADABLE_TOP_LEVEL}environments:\n  test:\n    install: \"echo \
             i\"\n    _dotfiles:\n      - source: myapp/config.toml\n        target: \
             ~/.config/myapp/config.toml\n"
        );
        let refusal = package_from(&yaml)
            .spec_refusal("test")
            .expect("both rules apply");
        assert!(
            refusal.to_string().starts_with("in environment 'test':"),
            "the environment's own key must be reported first, got: {refusal}"
        );
    }

    // The listing asks every environment, and the same ordering holds there: a
    // file whose environment names the key to fix must report that key rather
    // than the top level nothing could read. Grouping the two top-level rules
    // ahead of the environment compiles and passes everything else while
    // reporting the other one. A save shares this answer, so the same ordering
    // decides which error a rewrite reports.
    #[test]
    fn a_listing_reports_an_environment_key_ahead_of_an_unread_top_level() {
        let yaml = format!(
            "name: myapp\n{UNREADABLE_TOP_LEVEL}environments:\n  test:\n    install: \"echo \
             i\"\n    _dotfiles:\n      - source: myapp/config.toml\n        target: \
             ~/.config/myapp/config.toml\n"
        );
        let refusal = package_from(&yaml)
            .listing_refusal()
            .expect("both rules apply");
        assert!(
            refusal.to_string().starts_with("in environment 'test':"),
            "the environment's own key must be reported first, got: {refusal}"
        );
    }

    // `environments` is a `HashMap` whose iteration order is randomized per
    // process, so a listing that took whichever environment came first would
    // name a different one between runs of the same file.
    #[test]
    fn a_listing_names_the_offending_environment_in_name_order() {
        let yaml = r#"name: myapp
environments:
  alpha:
    install: "echo i"
    _dotfiles:
      - source: myapp/a.toml
        target: ~/.config/myapp/a.toml
  zulu:
    install: "echo i"
    _dotfiles:
      - source: myapp/z.toml
        target: ~/.config/myapp/z.toml
"#;
        let refusal = package_from(yaml)
            .listing_refusal()
            .expect("both environments carry a shadowing key");
        assert!(
            refusal.to_string().starts_with("in environment 'alpha':"),
            "got: {refusal}"
        );
    }
}
