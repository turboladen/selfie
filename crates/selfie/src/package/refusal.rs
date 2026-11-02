//! Whether selfie refuses a whole package before it acts on any part of it.

use std::fmt;

use super::{
    DotfileEntry, EnvironmentField, Package, PackageField, SpecOrigin, TopLevelKeys, UnknownKey,
};
use crate::package::event::{EventSender, RefusalKind, RefusedSpec};
use crate::validation::ValidationIssue;

/// Why a package cannot be deployed as it stands: never empty, holding every
/// reason that applies in report order.
///
/// [`Display`](fmt::Display) renders every reason as one clause, which a caller
/// naming the package puts after its colon. [`SpecRefusal::kind`] is the first
/// reason's kind.
// Every reason rather than the first, so a file with problems at several
// levels is fixed in one pass.
#[derive(Debug, Clone)]
pub(crate) struct SpecRefusal(Vec<RefusalReason>);

/// One reason a package cannot be deployed as it stands.
///
/// [`Display`](fmt::Display) renders the reason as a clause. The fields are
/// carried rather than pre-rendered so a caller reporting structured
/// diagnostics can name the keys.
#[derive(Debug, Clone)]
pub(crate) enum RefusalReason {
    /// Top-level keys a package does not accept.
    UnknownTopLevelKeys(Vec<UnknownKey>),
    /// Unrecognized keys inside environment mappings, in name order, each
    /// environment with its keys. [`Package::untrusted_keys_refusal`] names every
    /// environment carrying any; [`Package::spec_refusal`] names only the one
    /// asked about.
    UnknownEnvironmentKeys(Vec<(String, Vec<UnknownKey>)>),
    /// The top level could not be read back, carrying the parse failure.
    UncheckedTopLevel(String),
    /// No environment is declared, carrying the issue `spec validate` raises for
    /// it so a caller reporting structured diagnostics states one category and
    /// one field rather than inventing its own.
    NoEnvironments(ValidationIssue),
}

impl SpecRefusal {
    // `None` for no reasons, so a refusal always has one to report.
    fn of(reasons: impl IntoIterator<Item = Option<RefusalReason>>) -> Option<Self> {
        let reasons: Vec<RefusalReason> = reasons.into_iter().flatten().collect();
        (!reasons.is_empty()).then_some(Self(reasons))
    }

    /// Every reason, in the order they are reported.
    pub(crate) fn reasons(&self) -> &[RefusalReason] {
        &self.0
    }

    /// What this refusal objects to first, without its detail.
    pub(crate) fn kind(&self) -> RefusalKind {
        self.0[0].kind()
    }

    /// The field paths `selfie spec validate` reports this refusal's problems at,
    /// one per problem across every reason, so a caller holding its issues can
    /// tell whether each is already reported.
    pub(crate) fn fields(&self) -> Vec<String> {
        self.0.iter().flat_map(RefusalReason::fields).collect()
    }
}

impl fmt::Display for SpecRefusal {
    // One sentence per reason.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&super::join_sentences(
            self.0.iter().map(RefusalReason::to_string),
        ))
    }
}

impl RefusalReason {
    /// What this reason objects to, without its detail.
    pub(crate) fn kind(&self) -> RefusalKind {
        match self {
            Self::UnknownTopLevelKeys(_) => RefusalKind::UnknownTopLevelKeys,
            Self::UnknownEnvironmentKeys(_) => RefusalKind::UnknownEnvironmentKeys,
            Self::UncheckedTopLevel(_) => RefusalKind::UncheckedTopLevel,
            Self::NoEnvironments(_) => RefusalKind::NoEnvironments,
        }
    }

    /// The field paths `selfie spec validate` reports this reason's problems at,
    /// one per problem.
    pub(crate) fn fields(&self) -> Vec<String> {
        match self {
            Self::UnknownTopLevelKeys(keys) => keys.iter().map(|key| key.key.clone()).collect(),
            Self::UnknownEnvironmentKeys(environments) => environments
                .iter()
                .flat_map(|(environment, keys)| {
                    keys.iter()
                        .map(|key| super::environment_field(environment, &key.key))
                })
                .collect(),
            // Validate reports an unread top level against the package as a whole.
            Self::UncheckedTopLevel(_) => vec!["package".to_string()],
            Self::NoEnvironments(issue) => vec![issue.field().to_string()],
        }
    }
}

impl fmt::Display for RefusalReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTopLevelKeys(keys) => {
                f.write_str(&super::describe_unknown_keys::<PackageField>(keys))
            }
            // One sentence per environment, so the "; " inside a clause never has
            // to separate environments too. The first starts lowercase because
            // every caller puts it after a colon.
            Self::UnknownEnvironmentKeys(environments) => {
                let described: Vec<String> = environments
                    .iter()
                    .map(|(environment, keys)| {
                        format!(
                            "environment '{environment}': {}",
                            super::describe_unknown_keys::<EnvironmentField>(keys)
                        )
                    })
                    .collect();
                write!(f, "in {}", described.join(". In "))
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
                None => f.write_str(&issue.message()),
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
        // In name order, whatever order the file gives, because both consumers
        // render these in the order they arrive and name order is what every
        // environment diagnostic reports in.
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

    /// Why this file's keys cannot be trusted at any scope, when they cannot:
    /// an unknown key at the top level or in any environment, or a top level
    /// selfie could not read back. A rewrite refuses such a file, since it
    /// would drop what the keys hold.
    ///
    /// Says nothing about the individual entries, which are refused one at a
    /// time where they are read, and nothing about whether an environment is
    /// declared at all, which is a question about deploying. A command showing
    /// every environment asks [`Package::every_environment_refusal`], which
    /// adds that question.
    pub(crate) fn untrusted_keys_refusal(&self) -> Option<SpecRefusal> {
        // Every environment rather than one, because a rewrite serializes all of
        // them, and so does a listing that shows them.
        //
        // It names every level and every offending environment, so a file with
        // keys in several is fixed in one pass.
        //
        // The unread top level comes last, after the environments, because its
        // parse failure is a multi-line snippet and anything after it reads as
        // part of it.
        SpecRefusal::of([
            self.unknown_top_level_keys(),
            self.any_unknown_environment_keys(),
            self.unchecked_top_level(),
        ])
    }

    /// Why a command showing every environment refuses this package, when it
    /// does: [`Package::untrusted_keys_refusal`], then a package spec that
    /// declares no environment, which no environment can deploy.
    pub(crate) fn every_environment_refusal(&self) -> Option<SpecRefusal> {
        // A rewrite asks `untrusted_keys_refusal` alone, since `spec update` is
        // how a spec with no environment gets its first one. The missing
        // environment is judged as in `spec_refusal`.
        self.or_missing_environment(self.untrusted_keys_refusal())
    }

    // In name order, whatever order the file gives, as every environment
    // diagnostic reports.
    fn any_unknown_environment_keys(&self) -> Option<RefusalReason> {
        let offending: Vec<(String, Vec<UnknownKey>)> = self
            .environments_sorted()
            .into_iter()
            .filter(|(_, env)| !env.unknown_keys().is_empty())
            .map(|(name, env)| (name.to_string(), env.unknown_keys().to_vec()))
            .collect();
        (!offending.is_empty()).then_some(RefusalReason::UnknownEnvironmentKeys(offending))
    }

    /// Why selfie refuses this whole package in `environment`, when it does.
    ///
    /// `Some` carries every reason to report, and what it costs is the caller's
    /// to decide.
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
        // to be used, then the top level it could not read back at all, whose
        // multi-line parse failure has to end the sentence.
        self.or_missing_environment(SpecRefusal::of([
            self.unknown_top_level_keys(),
            self.unknown_environment_keys(environment),
            self.unchecked_top_level(),
        ]))
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
            || (self.requires_environments() && self.validate_environments_exists().is_err())
    }

    /// Whether this spec must declare at least one environment: every spec does
    /// except a standalone dotfile spec, which deploys on every machine.
    pub(crate) fn requires_environments(&self) -> bool {
        // Keyed away from the dotfiles directory, so a spec built in memory, such
        // as one a caller is about to save as a package, is held to the rule
        // apply will hold its file to.
        self.origin() != SpecOrigin::DotfilesDirectory
    }

    // `keys`, or the missing environment when no key reason holds. A file that is
    // both missing an environment and carrying a bad key reports the key, the
    // reason a reader can act on without first guessing which of the two selfie
    // objected to.
    fn or_missing_environment(&self, keys: Option<SpecRefusal>) -> Option<SpecRefusal> {
        keys.or_else(|| SpecRefusal::of([self.no_environments()]))
    }

    // Only a package spec must declare one. A standalone dotfile spec has no
    // environments because `dotfiles track` writes it that way: it deploys from
    // the shared list and an environment would have nothing to say about it.
    // Refusing those would refuse every dotfile anyone has tracked, and no test
    // in this workspace fails when it does -- the rule is keyed on the origin
    // for that reason and not as a convenience.
    fn no_environments(&self) -> Option<RefusalReason> {
        if !self.requires_environments() {
            return None;
        }
        self.validate_environments_exists()
            .err()
            .map(RefusalReason::NoEnvironments)
    }

    // A `configs:` or a `_dotfiles:` anchor leaves the list selfie read empty or
    // short, so a caller checking only for entries would pass over the package in
    // silence (selfie-g199, selfie-jt6m). This is the set `selfie spec validate`
    // errors on, so the two commands answer alike.
    fn unknown_top_level_keys(&self) -> Option<RefusalReason> {
        match self.top_level_keys() {
            TopLevelKeys::Checked(keys) if !keys.is_empty() => {
                Some(RefusalReason::UnknownTopLevelKeys(keys.clone()))
            }
            _ => None,
        }
    }

    // Scoped to one environment, because a typo in one a run does not touch
    // cannot affect what it deploys. An unknown key here is not merely ignored:
    // `_dotfiles:` leaves this environment's list empty, so
    // `dotfiles_for_environment` falls back to the shared entry and deploys a
    // file this machine was meant to override.
    fn unknown_environment_keys(&self, environment: &str) -> Option<RefusalReason> {
        let unknown = self.environments().get(environment)?.unknown_keys();

        (!unknown.is_empty()).then(|| {
            RefusalReason::UnknownEnvironmentKeys(vec![(environment.to_string(), unknown.to_vec())])
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
    fn unchecked_top_level(&self) -> Option<RefusalReason> {
        match self.top_level_keys() {
            TopLevelKeys::Unchecked(error) => Some(RefusalReason::UncheckedTopLevel(error.clone())),
            _ => None,
        }
    }
}

/// The environments a listing shows, which decide when it refuses a spec.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Shown<'a> {
    /// The current environment only: a spec is refused when apply would refuse
    /// it here.
    Current(&'a str),
    /// Every environment: a spec is refused when a key in any of them, or at its
    /// top level, cannot be trusted, or when it is a package spec that declares
    /// no environment.
    Every,
}

/// Split `packages` into the specs readable in the environments `shown` covers
/// and the ones refused there, each with its refusal.
pub(crate) fn separate_refused<'a>(
    packages: impl IntoIterator<Item = &'a Package>,
    shown: Shown<'_>,
) -> (Vec<&'a Package>, Vec<(&'a Package, RefusedSpec)>) {
    let mut readable = Vec::new();
    let mut refused = Vec::new();
    for package in packages {
        let refusal = match shown {
            Shown::Current(environment) => package.spec_refusal(environment),
            Shown::Every => package.every_environment_refusal(),
        };
        match refusal {
            Some(reason) => refused.push((package, RefusedSpec::new(package, &reason))),
            None => readable.push(package),
        }
    }
    (readable, refused)
}

/// Split `packages` into the ones a command over `environment` may act on and
/// the ones refused whole there, and report the refused ones on `sender`,
/// grouped by reason.
///
/// Every refusal is reported before the caller acts on any package, so a
/// consumer can say each reason once.
pub(crate) async fn refuse_up_front<'a>(
    packages: impl IntoIterator<Item = &'a Package>,
    environment: &str,
    sender: &EventSender,
) -> (Vec<&'a Package>, Vec<&'a Package>) {
    let (readable, refused) = separate_refused(packages, Shown::Current(environment));
    sender
        .send_packages_refused(refused.iter().map(|(_, spec)| spec.into()))
        .await;
    (
        readable,
        refused.into_iter().map(|(package, _)| package).collect(),
    )
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

    // An unread top level against an environment's own key is the one pair whose
    // order a reader can see: the two top-level rules cannot both hold of one
    // file. Both are named, the environment first, because the parse failure is
    // a multi-line snippet that has to end the sentence.
    #[test]
    fn an_environment_key_is_reported_ahead_of_an_unread_top_level() {
        let yaml = format!(
            "name: myapp\n{UNREADABLE_TOP_LEVEL}environments:\n  test:\n    install: \"echo \
             i\"\n    _dotfiles:\n      - source: myapp/config.toml\n        target: \
             ~/.config/myapp/config.toml\n"
        );
        let refusal = package_from(&yaml)
            .spec_refusal("test")
            .expect("both rules apply")
            .to_string();
        assert_environment_then_unread(&refusal);
    }

    // The listing asks every environment, and the same order holds there. A save
    // shares this answer, so the same order decides how a rewrite lists them.
    #[test]
    fn a_listing_reports_an_environment_key_ahead_of_an_unread_top_level() {
        let yaml = format!(
            "name: myapp\n{UNREADABLE_TOP_LEVEL}environments:\n  test:\n    install: \"echo \
             i\"\n    _dotfiles:\n      - source: myapp/config.toml\n        target: \
             ~/.config/myapp/config.toml\n"
        );
        let refusal = package_from(&yaml)
            .untrusted_keys_refusal()
            .expect("both rules apply")
            .to_string();
        assert_environment_then_unread(&refusal);
    }

    fn assert_environment_then_unread(refusal: &str) {
        assert!(
            refusal.starts_with("in environment 'test':"),
            "the environment's own key must be reported first, got: {refusal}"
        );
        crate::package::assert_in_order(
            refusal,
            &[
                "_dotfiles",
                ". Its top-level keys could not be checked",
                "The re-read failed with:",
            ],
        );
    }

    // Every level a listing asks about is named in one refusal, the top level
    // first and each offending environment after it in name order, so the file
    // is fixed in one pass.
    #[test]
    fn a_listing_names_the_top_level_and_every_environment() {
        use crate::package::event::RefusalKind;

        let yaml = "name: myapp\nconfigs: []\nenvironments:\n  work:\n    install: \"echo i\"\n    \
                    audt: x\n  alpha:\n    install: \"echo i\"\n    chk: x\n";
        let refusal = package_from(yaml)
            .untrusted_keys_refusal()
            .expect("the top level and two environments carry unknown keys");
        let rendered = refusal.to_string();

        crate::package::assert_in_order(
            &rendered,
            &[
                "'configs'",
                ". In environment 'alpha': unknown field 'chk'",
                ". In environment 'work': unknown field 'audt'",
            ],
        );
        assert_eq!(refusal.kind(), RefusalKind::UnknownTopLevelKeys);
        assert_eq!(
            refusal.fields(),
            [
                "configs",
                "environments.alpha.chk",
                "environments.work.audt"
            ]
        );
    }

    // Apply names the top level and the environment it applies, and still leaves
    // out an environment the run does not touch.
    #[test]
    fn apply_names_the_top_level_and_the_applied_environment_only() {
        let yaml = "name: myapp\nconfigs: []\nenvironments:\n  work:\n    install: \"echo i\"\n    \
                    audt: x\n  home:\n    install: \"echo i\"\n    chk: x\n";
        let rendered = reason(yaml, "work").expect("the top level and 'work' carry unknown keys");

        assert!(rendered.contains("'configs'"), "got: {rendered}");
        assert!(
            rendered.contains(". In environment 'work': unknown field 'audt'"),
            "got: {rendered}"
        );
        assert!(
            !rendered.contains("'home'") && !rendered.contains("chk"),
            "an environment apply does not touch is named, got: {rendered}"
        );
    }

    // A package spec carrying a key and declaring no environment reports the
    // key alone, in both rules, since the key may be what hid the environments.
    #[test]
    fn a_key_refusal_does_not_add_the_missing_environment() {
        use crate::package::event::RefusalKind;

        let package = package_from("name: myapp\nconfigs: []\n");
        for refusal in [
            package.spec_refusal("work").expect("the key refuses it"),
            package
                .every_environment_refusal()
                .expect("the key refuses it"),
        ] {
            let kinds: Vec<RefusalKind> = refusal
                .reasons()
                .iter()
                .map(super::RefusalReason::kind)
                .collect();
            assert_eq!(kinds, [RefusalKind::UnknownTopLevelKeys]);
            assert!(
                !refusal
                    .to_string()
                    .contains("Add an 'environments' section"),
                "got: {refusal}"
            );
        }
    }

    // A rewrite drops what a key holds whether or not an environment is declared,
    // so the rule a rewrite asks never names the missing one, with or without a
    // key beside it.
    #[test]
    fn untrusted_keys_refusal_never_names_a_missing_environment() {
        use crate::package::event::RefusalKind;

        assert!(
            package_from("name: myapp\n")
                .untrusted_keys_refusal()
                .is_none()
        );
        let refusal = package_from("name: myapp\nconfigs: []\n")
            .untrusted_keys_refusal()
            .expect("the key refuses it");
        let kinds: Vec<RefusalKind> = refusal
            .reasons()
            .iter()
            .map(super::RefusalReason::kind)
            .collect();
        assert_eq!(kinds, [RefusalKind::UnknownTopLevelKeys]);
    }

    // A single reason reads exactly as that reason does. The words for the two
    // key levels are written out, so a change to how one reason is joined cannot
    // pass by moving the reason's own words with it.
    #[test]
    fn a_single_reason_renders_as_that_reason() {
        let env = "    install: \"echo i\"\n";
        assert_eq!(
            reason(
                &format!("name: myapp\nchk: x\nenvironments:\n  work:\n{env}"),
                "work"
            )
            .as_deref(),
            Some(
                "unknown field 'chk'; expected one of: name, homepage, description, dotfiles, \
                 post_install_note, environments"
            )
        );
        assert_eq!(
            reason(
                &format!("name: myapp\nenvironments:\n  work:\n{env}    chk: x\n"),
                "work"
            )
            .as_deref(),
            Some(
                "in environment 'work': unknown field 'chk'; expected one of: install, check, \
                 audit, dependencies, recommends, dotfiles"
            )
        );
        for yaml in [
            format!("name: myapp\n{UNREADABLE_TOP_LEVEL}environments:\n  work:\n{env}"),
            "name: myapp\nenvironments: {}\n".to_string(),
        ] {
            let refusal = package_from(&yaml).spec_refusal("work").expect(&yaml);
            assert_eq!(refusal.reasons().len(), 1, "{yaml}");
            assert_eq!(refusal.to_string(), refusal.reasons()[0].to_string());
        }
    }

    // Every offending environment is named, in name order whatever order the file
    // gives, one sentence each: a file with keys in two environments is fixed in
    // one pass only if both are named.
    #[test]
    fn a_listing_names_every_offending_environment_in_name_order() {
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
            .untrusted_keys_refusal()
            .expect("both environments carry a shadowing key");
        let rendered = refusal.to_string();
        assert!(
            rendered.starts_with("in environment 'alpha':"),
            "got: {rendered}"
        );
        assert!(
            rendered.contains("is refused. In environment 'zulu':"),
            "got: {rendered}"
        );
    }

    // Environments are separated by sentences, never by the "; " a clause already
    // uses between keys and inside an "expected one of" message.
    #[test]
    fn a_listing_gives_each_environment_its_own_sentence() {
        let yaml = "name: myapp\nenvironments:\n  alpha:\n    install: \"echo i\"\n    audt: \
                    \"x\"\n  zulu:\n    install: \"echo i\"\n    chk: \"x\"\n";
        let rendered = package_from(yaml)
            .untrusted_keys_refusal()
            .expect("both environments carry an unknown key")
            .to_string();
        assert!(
            rendered.contains("dotfiles. In environment 'zulu': unknown field 'chk'"),
            "got: {rendered}"
        );
        assert!(!rendered.contains("; in environment"), "got: {rendered}");
    }

    // A listing of every environment refuses a package spec declaring none, as
    // one of the current environment does, and still lists a standalone spec
    // declaring none. A spec that also carries a top-level key reports the key,
    // as `spec_refusal` does.
    #[test]
    fn a_listing_of_every_environment_refuses_a_spec_declaring_none() {
        use crate::package::event::RefusalKind;

        let no_environment = package_from("name: noenv\n");
        let with_key = package_from("name: keyed\nconfigs: []\n");
        let standalone = standalone_from(TRACKED_STANDALONE);
        let clean = package_from(CLEAN);

        let (readable, refused) = super::separate_refused(
            [&no_environment, &with_key, &standalone, &clean],
            super::Shown::Every,
        );

        let readable: Vec<&str> = readable.iter().map(|package| package.name()).collect();
        assert_eq!(readable, ["gemrc", "myapp"]);
        let refused: Vec<(&str, RefusalKind)> = refused
            .iter()
            .map(|(_, spec)| (spec.package_name.as_str(), spec.kind))
            .collect();
        assert_eq!(
            refused,
            [
                ("noenv", RefusalKind::NoEnvironments),
                ("keyed", RefusalKind::UnknownTopLevelKeys)
            ]
        );
        // The listing of the current environment gives the same answers.
        for package in [&no_environment, &with_key] {
            assert_eq!(
                package.every_environment_refusal().map(|r| r.kind()),
                package.spec_refusal("test").map(|r| r.kind()),
                "{}",
                package.name()
            );
        }
    }
}
