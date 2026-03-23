//! Pure deployment decision logic.
//!
//! This module contains the stateless, side-effect-free functions that the
//! service layer delegates to when deciding what to do with each config file.
//! Keeping this logic separate from I/O makes it straightforward to test every
//! combination of drift state and target existence without touching the
//! filesystem.
//!
//! The three concerns here are:
//!
//! - **Checksumming** — SHA-256 hashes used to detect whether source or target
//!   files have changed since the last deploy.
//! - **Path resolution** — joining a config source's relative path against the
//!   configured base directory.
//! - **Decision routing** — given a [`DriftType`] and whether the target exists,
//!   produce a [`DeployDecision`] that the caller can act on.

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
///
/// For `NotTracked` entries (no prior deploy state), `source_checksum` and
/// `target_checksum` are compared directly: if they match, the file is already
/// in sync and can be recorded without deploying; if they differ, it's a real
/// conflict that needs user input.
pub fn deploy_decision(
    drift: &DriftType,
    target_exists: bool,
    source_checksum: &str,
    target_checksum: &str,
) -> DeployDecision {
    if !target_exists {
        return DeployDecision::Deploy;
    }
    match drift {
        DriftType::None => DeployDecision::Skip("already up to date".into()),
        DriftType::RepoChanged => DeployDecision::Deploy,
        DriftType::TargetChanged | DriftType::BothChanged => DeployDecision::Conflict,
        DriftType::NotTracked => {
            if source_checksum == target_checksum {
                DeployDecision::Skip("already in sync".into())
            } else {
                DeployDecision::Conflict
            }
        }
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
        let decision = deploy_decision(&DriftType::NotTracked, false, "a", "");
        assert_eq!(decision, DeployDecision::Deploy);
    }

    #[test]
    fn test_deploy_decision_already_current() {
        let decision = deploy_decision(&DriftType::None, true, "a", "a");
        assert_eq!(decision, DeployDecision::Skip("already up to date".into()));
    }

    #[test]
    fn test_deploy_decision_repo_changed() {
        let decision = deploy_decision(&DriftType::RepoChanged, true, "b", "a");
        assert_eq!(decision, DeployDecision::Deploy);
    }

    #[test]
    fn test_deploy_decision_target_changed() {
        let decision = deploy_decision(&DriftType::TargetChanged, true, "a", "b");
        assert_eq!(decision, DeployDecision::Conflict);
    }

    #[test]
    fn test_deploy_decision_both_changed() {
        let decision = deploy_decision(&DriftType::BothChanged, true, "b", "c");
        assert_eq!(decision, DeployDecision::Conflict);
    }

    #[test]
    fn test_deploy_decision_not_tracked_matching_checksums() {
        let decision = deploy_decision(&DriftType::NotTracked, true, "same", "same");
        assert_eq!(decision, DeployDecision::Skip("already in sync".into()));
    }

    #[test]
    fn test_deploy_decision_not_tracked_different_checksums() {
        let decision = deploy_decision(&DriftType::NotTracked, true, "source", "target");
        assert_eq!(decision, DeployDecision::Conflict);
    }
}
