//! What is at a deploy target, and how a refused deploy is worded.
//!
//! The secret-bearing path, the repository-file path and drift all classify a
//! target through here and refuse it in the same words, so none of the three can
//! describe one refusal differently from the others.

use crate::{
    dotfile_service::{deploy::DeployDecision, state::DriftType},
    fs::{
        filesystem::{FileSystem, FileSystemError},
        target::{TargetPath, TargetRejection},
    },
};

/// What is at an entry's target when apply or drift reaches it.
///
/// Kept distinct from `Option<Vec<u8>>` because "absent" and "present but
/// unreadable" call for opposite handling: the first is safe to write, the second
/// must never be written over as though nothing were there.
pub(super) enum TargetState {
    Absent,
    Readable(Vec<u8>),
    Unreadable(FileSystemError),
}

/// What is at `target`: absent, readable, or present but unreadable.
///
/// Read as raw bytes, so two different files are never reported identical after
/// a lossy decode.
// An unreadable file is still a file, and it may be the very thing an overwrite
// would destroy. No caller treats it as absent, which would write over it
// with no prompt: the secret-bearing path reports a conflict and lets an
// interactive resolver choose, since replacing a file needs only write
// permission on its directory; the repository-file path and drift refuse the
// entry outright, because there is no content to show a diff against.
pub(super) fn read_target_state<F: FileSystem>(filesystem: &F, target: &TargetPath) -> TargetState {
    if !filesystem.path_exists(target.path()) {
        return TargetState::Absent;
    }

    match filesystem.read_file_bytes(target.path()) {
        Ok(bytes) => TargetState::Readable(bytes),
        Err(e) => TargetState::Unreadable(e),
    }
}

/// Why an in-sync entry will never settle, when that is the case.
///
/// `Some` for an untracked target whose contents already match but which is a
/// symlink: apply skips it and records nothing, so drift reports it on every run
/// forever. Call it from both apply and drift so their wording cannot diverge.
// Scoped to `NotTracked` deliberately. A *tracked* entry whose target later became
// a symlink produces no drift line at all — a different bug — and answering for it
// here would half-fix that one from the wrong place (selfie-v7py).
pub(super) fn unmanaged_symlink_reason<F: FileSystem>(
    filesystem: &F,
    drift: &DriftType,
    decision: &DeployDecision,
    target: &TargetPath,
) -> Option<&'static str> {
    (*drift == DriftType::NotTracked
        && matches!(decision, DeployDecision::Skip(_))
        && filesystem.symlink_refusal(target).is_some())
    .then_some(
        "the target is a symlink, so selfie will not manage it \
         and records no deployment for it",
    )
}

// Every site that words a refused deploy shares this, so apply, drift and the
// writer cannot describe the same refusal differently. Format it here rather than
// at a call site — no test pins this wrapper at the write site, so a copy there
// could drift unnoticed.
//
// Named as a property rather than counted. The count was "three", and was correct
// until the same change that wrote it added three more call sites — a number in a
// comment is a claim that goes stale on the next edit, in a file whose whole
// subject is claims going stale.
pub(super) fn refusal_warning(source: &str, refusal: &FileSystemError) -> String {
    format!("Skipping '{source}': {refusal}")
}

// Why an entry whose target exists but could not be read is refused, worded the
// same by apply and drift. A symlink whose destination cannot be read is a
// symlinked target first, which is the refusal every command already shares;
// only a plain file gets the read failure.
pub(super) fn unreadable_target_refusal<F: FileSystem>(
    filesystem: &F,
    source: &str,
    target: &TargetPath,
    error: &FileSystemError,
) -> String {
    match filesystem.symlink_refusal(target) {
        Some(refusal) => refusal_warning(source, &refusal),
        None => format!(
            "Skipping '{source}': target '{}' exists but could not be read: {error}",
            target.display()
        ),
    }
}

// The target's bytes if the entry can go on to a decision: `None` for an absent
// target, `Err(warning)` for one that exists and could not be read. Apply and
// drift both classify through here, so they cannot answer differently about one
// file.
pub(super) fn readable_target<F: FileSystem>(
    filesystem: &F,
    source: &str,
    target: &TargetPath,
) -> Result<Option<Vec<u8>>, String> {
    match read_target_state(filesystem, target) {
        TargetState::Absent => Ok(None),
        TargetState::Readable(bytes) => Ok(Some(bytes)),
        TargetState::Unreadable(e) => {
            Err(unreadable_target_refusal(filesystem, source, target, &e))
        }
    }
}

// Why selfie will not read a file out of its own repository.
//
// Reading a fifo blocks until a writer arrives, so one committed into the
// dotfiles directory hangs `selfie apply` and `dotfiles drift` with no timeout --
// `command_timeout` governs provider commands, not filesystem calls (selfie-lwv5).
//
// Returns the reason only; the three read sites frame it differently.
//
// Worded for a *source*. `IrregularTarget`'s own `Display` describes a deploy
// target, and here the problem is a file in the repository the user syncs.
pub(crate) fn repository_read_refusal(refusal: &FileSystemError) -> String {
    match refusal {
        FileSystemError::IrregularTarget { kind, .. } => {
            format!("the repository file is a {kind} and selfie will not read it")
        }
        // Fails **closed**, and deliberately not a `_ => {}` that would skip the
        // guard. `irregular_target_refusal` returns only `IrregularTarget` today,
        // so nothing reaches this arm; a wildcard would silently let a future
        // variant through and un-guard the read, which is the failure this whole
        // guard exists to prevent. Refuse on anything it reports.
        other => format!("selfie will not read the repository file: {other}"),
    }
}

// The three deploy-side sites that refuse a target by the rule: apply's
// secret-bearing path, apply's repository-file path, and drift. `TargetRejection`
// supplies the words so all three say the same thing; this supplies the frame.
pub(super) fn target_refusal(target: &str, rejection: TargetRejection) -> String {
    format!("Skipping '{target}': {}", rejection.message())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    // The `other` arm fails closed. Nothing returns a non-`IrregularTarget`
    // variant from `irregular_target_refusal` today, so this is the only thing
    // holding the arm: hand it one directly and the read must still be refused
    // with something a user can read. A `_ => {}` that skipped the guard would
    // return an empty string here.
    #[test]
    fn a_read_refusal_that_is_not_an_irregular_file_still_refuses() {
        let message = repository_read_refusal(&FileSystemError::SymlinkedTarget {
            path: PathBuf::from("/pkgs/myapp/config.toml"),
            points_to: None,
        });
        assert!(!message.is_empty(), "the guard fell through silently");
        assert!(message.contains("repository file"), "got: {message}");
    }
}
