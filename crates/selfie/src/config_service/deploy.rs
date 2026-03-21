use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

use super::state::DriftType;

/// Compute the SHA-256 checksum of the given data, returning it as a hex string.
pub fn compute_checksum(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

/// Resolve a config source path relative to the base directory.
pub fn resolve_source_path(base_dir: &Path, source: &str) -> PathBuf {
    base_dir.join(source)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeployDecision {
    /// Safe to deploy (target doesn't exist or repo is newer).
    Deploy,
    /// Skip deployment (already up to date).
    Skip(String),
    /// Conflict detected — needs user input.
    Conflict,
}

/// Given the drift type and whether the target file exists, decide what to do.
pub fn deploy_decision(drift: &DriftType, target_exists: bool) -> DeployDecision {
    if !target_exists {
        return DeployDecision::Deploy;
    }
    match drift {
        DriftType::None => DeployDecision::Skip("already up to date".into()),
        DriftType::RepoChanged => DeployDecision::Deploy,
        DriftType::TargetChanged | DriftType::BothChanged => DeployDecision::Conflict,
        DriftType::NotTracked => DeployDecision::Conflict, // unknown state, be cautious
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_checksum() {
        let checksum = compute_checksum(b"hello world");
        // SHA-256 of "hello world"
        assert_eq!(
            checksum,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn test_compute_checksum_different_content() {
        let a = compute_checksum(b"hello");
        let b = compute_checksum(b"world");
        assert_ne!(a, b);
    }

    #[test]
    fn test_resolve_source_path() {
        let base_dir = Path::new("/home/user/selfie-packages/packages");
        let source = "fnm/fish-conf.fish";
        let resolved = resolve_source_path(base_dir, source);
        assert_eq!(
            resolved,
            PathBuf::from("/home/user/selfie-packages/packages/fnm/fish-conf.fish")
        );
    }

    #[test]
    fn test_deploy_decision_target_does_not_exist() {
        let decision = deploy_decision(&DriftType::NotTracked, false);
        assert_eq!(decision, DeployDecision::Deploy);
    }

    #[test]
    fn test_deploy_decision_already_current() {
        let decision = deploy_decision(&DriftType::None, true);
        assert_eq!(decision, DeployDecision::Skip("already up to date".into()));
    }

    #[test]
    fn test_deploy_decision_repo_changed() {
        let decision = deploy_decision(&DriftType::RepoChanged, true);
        assert_eq!(decision, DeployDecision::Deploy);
    }

    #[test]
    fn test_deploy_decision_target_changed() {
        let decision = deploy_decision(&DriftType::TargetChanged, true);
        assert_eq!(decision, DeployDecision::Conflict);
    }

    #[test]
    fn test_deploy_decision_both_changed() {
        let decision = deploy_decision(&DriftType::BothChanged, true);
        assert_eq!(decision, DeployDecision::Conflict);
    }
}
