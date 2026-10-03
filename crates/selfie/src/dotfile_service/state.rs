//! Per-machine deploy state persistence and drift detection.
//!
//! Each time selfie deploys a config file, it records the checksum of what it
//! wrote, keyed by the target path, in a [`DeployState`] file (typically
//! `~/.local/state/selfie/deploy-state.yml`, i.e., under XDG_STATE_HOME).
//! On subsequent runs, the stored checksum is compared against the current
//! source and target contents to classify changes as one of four [`DriftType`]
//! variants — enabling the service layer to decide whether to deploy, skip, or
//! flag a conflict.
//!
//! The state file is intentionally per-machine and not version-controlled: it
//! reflects what was deployed *here*, which may differ from other machines
//! sharing the same config repository.

use serde::{Deserialize, Serialize};

use crate::{
    config::SelfieConfig,
    package::event::{BaseKind, DotfileSource, DriftType, RepoPath, SourceBase},
};
use std::collections::HashMap;

// Keyed by the expanded target path, because the target is what has one file on
// disk and one checksum. One source deployed to two targets must be two
// records, or drift for one target is answered from the other's checksum.
//
// `deployed` has no default on purpose: a document without it -- a comment, a
// null, a stray key -- must fail to parse rather than read as "nothing deployed",
// which is what a first run looks like. The writer always emits the mapping.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeployState {
    deployed: HashMap<String, DeployEntry>,
}

/// What selfie last wrote to one target.
// One checksum, because the repository file is written to the target as it is:
// the two are equal at the moment of deployment, and secret-bearing entries
// record nothing.
//
// `package` is optional because records written before it existed lack it, and
// a required field would make every such state file fail to parse, which
// refuses every apply. An apply fills it in for a target its package produces.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployEntry {
    source: String,
    checksum: String,
    deployed_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    package: Option<String>,
    /// The directory `source` is relative to, when the record names one.
    // Optional because a state file may hold records without it, which must still
    // parse. Apply fills it in when it finds such a target in sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base: Option<BaseKind>,
}

impl DeployState {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Every record, keyed by target path.
    pub fn entries(&self) -> &HashMap<String, DeployEntry> {
        &self.deployed
    }

    /// The record for a target path, if selfie has deployed to it.
    pub fn get(&self, target: &str) -> Option<&DeployEntry> {
        self.deployed.get(target)
    }

    /// Record that `source` was deployed to `target` with `checksum` by the
    /// package named `package`, replacing any earlier record for the same target.
    ///
    /// `package` is the package's spec name, case folded, or `None` for a
    /// package with no spec file behind it.
    pub fn record_deployment(
        &mut self,
        target: &str,
        source: &str,
        checksum: &str,
        package: Option<&str>,
        base: Option<BaseKind>,
    ) {
        self.deployed.insert(
            target.to_string(),
            DeployEntry {
                source: source.to_string(),
                checksum: checksum.to_string(),
                deployed_at: chrono::Utc::now().to_rfc3339(),
                package: package.map(str::to_string),
                base,
            },
        );
    }

    /// Name `package` as the one that deployed `target`, replacing whatever the
    /// record named. Returns whether the record changed; a target with no
    /// record gains none.
    pub fn attribute(&mut self, target: &str, package: &str) -> bool {
        match self.deployed.get_mut(target) {
            Some(entry) if entry.package.as_deref() != Some(package) => {
                entry.package = Some(package.to_string());
                true
            }
            _ => false,
        }
    }

    /// Record that `target`'s source is `source`, relative to `base`, keeping
    /// the rest of its record. Returns whether the record changed; a target
    /// with no record gains none.
    pub fn place(&mut self, target: &str, source: &str, base: BaseKind) -> bool {
        match self.deployed.get_mut(target) {
            Some(entry) if entry.base != Some(base) || entry.source != source => {
                entry.source = source.to_string();
                entry.base = Some(base);
                true
            }
            _ => false,
        }
    }

    /// Drop the record for `target`. Returns whether there was one.
    pub fn remove(&mut self, target: &str) -> bool {
        self.deployed.remove(target).is_some()
    }

    /// How `target` and its source have moved since the recorded deploy, or `None`
    /// when the target holds what was deployed and the source has not changed.
    pub fn detect_drift(
        &self,
        target: &str,
        current_source_checksum: &str,
        current_target_checksum: &str,
    ) -> Option<DriftType> {
        let Some(entry) = self.deployed.get(target) else {
            return Some(DriftType::NotTracked);
        };
        match (
            entry.checksum != current_source_checksum,
            entry.checksum != current_target_checksum,
        ) {
            (false, false) => None,
            (true, false) => Some(DriftType::RepoChanged),
            (false, true) => Some(DriftType::TargetChanged),
            (true, true) => Some(DriftType::BothChanged),
        }
    }
}

impl DeployEntry {
    /// The repository source the target was deployed from.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The checksum of the content written.
    pub fn checksum(&self) -> &str {
        &self.checksum
    }

    /// The spec name of the package that deployed the target, or `None` if the
    /// record does not say.
    pub fn package(&self) -> Option<&str> {
        self.package.as_deref()
    }

    /// The directory the source is relative to, or `None` if the record does
    /// not say.
    pub fn base(&self) -> Option<BaseKind> {
        self.base
    }

    /// The source as events name it: relative to the configured directory the
    /// record names, or the recorded spelling when it names none.
    pub fn event_source(&self, config: &SelfieConfig) -> DotfileSource {
        match self.base {
            Some(kind) => DotfileSource::File(RepoPath {
                base: Some(SourceBase {
                    kind,
                    directory: match kind {
                        BaseKind::PackageDirectory => config.package_directory().clone(),
                        BaseKind::DotfilesDirectory => config.dotfiles_directory(),
                    },
                }),
                path: self.source.clone().into(),
            }),
            None => DotfileSource::Recorded(self.source.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The exact sentence each malformed deploy state file produces.
    //
    // `no_malformed_state_file_shape_quotes_its_contents` in the state-file tests
    // proves no fixture quotes its own content; this proves the wording itself does
    // not drift. The classifier is shared with the package specs, whose diagnostics
    // are allowed to say more, so a change made for their benefit has to show up
    // here as a diff rather than as silence.
    #[test]
    fn every_malformed_state_file_renders_its_exact_sentence() {
        let cases: &[(&str, &str, &str)] = &[
            (
                "duplicate key",
                "deployed:\n  /a: {source: '1', checksum: '2', deployed_at: '3'}\n  /a: {source: '1', checksum: '2', deployed_at: '3'}\n",
                "a key is listed twice at line 3, column 3",
            ),
            (
                "missing field",
                "deployed:\n  /home/u/.npmrc:\n    source: '1'\n",
                "an entry is missing the field checksum at line 3, column 5",
            ),
            (
                "wrong shape",
                "deployed: 3\n",
                "the file has the wrong shape, expected mapping start at line 1, column 11",
            ),
            (
                "unclosed flow sequence",
                "deployed: [unclosed\n",
                "the file has the wrong shape, expected mapping start at line 1, column 11",
            ),
            (
                "unexpected character",
                "deployed: @nope\n",
                "unexpected character: `@' at line 1, column 11",
            ),
            (
                "tab indentation",
                "deployed:\n\ta: 1\n",
                "tabs disallowed within this context (block indentation) at line 2, column 2",
            ),
            (
                "multiple documents",
                "deployed: {}\n---\ndeployed: {}\n",
                "the file holds more than one YAML document at line 3, column 1",
            ),
            (
                "null where text is required",
                "deployed:\n  /a:\n    source: ~\n    checksum: '2'\n    deployed_at: '3'\n",
                "a value is empty where text is required at line 3, column 13",
            ),
            (
                "unclosed flow mapping",
                "deployed: {oops\n",
                "unclosed bracket '{' at line 1, column 11",
            ),
            (
                "unknown anchor",
                "deployed: *ghost\n",
                "the file refers to an anchor it never defines at line 1, column 11",
            ),
            // The shape selfie wrote before entries were keyed by target. It is
            // refused like any other unparsable file: the entry's fields name a
            // source and two checksums where a source, one checksum and the
            // target key are expected.
            (
                "source-keyed entry",
                "deployed:\n  myapp/config.toml:\n    target: /home/u/.config/app.toml\n    source_checksum: '1'\n    deployed_checksum: '1'\n    deployed_at: '3'\n",
                "an entry is missing the field source at line 6, column 5",
            ),
        ];

        for (label, yaml, expected) in cases {
            let failure =
                crate::yaml::parse::<DeployState>(yaml).expect_err("fixture must fail to parse");
            assert_eq!(&failure.to_string(), expected, "{label}");
        }
    }

    #[test]
    fn test_empty_state() {
        let state = DeployState::empty();
        assert!(state.entries().is_empty());
    }

    #[test]
    fn a_deployment_is_recorded_under_its_target() {
        let mut state = DeployState::empty();
        state.record_deployment(
            "/home/user/.config/fish/conf.d/fnm.fish",
            "fnm/fish-conf.fish",
            "abc123",
            None,
            None,
        );
        let entry = state
            .get("/home/user/.config/fish/conf.d/fnm.fish")
            .expect("recorded under the target");
        assert_eq!(entry.source(), "fnm/fish-conf.fish");
        assert_eq!(entry.checksum(), "abc123");
        assert!(
            state.get("fnm/fish-conf.fish").is_none(),
            "the record was keyed by source"
        );
    }

    // One source deployed to two targets is two records: each target has its
    // own file on disk and its own checksum.
    #[test]
    fn one_source_deployed_to_two_targets_is_two_records() {
        let mut state = DeployState::empty();
        state.record_deployment("/home/user/.zshrc", "shell/rc", "h1", None, None);
        state.record_deployment("/home/user/.bashrc", "shell/rc", "h2", None, None);
        assert_eq!(state.entries().len(), 2);
        assert_eq!(state.get("/home/user/.zshrc").unwrap().checksum(), "h1");
        assert_eq!(state.get("/home/user/.bashrc").unwrap().checksum(), "h2");
    }

    #[test]
    fn the_written_shape_round_trips_keyed_by_target() {
        let mut state = DeployState::empty();
        state.record_deployment("/home/user/b.txt", "a/b.txt", "hash1", None, None);
        let yaml = crate::yaml::serialize(&state).unwrap();
        assert!(
            yaml.contains("/home/user/b.txt:") && yaml.contains("source: a/b.txt"),
            "{yaml}"
        );
        let loaded: DeployState = crate::yaml::parse(&yaml).unwrap();
        assert_eq!(loaded.entries().len(), 1);
        assert_eq!(loaded.get("/home/user/b.txt").unwrap().checksum(), "hash1");
    }

    // A record written before `package` existed must still parse: a required
    // field would make the whole file unparsable, and apply refuses such a file.
    #[test]
    fn a_record_without_a_package_parses_and_names_none() {
        let yaml = "deployed:\n  /home/u/.npmrc:\n    source: npm/npmrc\n    checksum: abc\n    deployed_at: '2026-01-01T00:00:00Z'\n";
        let state: DeployState = crate::yaml::parse(yaml).expect("a legacy record parses");
        assert_eq!(state.get("/home/u/.npmrc").unwrap().package(), None);
    }

    #[test]
    fn the_recorded_package_round_trips_and_an_absent_one_is_not_written() {
        let mut state = DeployState::empty();
        state.record_deployment("/t/a", "a/a", "h1", Some("alpha"), None);
        state.record_deployment("/t/b", "b/b", "h2", None, None);
        let yaml = crate::yaml::serialize(&state).unwrap();
        assert_eq!(yaml.matches("package:").count(), 1, "{yaml}");
        let loaded: DeployState = crate::yaml::parse(&yaml).unwrap();
        assert_eq!(loaded.get("/t/a").unwrap().package(), Some("alpha"));
        assert_eq!(loaded.get("/t/b").unwrap().package(), None);
    }

    #[test]
    fn attributing_names_the_package_and_reports_only_a_change() {
        let mut state = DeployState::empty();
        state.record_deployment("/t/legacy", "x", "h", None, None);
        state.record_deployment("/t/owned", "y", "h", Some("first"), None);
        assert!(state.attribute("/t/legacy", "second"));
        assert!(state.attribute("/t/owned", "second"));
        assert!(!state.attribute("/t/owned", "second"));
        assert!(!state.attribute("/t/absent", "second"));
        assert_eq!(state.get("/t/legacy").unwrap().package(), Some("second"));
        assert_eq!(state.get("/t/owned").unwrap().package(), Some("second"));
        assert!(state.get("/t/absent").is_none());
    }

    #[test]
    fn removing_drops_only_the_named_record() {
        let mut state = DeployState::empty();
        state.record_deployment("/t/a", "a", "h", None, None);
        state.record_deployment("/t/b", "b", "h", None, None);
        assert!(state.remove("/t/a"));
        assert!(!state.remove("/t/a"));
        assert!(state.get("/t/a").is_none());
        assert!(state.get("/t/b").is_some());
    }

    #[test]
    fn test_detect_drift_no_change() {
        let mut state = DeployState::empty();
        state.record_deployment("/t/b.txt", "a/b.txt", "hash1", None, None);
        assert_eq!(state.detect_drift("/t/b.txt", "hash1", "hash1"), None);
    }

    #[test]
    fn test_detect_drift_repo_changed() {
        let mut state = DeployState::empty();
        state.record_deployment("/t/b.txt", "a/b.txt", "hash1", None, None);
        assert_eq!(
            state.detect_drift("/t/b.txt", "hash2", "hash1"),
            Some(DriftType::RepoChanged)
        );
    }

    #[test]
    fn test_detect_drift_target_changed() {
        let mut state = DeployState::empty();
        state.record_deployment("/t/b.txt", "a/b.txt", "hash1", None, None);
        assert_eq!(
            state.detect_drift("/t/b.txt", "hash1", "hash_different"),
            Some(DriftType::TargetChanged)
        );
    }

    #[test]
    fn test_detect_drift_both_changed() {
        let mut state = DeployState::empty();
        state.record_deployment("/t/b.txt", "a/b.txt", "hash1", None, None);
        assert_eq!(
            state.detect_drift("/t/b.txt", "hash2", "hash3"),
            Some(DriftType::BothChanged)
        );
    }

    #[test]
    fn test_detect_drift_not_tracked() {
        let state = DeployState::empty();
        assert_eq!(
            state.detect_drift("/t/unknown.txt", "hash1", "hash2"),
            Some(DriftType::NotTracked)
        );
    }

    fn config() -> SelfieConfig {
        crate::config::SelfieConfigBuilder::default()
            .environment("test")
            .package_directory("/r/packages")
            .dotfiles_directory(std::path::PathBuf::from("/r/dotfiles"))
            .build()
    }

    // A record names the directory its source is relative to, and comes back as a
    // source in that directory.
    #[test]
    fn a_record_with_a_base_names_its_source_in_that_directory() {
        let mut state = DeployState::empty();
        state.record_deployment(
            "/t/rc",
            "zsh/rc",
            "h",
            Some("zsh"),
            Some(BaseKind::DotfilesDirectory),
        );

        let source = state.get("/t/rc").unwrap().event_source(&config());

        assert_eq!(
            source.absolute(),
            Some(std::path::PathBuf::from("/r/dotfiles/zsh/rc"))
        );
    }

    // A record that names no base still parses: the maintainer's own state file
    // holds such records. Its source comes back as it was spelled, and claims
    // no path.
    #[test]
    fn a_record_without_a_base_parses_and_keeps_its_spelling() {
        let text = "deployed:\n  /t/old:\n    source: myapp/rc\n    checksum: abc\n    \
                    deployed_at: \"2026-09-01T00:00:00+00:00\"\n    package: myapp\n";
        let state: DeployState = crate::yaml::parse(text).expect("a record without a base parses");

        let source = state.get("/t/old").unwrap().event_source(&config());

        assert_eq!(source, DotfileSource::Recorded("myapp/rc".to_string()));
        assert_eq!(source.absolute(), None);
    }
}
