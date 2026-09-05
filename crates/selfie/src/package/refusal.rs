//! Whether `selfie apply` refuses a package before it reaches any of its
//! entries.

use std::fmt;

use super::{EnvironmentField, Package, TopLevelKeys, UnknownKey, describe_unknown_key_in};

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
        keys: Vec<String>,
    },
    /// The top level could not be read back, carrying the parse failure.
    UncheckedTopLevel(String),
}

impl fmt::Display for SpecRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTopLevelKeys(keys) => {
                let described: Vec<&str> = keys.iter().map(|key| key.message.as_str()).collect();
                f.write_str(&described.join("; "))
            }
            // Described here rather than at the point the keys were collected,
            // so the level a key is judged against is also the level it is
            // explained against.
            Self::UnknownEnvironmentKeys { environment, keys } => {
                let described: Vec<String> = keys
                    .iter()
                    .map(|key| describe_unknown_key_in::<EnvironmentField>(key))
                    .collect();
                write!(
                    f,
                    "in environment '{environment}': {}",
                    described.join("; ")
                )
            }
            // The parse failure ends the sentence because it is several lines of
            // source snippet, and anything after it reads as part of it.
            Self::UncheckedTopLevel(error) => write!(
                f,
                "it has no dotfiles to deploy, and its top-level keys could not be checked, so a \
                 shadowed 'dotfiles:' key cannot be ruled out. The re-read failed with: {error}"
            ),
        }
    }
}

impl Package {
    /// Why `selfie apply` refuses this whole package in `environment`, when it
    /// does.
    ///
    /// `Some` carries the reason for the caller to report, and the package
    /// deploys nothing. `None` says this file's top level and the environment's
    /// own keys are usable; it says nothing about the individual entries, which
    /// are refused one at a time where they are read.
    ///
    /// Reads only the package, so a caller that never touches the file system
    /// can ask.
    pub(crate) fn apply_refusal(&self, environment: &str) -> Option<SpecRefusal> {
        // Every reason here lies in the file's top level or in an environment
        // mapping, where there is no entry to attach it to -- which is why the
        // answer covers the package rather than a dotfile.
        //
        // Ordered as apply reads a file: the top level, the environment about to
        // be applied, then the top level it could not read back at all.
        if let TopLevelKeys::Checked(keys) = self.top_level_keys()
            && !keys.is_empty()
        {
            // A `configs:` or a `_dotfiles:` anchor leaves the list selfie read
            // empty or short, so a caller checking only for entries would pass
            // over the package in silence (selfie-g199, selfie-jt6m). This is the
            // set `selfie spec validate` errors on, so the two commands answer
            // alike.
            return Some(SpecRefusal::UnknownTopLevelKeys(keys.clone()));
        }

        // Scoped to the environment being applied, because a typo in one this run
        // does not touch cannot affect what it deploys. An unknown key here is
        // not merely ignored: `_dotfiles:` leaves this environment's list empty,
        // so `dotfiles_for_environment` falls back to the shared entry and
        // deploys a file this machine was meant to override.
        if let Some(env) = self.environments().get(environment) {
            let unknown = env.unknown_keys();
            if !unknown.is_empty() {
                return Some(SpecRefusal::UnknownEnvironmentKeys {
                    environment: environment.to_string(),
                    keys: unknown.to_vec(),
                });
            }
        }

        // With nothing to deploy, a package that has none by design and one whose
        // entries are hidden behind an unchecked key are indistinguishable, and a
        // caller that reported both as nothing to do would exit zero having done
        // nothing (selfie-c28).
        if let TopLevelKeys::Unchecked(error) = self.top_level_keys()
            && self.dotfiles_for_environment(environment).is_empty()
        {
            return Some(SpecRefusal::UncheckedTopLevel(error.clone()));
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::Package;

    // The construct that fails the re-read while the package itself still
    // parses: `serde_json::Value` has no key for a mapping keyed by a sequence.
    const UNREADABLE_TOP_LEVEL: &str = "extra:\n  ? [a, b]\n  : v\n";

    // Loaded the way the repository loads a file, because the answer is derived
    // at `set_source` and a package built any other way carries no keys to judge.
    fn package_from(yaml: &str) -> Package {
        let mut package: Package = crate::yaml::parse(yaml).expect("fixture must parse");
        package.set_source(PathBuf::from("/packages/myapp.yml"), yaml.to_string());
        package
    }

    // The rendered clause, which is what apply and drift put after their colon.
    fn reason(yaml: &str, environment: &str) -> Option<String> {
        package_from(yaml)
            .apply_refusal(environment)
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
        let reason = reason(&yaml, "test")
            .expect("a file that could not be checked and deploys nothing must be refused");
        assert!(reason.contains("cannot be ruled out"), "got: {reason}");
        assert!(reason.contains("The re-read failed with:"), "got: {reason}");
    }

    #[test]
    fn an_unchecked_file_with_dotfiles_is_not_refused() {
        let yaml = format!(
            "name: myapp\n{UNREADABLE_TOP_LEVEL}dotfiles:\n  - source: myapp/config.toml\n    \
             target: ~/.config/myapp/config.toml\nenvironments:\n  test:\n    install: \"echo i\"\n"
        );
        assert_eq!(reason(&yaml, "test"), None);
    }

    // A package with no file behind it has no key a user could misspell. Left
    // out, the whole answer would depend on a state nothing pins.
    #[test]
    fn a_package_built_in_memory_is_not_refused() {
        assert!(Package::new_template("myapp").apply_refusal("test").is_none());
    }
}
