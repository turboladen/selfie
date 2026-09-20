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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployEntry {
    source: String,
    checksum: String,
    deployed_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriftType {
    None,
    RepoChanged,
    TargetChanged,
    BothChanged,
    NotTracked,
}

impl std::fmt::Display for DriftType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DriftType::None => write!(f, "none"),
            DriftType::RepoChanged => write!(f, "repo changed"),
            DriftType::TargetChanged => write!(f, "target changed"),
            DriftType::BothChanged => write!(f, "both changed"),
            DriftType::NotTracked => write!(f, "not tracked"),
        }
    }
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

    /// Record that `source` was deployed to `target` with `checksum`, replacing
    /// any earlier record for the same target.
    pub fn record_deployment(&mut self, target: &str, source: &str, checksum: &str) {
        self.deployed.insert(
            target.to_string(),
            DeployEntry {
                source: source.to_string(),
                checksum: checksum.to_string(),
                deployed_at: chrono::Utc::now().to_rfc3339(),
            },
        );
    }

    pub fn detect_drift(
        &self,
        target: &str,
        current_source_checksum: &str,
        current_target_checksum: &str,
    ) -> DriftType {
        let Some(entry) = self.deployed.get(target) else {
            return DriftType::NotTracked;
        };
        match (
            entry.checksum != current_source_checksum,
            entry.checksum != current_target_checksum,
        ) {
            (false, false) => DriftType::None,
            (true, false) => DriftType::RepoChanged,
            (false, true) => DriftType::TargetChanged,
            (true, true) => DriftType::BothChanged,
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
        state.record_deployment("/home/user/.zshrc", "shell/rc", "h1");
        state.record_deployment("/home/user/.bashrc", "shell/rc", "h2");
        assert_eq!(state.entries().len(), 2);
        assert_eq!(state.get("/home/user/.zshrc").unwrap().checksum(), "h1");
        assert_eq!(state.get("/home/user/.bashrc").unwrap().checksum(), "h2");
    }

    #[test]
    fn the_written_shape_round_trips_keyed_by_target() {
        let mut state = DeployState::empty();
        state.record_deployment("/home/user/b.txt", "a/b.txt", "hash1");
        let yaml = serde_saphyr::to_string(&state).unwrap();
        assert!(
            yaml.contains("/home/user/b.txt:") && yaml.contains("source: a/b.txt"),
            "{yaml}"
        );
        let loaded: DeployState = crate::yaml::parse(&yaml).unwrap();
        assert_eq!(loaded.entries().len(), 1);
        assert_eq!(loaded.get("/home/user/b.txt").unwrap().checksum(), "hash1");
    }

    #[test]
    fn test_detect_drift_no_change() {
        let mut state = DeployState::empty();
        state.record_deployment("/t/b.txt", "a/b.txt", "hash1");
        assert_eq!(
            state.detect_drift("/t/b.txt", "hash1", "hash1"),
            DriftType::None
        );
    }

    #[test]
    fn test_detect_drift_repo_changed() {
        let mut state = DeployState::empty();
        state.record_deployment("/t/b.txt", "a/b.txt", "hash1");
        assert_eq!(
            state.detect_drift("/t/b.txt", "hash2", "hash1"),
            DriftType::RepoChanged
        );
    }

    #[test]
    fn test_detect_drift_target_changed() {
        let mut state = DeployState::empty();
        state.record_deployment("/t/b.txt", "a/b.txt", "hash1");
        assert_eq!(
            state.detect_drift("/t/b.txt", "hash1", "hash_different"),
            DriftType::TargetChanged
        );
    }

    #[test]
    fn test_detect_drift_both_changed() {
        let mut state = DeployState::empty();
        state.record_deployment("/t/b.txt", "a/b.txt", "hash1");
        assert_eq!(
            state.detect_drift("/t/b.txt", "hash2", "hash3"),
            DriftType::BothChanged
        );
    }

    #[test]
    fn test_detect_drift_not_tracked() {
        let state = DeployState::empty();
        assert_eq!(
            state.detect_drift("/t/unknown.txt", "hash1", "hash2"),
            DriftType::NotTracked
        );
    }
}
