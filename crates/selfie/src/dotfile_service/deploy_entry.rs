//! Writing one entry to its target and recording what was written.
//!
//! The target's former content is copied aside first, when there is anywhere to
//! put it and anything worth keeping. Nothing here decides whether an entry should
//! deploy; it carries out a decision already made.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::{
    dotfile_service::{appeared, backup, diff::unified_diff},
    fs::{
        filesystem::{FileSystem, FileSystemError, IfPresent, Placed},
        target::TargetPath,
    },
    package::event::{
        ConflictReport, DotfileSource, EntryOperation, EventSender, Failure, Refusal, RepoPath,
    },
};

use super::apply::EntryOutcome;
use super::refusal::{
    classify_write, guard_refusal, read_target_state, readable_or_refusal, refusal_for,
};
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
    /// The name of the package the entry belongs to, for a refusal to name.
    pub(super) package_name: &'a str,
    /// The entry's target as the package file spells it, for a refusal to name.
    pub(super) entry_target: &'a str,
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

/// Whether a write overwrites a conflict the run's resolver accepted, which its
/// deployed event reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Overwrite {
    /// The resolver accepted a conflict at the target.
    Accepted,
    /// Nobody was asked: there was no conflict, or `--yes` answered for the
    /// user.
    Unasked,
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
/// `overwrite` says whether a resolver accepted a conflict to reach this write,
/// which the deployed event reports. `backed_up` carries what this run
/// has already copied aside, keyed by target, so a target two entries deploy
/// to is copied once.
pub(super) async fn deploy_and_record<F: FileSystem>(
    filesystem: &F,
    sender: &EventSender,
    ledger: &mut Ledger,
    unit: &DeployUnit<'_>,
    decided: Decided<'_>,
    overwrite: Overwrite,
    backed_up: &mut HashMap<String, Option<PathBuf>>,
) -> EntryOutcome {
    let Ledger::Record(recorder) = ledger else {
        sender
            .send_dotfile_skipped(
                unit.event_source,
                unit.target_path.display(),
                crate::package::event::SkipReason::DryRun,
            )
            .await;
        return EntryOutcome::Skipped;
    };
    // The state is written back here, ahead of the backup and the target write,
    // so a state that cannot be written stops the run before this entry writes.
    let mut loaded = match recorder.writable(filesystem) {
        Ok(loaded) => loaded,
        Err(failure) => return EntryOutcome::Unrecorded(stopped_before_writing(&failure)),
    };
    match perform_deploy(filesystem, sender, unit, decided, overwrite, backed_up).await {
        Wrote::Written => {
            match record_and_save(filesystem, &mut loaded, sender, Recorded::Deployed, unit).await {
                Some(reason) => EntryOutcome::Unrecorded(reason),
                None => EntryOutcome::Deployed,
            }
        }
        Wrote::Refused => EntryOutcome::Refused,
        Wrote::Failed => EntryOutcome::Failed,
        Wrote::Kept => {
            appeared::settle(&mut RepoAppeared {
                filesystem,
                sender,
                loaded: &mut loaded,
                unit,
            })
            .await
        }
    }
}

/// What a write did, before anything is recorded.
enum Wrote {
    Written,
    /// Refused, and already reported.
    Refused,
    /// The copy or the write failed, and was already reported.
    Failed,
    /// Something appeared at a target last read as absent, and the write left it
    /// in place. Nothing is reported yet.
    Kept,
}

/// A repository-file entry whose write left in place what appeared at its target.
struct RepoAppeared<'a, 'w, F> {
    filesystem: &'a F,
    sender: &'a EventSender,
    loaded: &'a mut WritableState<'w>,
    unit: &'a DeployUnit<'a>,
}

impl<F: FileSystem> appeared::Appeared for RepoAppeared<'_, '_, F> {
    type Outcome = EntryOutcome;

    fn content(&self) -> &[u8] {
        self.unit.source_content.as_bytes()
    }

    fn target(&self) -> &TargetPath {
        self.unit.target_path
    }

    fn sender(&self) -> &EventSender {
        self.sender
    }

    async fn reread(&mut self) -> Result<Option<Vec<u8>>, EntryOutcome> {
        match last_read(self.filesystem, self.unit, Decided::BeforePrompt) {
            Ok(current) => Ok(current.map(Cow::into_owned)),
            Err(refusal) => {
                self.sender
                    .send_dotfile_refused(self.unit.package_name, self.unit.entry_target, refusal)
                    .await;
                Err(EntryOutcome::Refused)
            }
        }
    }

    async fn failed(&mut self, failure: Failure) -> EntryOutcome {
        self.sender
            .send_dotfile_failed(self.unit.package_name, self.unit.entry_target, failure)
            .await;
        EntryOutcome::Failed
    }

    // Recorded in sync even for a tracked entry whose record differs, which the
    // next run would call a conflict: the target holds exactly the source.
    async fn in_sync(&mut self) -> EntryOutcome {
        if let Some(reason) = record_and_save(
            self.filesystem,
            self.loaded,
            self.sender,
            Recorded::InSync,
            self.unit,
        )
        .await
        {
            return EntryOutcome::Unrecorded(reason);
        }
        self.sender
            .send_dotfile_skipped(
                self.unit.event_source,
                self.unit.target_path.display(),
                crate::package::event::SkipReason::InSync,
            )
            .await;
        EntryOutcome::Skipped
    }

    async fn conflict(&mut self, current: &[u8]) -> EntryOutcome {
        self.sender
            .send_dotfile_conflict(
                self.unit.event_source,
                self.unit.target_path.display(),
                ConflictReport::Diff(conflict_diff(current, self.unit)),
                false,
            )
            .await;
        EntryOutcome::Conflicted
    }
}

/// Deploy a single config file to its target path and emit events. Records
/// nothing.
async fn perform_deploy<F: FileSystem>(
    filesystem: &F,
    sender: &EventSender,
    unit: &DeployUnit<'_>,
    decided: Decided<'_>,
    overwrite: Overwrite,
    backed_up: &mut HashMap<String, Option<PathBuf>>,
) -> Wrote {
    sender
        .send_dotfile_deploying(unit.event_source, unit.target_path.display())
        .await;

    // `kept` is `Some` only for the entry that actually made the copy, so only it
    // prunes, and only once the write below has landed.
    let mut inserted = false;
    let (backup, kept, if_present) = match backed_up.get(unit.target_key) {
        // This run has already settled this target. Report the copy it made --
        // which holds what the target held before the run touched it -- and make
        // no second one. `None` means the run found nothing there to keep.
        //
        // Without this, two entries naming one target destroy the very thing the
        // copy exists for: the first copies the user's file, the second finds the
        // first entry's output, copies that, and the prune deletes the user's.
        // Two entries can name one target -- an apply covers every package, and
        // the only same-target check anywhere looks inside a single package.
        Some(existing) => (
            existing.clone(),
            None,
            // Not read again, so a decision that found the target absent decides.
            if_present(matches!(decided, Decided::Now(None))),
        ),
        None => {
            let current = match last_read(filesystem, unit, decided) {
                Ok(current) => current,
                Err(refusal) => {
                    sender
                        .send_dotfile_refused(unit.package_name, unit.entry_target, refusal)
                        .await;
                    return Wrote::Refused;
                }
            };
            match keep_current(filesystem, unit, current.as_deref()) {
                Ok(kept) => {
                    let path = kept.as_ref().map(|kept| kept.path().to_path_buf());
                    // Recorded before the write, not after: a copy that was made and a
                    // target write that then failed still holds what the target held,
                    // so a later entry for this target must report it.
                    backed_up.insert(unit.target_key.to_string(), path.clone());
                    inserted = true;
                    (path, kept, if_present(current.is_none()))
                }
                // Left out of `backed_up`, so an entry that kept no copy does not mark
                // the target as settled for a later one.
                Err(failure) => {
                    sender
                        .send_dotfile_failed(unit.package_name, unit.entry_target, failure)
                        .await;
                    return Wrote::Failed;
                }
            }
        }
    };

    // Refuses a symlinked target rather than writing through it: the content would
    // otherwise land wherever the link points, which may be a path chosen by
    // whoever planted it.
    let written = filesystem.write_file_no_follow(
        unit.target_path,
        unit.source_content.as_bytes(),
        if_present,
    );
    if let Ok(Placed::Kept) = written {
        // The `None` this entry recorded says the run found nothing to keep at a
        // target that now holds a file, so it goes.
        if inserted {
            backed_up.remove(unit.target_key);
        }
        return Wrote::Kept;
    }
    if let Err(e) = written {
        // Either way nothing is recorded as deployed that was not. An entry already
        // in the state keeps its previous checksum and is stale rather than
        // untracked, which is the honest record: a refusal writes nothing, and a
        // failed write leaves the target as it was, so the previous checksum still
        // describes it. The error names the target in both arms, so neither
        // repeats it.
        return match classify_write(unit.source, unit.target_path, &e) {
            // A refusal is not a failure: "Failed to write" would read as something
            // going wrong rather than as selfie declining. Reaching this arm means
            // what refused the write appeared after `classify_entry`'s checks.
            Some(refusal) => {
                sender
                    .send_dotfile_refused(unit.package_name, unit.entry_target, refusal)
                    .await;
                Wrote::Refused
            }
            None => {
                sender
                    .send_dotfile_failed(unit.package_name, unit.entry_target, write_failure(&e))
                    .await;
                Wrote::Failed
            }
        };
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
            overwrite == Overwrite::Accepted,
        )
        .await;
    Wrote::Written
}

/// How a write treats anything at its target when it lands: kept when the last
/// read found the target `absent`, replaced otherwise.
// Replaces nothing when the last read found nothing, so a file that appeared
// since is kept rather than destroyed without a copy.
fn if_present(absent: bool) -> IfPresent {
    if absent {
        IfPresent::Keep
    } else {
        IfPresent::Replace
    }
}

/// Copy the target's content aside, if this overwrite would destroy any.
///
/// `Ok(None)` when there is nothing to keep: nowhere to put a copy, no target,
/// or the target already holds what is about to be written.
///
/// # Errors
///
/// The failure of the copy, which could not be written. Nothing has been
/// written to the target.
fn keep_current<F: FileSystem>(
    filesystem: &F,
    unit: &DeployUnit<'_>,
    current: Option<&[u8]>,
) -> Result<Option<backup::Kept>, Failure> {
    let Some(root) = unit.backups else {
        return Ok(None);
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

    backup::keep(filesystem, root, unit.target_key, current)
        .map(Some)
        .map_err(|e| backup::failure(unit.target_path.path(), &e))
}

/// What the target holds as its write is about to happen: `None` for nothing
/// there.
///
/// # Errors
///
/// The refusal of the entry for what a read made now found instead of a file.
fn last_read<'d, F: FileSystem>(
    filesystem: &F,
    unit: &DeployUnit<'_>,
    decided: Decided<'d>,
) -> Result<Option<Cow<'d, [u8]>>, Refusal> {
    match decided {
        Decided::Now(current) => Ok(current.map(Cow::Borrowed)),
        // Read again rather than reuse the bytes the decision was made from: the
        // earlier read is what the user was shown, and this one is what the write
        // is about to destroy. Copying the first would keep bytes that are still
        // reachable and lose the ones that are not.
        Decided::BeforePrompt => {
            // The guard first, as before every read of a target: a fifo or device
            // node put in place during the prompt must not be opened at all.
            if let Some(refusal) = guard_refusal(filesystem, unit.target_path) {
                return Err(refusal_for(unit.source, &refusal));
            }
            readable_or_refusal(
                unit.source,
                unit.target_path,
                read_target_state(filesystem, unit.target_path),
            )
            .map(|current| current.map(Cow::Owned))
        }
    }
}

/// The diff a conflict between `current`, the target's bytes, and `unit`'s
/// source shows.
pub(super) fn conflict_diff(current: &[u8], unit: &DeployUnit<'_>) -> String {
    // Lossy only for display: the decision compared the raw bytes.
    unified_diff(
        &String::from_utf8_lossy(current),
        unit.source_content,
        &unit.target_path.display().to_string(),
        // Relative, as the line above the diff names it.
        &unit.event_source.relative().to_string(),
    )
}

/// The failure of a write to a target, named by `error`.
pub(super) fn write_failure(error: &FileSystemError) -> Failure {
    // The error already names the target; naming it in the head too would print
    // the path twice.
    Failure::new(EntryOperation::Write, "Failed to write", error)
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
        DotfileSource::File(RepoPath {
            base: Some(base),
            path,
        }) => (path.to_string_lossy(), Some(base.kind)),
        // A template is never recorded; it takes the secret-bearing path.
        DotfileSource::File(RepoPath { base: None, .. })
        | DotfileSource::Template { .. }
        | DotfileSource::Command(_)
        | DotfileSource::Recorded(_) => (std::borrow::Cow::Borrowed(unit.source), None),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{MockFileSystem, target::repository_path};
    use crate::package::event::{OperationContext, OperationType};

    const KEY: &str = "/home/me/.config/app.toml";

    fn sender() -> EventSender {
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        EventSender::new_with_context(
            tx,
            OperationType::DotfileApply,
            String::new(),
            String::new(),
            OperationContext::default(),
        )
    }

    // Deploy one repository file to `KEY`, which the decision found absent, through
    // `fs`, with `backed_up` as the run has it so far.
    async fn deploy_absent(
        fs: &MockFileSystem,
        backed_up: &mut HashMap<String, Option<PathBuf>>,
    ) -> Wrote {
        let target = repository_path(Path::new(KEY));
        let source = DotfileSource::File(RepoPath {
            base: None,
            path: PathBuf::from("/repo/app.toml"),
        });
        let unit = DeployUnit {
            event_source: &source,
            target_path: &target,
            target_key: KEY,
            source_content: "content",
            source_checksum: "checksum",
            source: "app.toml",
            package: None,
            package_name: "app",
            entry_target: "~/.config/app.toml",
            backups: Some(Path::new("/state/backups")),
        };
        perform_deploy(
            fs,
            &sender(),
            &unit,
            Decided::Now(None),
            Overwrite::Unasked,
            backed_up,
        )
        .await
    }

    // A target an earlier entry already settled is written without reading it
    // again, so its mode comes from the decision: one that found it absent keeps
    // what is there. Two entries reach this arm only when they share a
    // deploy-state key, which apply refuses as a collision first, so it is held
    // here rather than through apply.
    #[tokio::test]
    async fn a_target_already_settled_and_last_read_as_absent_keeps_what_is_there() {
        let mut fs = MockFileSystem::new();
        fs.expect_write_file_no_follow()
            .withf(|_, _, if_present| *if_present == IfPresent::Keep)
            .times(1)
            .returning(|_, _, _| Ok(Placed::Written));
        let mut backed_up = HashMap::from([(KEY.to_string(), None)]);

        let wrote = deploy_absent(&fs, &mut backed_up).await;

        assert!(matches!(wrote, Wrote::Written));
    }

    // The `None` an entry records says the run found nothing to keep at the target.
    // Once something has appeared there, that is false, so the entry takes it back.
    #[tokio::test]
    async fn a_write_that_kept_what_appeared_leaves_the_target_unsettled() {
        let mut fs = MockFileSystem::new();
        fs.expect_write_file_no_follow()
            .times(1)
            .returning(|_, _, _| Ok(Placed::Kept));
        let mut backed_up = HashMap::new();

        let wrote = deploy_absent(&fs, &mut backed_up).await;

        assert!(matches!(wrote, Wrote::Kept));
        assert!(!backed_up.contains_key(KEY), "{backed_up:?}");
    }

    // What an earlier entry recorded is that entry's, and still true: its copy
    // holds what the target held before the run touched it.
    #[tokio::test]
    async fn a_write_that_kept_what_appeared_leaves_the_earlier_copy_recorded() {
        let mut fs = MockFileSystem::new();
        fs.expect_write_file_no_follow()
            .times(1)
            .returning(|_, _, _| Ok(Placed::Kept));
        let copy = PathBuf::from("/state/backups/app.toml/copy");
        let mut backed_up = HashMap::from([(KEY.to_string(), Some(copy.clone()))]);

        let wrote = deploy_absent(&fs, &mut backed_up).await;

        assert!(matches!(wrote, Wrote::Kept));
        assert_eq!(backed_up.get(KEY), Some(&Some(copy)), "{backed_up:?}");
    }
}
