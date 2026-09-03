//! Per-machine deploy state persistence and drift detection.
//!
//! Each time selfie deploys a config file, it records the source and deployed
//! checksums in a [`DeployState`] file (typically `~/.local/state/selfie/deploy-state.yml`,
//! i.e., under XDG_STATE_HOME).
//! On subsequent runs, these stored checksums are compared against the current
//! file contents to classify changes as one of four [`DriftType`] variants —
//! enabling the service layer to decide whether to deploy, skip, or flag a
//! conflict.
//!
//! The state file is intentionally per-machine and not version-controlled: it
//! reflects what was deployed *here*, which may differ from other machines
//! sharing the same config repository.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeployState {
    #[serde(default)]
    deployed: HashMap<String, DeployEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployEntry {
    source_checksum: String,
    deployed_checksum: String,
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

    pub fn entries(&self) -> &HashMap<String, DeployEntry> {
        &self.deployed
    }

    pub fn get(&self, source: &str) -> Option<&DeployEntry> {
        self.deployed.get(source)
    }

    pub fn record_deployment(&mut self, source: &str, checksum: &str) {
        self.deployed.insert(
            source.to_string(),
            DeployEntry {
                source_checksum: checksum.to_string(),
                deployed_checksum: checksum.to_string(),
                deployed_at: chrono::Utc::now().to_rfc3339(),
            },
        );
    }

    pub fn detect_drift(
        &self,
        source: &str,
        current_source_checksum: &str,
        current_target_checksum: &str,
    ) -> DriftType {
        let Some(entry) = self.deployed.get(source) else {
            return DriftType::NotTracked;
        };
        match (
            entry.source_checksum != current_source_checksum,
            entry.deployed_checksum != current_target_checksum,
        ) {
            (false, false) => DriftType::None,
            (true, false) => DriftType::RepoChanged,
            (false, true) => DriftType::TargetChanged,
            (true, true) => DriftType::BothChanged,
        }
    }
}

impl DeployEntry {
    pub fn source_checksum(&self) -> &str {
        &self.source_checksum
    }

    pub fn deployed_checksum(&self) -> &str {
        &self.deployed_checksum
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The exact sentence each malformed deploy state file produces.
    //
    // `no_malformed_state_file_shape_quotes_its_contents` in the service tests
    // proves no fixture quotes its own content; this proves the wording itself does
    // not drift. The classifier is shared with the package specs, whose diagnostics
    // are allowed to say more, so a change made for their benefit has to show up
    // here as a diff rather than as silence.
    #[test]
    fn every_malformed_state_file_renders_its_exact_sentence() {
        let cases: &[(&str, &str, &str)] = &[
            (
                "duplicate key",
                "deployed:\n  a: {source_checksum: '1', deployed_checksum: '2', deployed_at: '3'}\n  a: {source_checksum: '1', deployed_checksum: '2', deployed_at: '3'}\n",
                "a key is listed twice at line 3, column 3",
            ),
            (
                "missing field",
                "deployed:\n  ~/.npmrc:\n    source_checksum: '1'\n",
                "an entry is missing the field deployed_checksum at line 3, column 5",
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
                "deployed:\n  a:\n    source_checksum: ~\n    deployed_checksum: '2'\n    deployed_at: '3'\n",
                "a value is empty where text is required at line 3, column 22",
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
    fn test_record_deployment() {
        let mut state = DeployState::empty();
        state.record_deployment("fnm/fish-conf.fish", "abc123");
        let entry = state.get("fnm/fish-conf.fish").unwrap();
        assert_eq!(entry.source_checksum(), "abc123");
        assert_eq!(entry.deployed_checksum(), "abc123");
    }

    #[test]
    fn test_roundtrip_serialization() {
        let mut state = DeployState::empty();
        state.record_deployment("a/b.txt", "hash1");
        let yaml = serde_saphyr::to_string(&state).unwrap();
        let loaded: DeployState = crate::yaml::parse(&yaml).unwrap();
        assert_eq!(loaded.entries().len(), 1);
    }

    #[test]
    fn test_detect_drift_no_change() {
        let mut state = DeployState::empty();
        state.record_deployment("a/b.txt", "hash1");
        assert_eq!(
            state.detect_drift("a/b.txt", "hash1", "hash1"),
            DriftType::None
        );
    }

    #[test]
    fn test_detect_drift_repo_changed() {
        let mut state = DeployState::empty();
        state.record_deployment("a/b.txt", "hash1");
        assert_eq!(
            state.detect_drift("a/b.txt", "hash2", "hash1"),
            DriftType::RepoChanged
        );
    }

    #[test]
    fn test_detect_drift_target_changed() {
        let mut state = DeployState::empty();
        state.record_deployment("a/b.txt", "hash1");
        assert_eq!(
            state.detect_drift("a/b.txt", "hash1", "hash_different"),
            DriftType::TargetChanged
        );
    }

    #[test]
    fn test_detect_drift_both_changed() {
        let mut state = DeployState::empty();
        state.record_deployment("a/b.txt", "hash1");
        assert_eq!(
            state.detect_drift("a/b.txt", "hash2", "hash3"),
            DriftType::BothChanged
        );
    }

    #[test]
    fn test_detect_drift_not_tracked() {
        let state = DeployState::empty();
        assert_eq!(
            state.detect_drift("unknown.txt", "hash1", "hash2"),
            DriftType::NotTracked
        );
    }
}
