use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeployState {
    #[serde(default)]
    deployed: HashMap<String, DeployEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployEntry {
    target: String,
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

    pub fn record_deployment(&mut self, source: &str, target: &str, checksum: &str) {
        self.deployed.insert(
            source.to_string(),
            DeployEntry {
                target: target.to_string(),
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
    pub fn target(&self) -> &str {
        &self.target
    }

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

    #[test]
    fn test_empty_state() {
        let state = DeployState::empty();
        assert!(state.entries().is_empty());
    }

    #[test]
    fn test_record_deployment() {
        let mut state = DeployState::empty();
        state.record_deployment(
            "fnm/fish-conf.fish",
            "/home/user/.config/fish/conf.d/fnm.fish",
            "abc123",
        );
        let entry = state.get("fnm/fish-conf.fish").unwrap();
        assert_eq!(entry.target(), "/home/user/.config/fish/conf.d/fnm.fish");
        assert_eq!(entry.source_checksum(), "abc123");
        assert_eq!(entry.deployed_checksum(), "abc123");
    }

    #[test]
    fn test_roundtrip_serialization() {
        let mut state = DeployState::empty();
        state.record_deployment("a/b.txt", "/home/user/b.txt", "hash1");
        let yaml = serde_yaml::to_string(&state).unwrap();
        let loaded: DeployState = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(loaded.entries().len(), 1);
    }

    #[test]
    fn test_detect_drift_no_change() {
        let mut state = DeployState::empty();
        state.record_deployment("a/b.txt", "/target/b.txt", "hash1");
        assert_eq!(
            state.detect_drift("a/b.txt", "hash1", "hash1"),
            DriftType::None
        );
    }

    #[test]
    fn test_detect_drift_repo_changed() {
        let mut state = DeployState::empty();
        state.record_deployment("a/b.txt", "/target/b.txt", "hash1");
        assert_eq!(
            state.detect_drift("a/b.txt", "hash2", "hash1"),
            DriftType::RepoChanged
        );
    }

    #[test]
    fn test_detect_drift_target_changed() {
        let mut state = DeployState::empty();
        state.record_deployment("a/b.txt", "/target/b.txt", "hash1");
        assert_eq!(
            state.detect_drift("a/b.txt", "hash1", "hash_different"),
            DriftType::TargetChanged
        );
    }

    #[test]
    fn test_detect_drift_both_changed() {
        let mut state = DeployState::empty();
        state.record_deployment("a/b.txt", "/target/b.txt", "hash1");
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
