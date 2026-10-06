//! Writing one entry to its target and recording what was written.
//!
//! The target's former content is copied aside first, when there is anywhere to
//! put it and anything worth keeping. Nothing here decides whether an entry should
//! deploy; it carries out a decision already made.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::{
    dotfile_service::backup,
    fs::{
        filesystem::{FileSystem, FileSystemError},
        target::TargetPath,
    },
    package::event::{DotfileSource, EventSender},
};

use super::refusal::{guard_refusal, read_target_state, readable_or_refusal, refusal_warning};
use super::state_file::{
    LoadedState, Recorder, WritableState, save_deploy_state, stopped_before_writing,
};

/// Describes a single config file deployment operation
pub(super) struct DeployUnit<'a> {
    /// How events name the source.
    pub(super) event_source: &'a DotfileSource,
    pub(super) target_path: &'a TargetPath,
    /// `target_path` as the deploy state keys it.
    pub(super) target_key: &'a str,
    pub(super) source_content: &'a str,
    pub(super) source_checksum: &'a str,
    /// The entry's `source` as the spec names it, recorded beside the checksum.
    pub(super) source: &'a str,
    /// The spec name of the package the entry belongs to, recorded beside the
    /// checksum. `None` for a package with no spec file behind it.
    pub(super) package: Option<&'a str>,
    /// Where copies of overwritten targets go, or `None` if there is nowhere to
    /// put one. `Some` does not mean a copy will be made.
    // A dry run can have a root here and writes nothing: under a preview ledger
    // `deploy_and_record` never reaches the write.
    pub(super) backups: Option<&'a Path>,
}

/// What the target held when the deploy decision was made, and whether that
/// answer is still current when the write comes.
#[derive(Clone, Copy)]
pub(super) enum Decided<'a> {
    /// Nothing waited between the decision and the write: these are the bytes
    /// the write replaces, `None` for a target that was absent.
    Now(Option<&'a [u8]>),
    /// An interactive resolver's prompt ran in between, for as long as the user
    /// took, so the target is looked at and read again before the write.
    BeforePrompt,
}

/// What became of an entry apply decided to write.
pub(super) enum DeployOutcome {
    /// Written, and recorded when there is a state to record it in.
    Deployed,
    /// A dry run: reported as what it would do, and nothing written.
    Previewed,
    /// Refused or failed, and already reported as whichever it was. Nothing was
    /// written or recorded.
    Refused,
    /// The deploy state could not be written: before the target, so nothing was
    /// written, or after it, so the target is unrecorded. Carries why the run
    /// stops.
    Unrecorded(String),
}

/// The deploy state an apply reads, and whether the run writes and records.
///
/// Only a real run holds [`Record`](Self::Record), so only a real run writes a
/// repository-file entry, and every such write has a state to be recorded in.
/// A secret-bearing entry's write is not gated by this type.
pub(super) enum Ledger {
    /// A real run: writes targets, and records each write in this state.
    Record(Recorder),
    /// A dry run: writes and records nothing. Holds the state it previews
    /// against, when one could be read.
    Preview(Option<LoadedState>),
}

impl Ledger {
    /// The state this run reads, if it has one.
    pub(super) fn loaded(&self) -> Option<&LoadedState> {
        match self {
            Ledger::Record(recorder) => Some(recorder.loaded()),
            Ledger::Preview(loaded) => loaded.as_ref(),
        }
    }
}

/// Write `unit` to its target and record it, emitting the events for each step.
///
/// The one path from a decision to deploy to a recorded deployment, whatever
/// made the decision: the entry's own drift, or an accepted conflict. Under a
/// [`Ledger::Preview`] it reports what it would do and writes nothing.
///
/// `backed_up` carries what this run has already copied aside, keyed by target,
/// so a target two entries deploy to is copied once.
pub(super) async fn deploy_and_record<F: FileSystem>(
    filesystem: &F,
    sender: &EventSender,
    ledger: &mut Ledger,
    unit: &DeployUnit<'_>,
    decided: Decided<'_>,
    backed_up: &mut HashMap<String, Option<PathBuf>>,
) -> DeployOutcome {
    let Ledger::Record(recorder) = ledger else {
        sender
            .send_dotfile_skipped(unit.event_source, unit.target_path.display(), "dry run")
            .await;
        return DeployOutcome::Previewed;
    };
    // The state is written back here, ahead of the backup and the target write,
    // so a state that cannot be written stops the run before this entry writes.
    let mut loaded = match recorder.writable(filesystem) {
        Ok(loaded) => loaded,
        Err(failure) => return DeployOutcome::Unrecorded(stopped_before_writing(&failure)),
    };
    match perform_deploy(filesystem, sender, unit, decided, backed_up).await {
        Wrote::Written => {
            match record_and_save(filesystem, &mut loaded, sender, Recorded::Deployed, unit).await {
                Some(reason) => DeployOutcome::Unrecorded(reason),
                None => DeployOutcome::Deployed,
            }
        }
        Wrote::Refused => DeployOutcome::Refused,
    }
}

/// What a write did, before anything is recorded.
enum Wrote {
    Written,
    /// Refused or failed, and already reported as whichever it was.
    Refused,
}

/// Deploy a single config file to its target path and emit events. Records
/// nothing.
async fn perform_deploy<F: FileSystem>(
    filesystem: &F,
    sender: &EventSender,
    unit: &DeployUnit<'_>,
    decided: Decided<'_>,
    backed_up: &mut HashMap<String, Option<PathBuf>>,
) -> Wrote {
    sender
        .send_dotfile_deploying(unit.event_source, unit.target_path.display())
        .await;

    // `kept` is `Some` only for the entry that actually made the copy, so only it
    // prunes, and only once the write below has landed.
    let (backup, kept) = match backed_up.get(unit.target_key) {
        // This run has already settled this target. Report the copy it made --
        // which holds what the target held before the run touched it -- and make
        // no second one. `None` means the run found nothing there to keep.
        //
        // Without this, two entries naming one target destroy the very thing the
        // copy exists for: the first copies the user's file, the second finds the
        // first entry's output, copies that, and the prune deletes the user's.
        // Two entries can name one target -- an apply covers every package, and
        // the only same-target check anywhere looks inside a single package.
        Some(existing) => (existing.clone(), None),
        None => match keep_current(filesystem, unit, decided) {
            Ok(kept) => {
                let path = kept.as_ref().map(|kept| kept.path().to_path_buf());
                // Recorded before the write, not after: a copy that was made and a
                // target write that then failed still holds what the target held,
                // so a later entry for this target must report it.
                backed_up.insert(unit.target_key.to_string(), path.clone());
                (path, kept)
            }
            Err(warning) => {
                sender.send_warning(warning).await;
                // Left out of `backed_up`, so a refused entry does not mark the
                // target as settled for a later one.
                return Wrote::Refused;
            }
        },
    };

    // Refuses a symlinked target rather than writing through it: the content would
    // otherwise land wherever the link points, which may be a path chosen by
    // whoever planted it.
    if let Err(e) =
        filesystem.write_file_no_follow(unit.target_path, unit.source_content.as_bytes())
    {
        // A refusal is not a failure. "Failed to write" would read as something
        // going wrong rather than as selfie declining. The error names the target
        // in both arms, so neither repeats it.
        //
        // Reaching the refusal arm here means the link or fifo appeared between
        // `classify_entry`'s checks and this write. It is exercised by
        // `the_writer_refuses_even_when_the_check_is_blinded`, which asserts only
        // that the message names a symlink — not the `Skipping '{source}': `
        // wrapper. Share `refusal_warning` rather than repeating the wording, or
        // that unpinned half can drift.
        let message = match &e {
            FileSystemError::SymlinkedTarget { .. } | FileSystemError::IrregularTarget { .. } => {
                refusal_warning(unit.source, &e)
            }
            _ => format!("Failed to write: {e}"),
        };
        sender.send_warning(message).await;
        // `Refused` has the caller count this as refused and record nothing, so
        // nothing is recorded as deployed that was not. An entry already in the
        // state keeps its previous checksum and is stale rather than untracked,
        // which is the honest record: a refusal writes nothing, and a failed write
        // leaves the target as it was, so the previous checksum still describes it.
        return Wrote::Refused;
    }

    // Only now that the overwrite has landed is an earlier copy redundant. Before
    // this point it may be the only record of content the target no longer holds,
    // while this run's copy holds what is still at the target -- so pruning on the
    // way to a write that then fails trades the irreplaceable for a duplicate.
    // Both returns above therefore leave two copies, and the next successful
    // overwrite reduces them to one. Do not delete either on the way out: a delete
    // path fails too, and losing a copy is worse than keeping a redundant one.
    if let Some(kept) = kept
        && let Some(stale) = kept.prune_earlier(filesystem)
    {
        sender.send_warning(stale).await;
    }

    sender
        .send_dotfile_deployed(
            unit.event_source,
            unit.target_path.display(),
            backup.as_deref(),
        )
        .await;
    Wrote::Written
}

/// Copy the target's content aside, if this overwrite would destroy any.
///
/// `Ok(None)` when there is nothing to keep: nowhere to put a copy, no target,
/// or the target already holds what is about to be written.
///
/// # Errors
///
/// The warning to report, when the target cannot be read or the copy cannot be
/// written. Nothing has been written to the target in either case.
fn keep_current<F: FileSystem>(
    filesystem: &F,
    unit: &DeployUnit<'_>,
    decided: Decided<'_>,
) -> Result<Option<backup::Kept>, String> {
    let Some(root) = unit.backups else {
        return Ok(None);
    };

    let current = match decided {
        Decided::Now(current) => current.map(<[u8]>::to_vec),
        // Read again rather than reuse the bytes the decision was made from: the
        // earlier read is what the user was shown, and this one is what the write
        // is about to destroy. Copying the first would keep bytes that are still
        // reachable and lose the ones that are not.
        Decided::BeforePrompt => {
            // The guard first, as before every read of a target: a fifo or device
            // node put in place during the prompt must not be opened at all.
            if let Some(refusal) = guard_refusal(filesystem, unit.target_path) {
                return Err(refusal_warning(unit.source, &refusal));
            }
            readable_or_refusal(
                unit.source,
                unit.target_path,
                read_target_state(filesystem, unit.target_path),
            )?
        }
    };
    // Nothing there: nothing to keep, and the write creates it.
    let Some(current) = current else {
        return Ok(None);
    };

    // Decided from the bytes rather than from the deploy decision. A refresh whose
    // repository file changed is `Deploy` and overwrites differing content with no
    // prompt at all, so gating on an accepted conflict would leave the commonest
    // overwrite uncovered.
    if current == unit.source_content.as_bytes() {
        return Ok(None);
    }

    backup::keep(filesystem, root, unit.target_key, &current)
        .map(Some)
        .map_err(|e| backup::refusal(unit.source, unit.target_path.path(), &e))
}

/// What an apply just recorded about a target.
#[derive(Clone, Copy)]
pub(super) enum Recorded {
    /// The target was written.
    Deployed,
    /// The target already matched its source and was left alone.
    InSync,
}

impl std::fmt::Display for Recorded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Recorded::Deployed => write!(f, "Deployed"),
            Recorded::InSync => write!(f, "Found in sync"),
        }
    }
}

/// Record `unit` in the loaded state and write the state back. On failure,
/// warns with what happened to the target and returns the reason the run stops.
// Only a real run's ledger hands out the state this takes. The state is written
// after every record, so a run that cannot write it has recorded everything
// before the failing entry. Stopping on the first failure keeps the
// unrecorded set to one entry: a state directory that refused this write
// refuses the next one too. An unrecorded target is re-evaluated by the next
// run as untracked: one whose content still matches its source is recorded
// silently through the in-sync skip arm, and only one whose source has
// changed since is asked about.
pub(super) async fn record_and_save<F: FileSystem>(
    filesystem: &F,
    loaded: &mut WritableState<'_>,
    sender: &EventSender,
    recorded: Recorded,
    unit: &DeployUnit<'_>,
) -> Option<String> {
    // With a base, the record holds the path relative to it, which is what the
    // base means; the spec's spelling is relative to the spec file instead.
    let (source, base) = match unit.event_source {
        DotfileSource::File {
            base: Some(base),
            path,
            ..
        } => (path.to_string_lossy(), Some(base.kind)),
        _ => (std::borrow::Cow::Borrowed(unit.source), None),
    };
    loaded.state_mut().record_deployment(
        unit.target_key,
        &source,
        unit.source_checksum,
        unit.package,
        base,
    );
    let Err(e) = save_deploy_state(filesystem, loaded) else {
        return None;
    };
    // `e` already names the state file, so the message does not repeat it.
    sender
        .send_warning(format!(
            "{recorded} '{}' but cannot record it: {e}",
            unit.target_path.display()
        ))
        .await;
    Some(format!(
        "Stopped after failing to record '{}' in the deploy state; the next run re-evaluates \
         it once the state can be written",
        unit.target_path.display()
    ))
}
