//! Deciding what to do with each config file: checksumming, path resolution, and
//! turning a [`DriftType`] plus target existence into a [`DeployDecision`].
//!
//! Everything here is pure, which is what lets every combination of drift state
//! and target existence be tested without touching a filesystem.

use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::package::event::{DriftType, SkipReason};

/// Compute the SHA-256 checksum of the given data, returning it as a hex string.
pub fn compute_checksum(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        // Infallible: writing to a String never errors.
        let _ = write!(hex, "{byte:02x}");
    }
    hex
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
    Skip(SkipReason),
    /// Conflict detected — needs user input.
    Conflict,
}

/// Given the drift, if any, and whether the target file exists, decide what to do.
///
/// `None` means the target holds what was last deployed and the source has not
/// changed, so there is nothing to do. For `NotTracked` entries (no prior deploy state), `source_checksum` and
/// `target_checksum` are compared directly: if they match, the file is already
/// in sync and can be recorded without deploying; if they differ, it's a real
/// conflict that needs user input.
pub fn deploy_decision(
    drift: Option<&DriftType>,
    target_exists: bool,
    source_checksum: &str,
    target_checksum: &str,
) -> DeployDecision {
    if !target_exists {
        return DeployDecision::Deploy;
    }
    match drift {
        None => DeployDecision::Skip(SkipReason::UpToDate),
        Some(DriftType::RepoChanged) => DeployDecision::Deploy,
        Some(DriftType::TargetChanged | DriftType::BothChanged) => DeployDecision::Conflict,
        Some(DriftType::NotTracked) => {
            if source_checksum == target_checksum {
                DeployDecision::Skip(SkipReason::InSync)
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
        let decision = deploy_decision(Some(&DriftType::NotTracked), false, "a", "");
        assert_eq!(decision, DeployDecision::Deploy);
    }

    #[test]
    fn test_deploy_decision_already_current() {
        let decision = deploy_decision(None, true, "a", "a");
        assert_eq!(decision, DeployDecision::Skip(SkipReason::UpToDate));
    }

    #[test]
    fn test_deploy_decision_repo_changed() {
        let decision = deploy_decision(Some(&DriftType::RepoChanged), true, "b", "a");
        assert_eq!(decision, DeployDecision::Deploy);
    }

    #[test]
    fn test_deploy_decision_target_changed() {
        let decision = deploy_decision(Some(&DriftType::TargetChanged), true, "a", "b");
        assert_eq!(decision, DeployDecision::Conflict);
    }

    #[test]
    fn test_deploy_decision_both_changed() {
        let decision = deploy_decision(Some(&DriftType::BothChanged), true, "b", "c");
        assert_eq!(decision, DeployDecision::Conflict);
    }

    #[test]
    fn test_deploy_decision_not_tracked_matching_checksums() {
        let decision = deploy_decision(Some(&DriftType::NotTracked), true, "same", "same");
        assert_eq!(decision, DeployDecision::Skip(SkipReason::InSync));
    }

    #[test]
    fn test_deploy_decision_not_tracked_different_checksums() {
        let decision = deploy_decision(Some(&DriftType::NotTracked), true, "source", "target");
        assert_eq!(decision, DeployDecision::Conflict);
    }
}
