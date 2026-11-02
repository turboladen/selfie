//! Taking a file the user already has under management.
//!
//! Copies the file into the repository beside its spec, adds the entry, saves the
//! spec, and records the deployment. Every step that can fail says what became of
//! the copy the step before it wrote.

use std::path::{Path, PathBuf};

use crate::{
    config::SelfieConfig,
    dotfile_service::deploy::compute_checksum,
    fs::{
        filesystem::{FileSystem, FileSystemError, IfPresent, Placed, Removal},
        target::{
            HomeDir, TargetPath, TargetRejection, deploy_target, expand_target_path,
            portable_target, repository_path,
        },
    },
    package::{
        DotfileEntry, Package, SpecOrigin,
        event::{EventSender, OperationFailure, OperationResult, OperationSuccess, StepCount},
        port::{PackageRepoError, PackageRepository},
    },
    parts::{Composed, Message, Parts, SuggestedCommand},
    paths::{is_within, normalize_path},
};

use super::refusal::{TargetState, directory_at_target, guard_refusal, read_target_state};
use super::state_file::{
    DeployStateFailure, Recorder, StateLoad, load_deploy_state, save_deploy_state,
};
use crate::package::collect::unchecked::{Checking, Others};

/// A spec that already has an entry for a target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tracker {
    /// The name lookup finds the spec by: its file's stem, as the file spells it.
    pub spec_name: String,
    /// The spec file, which is what a user opens to change the entry.
    pub spec_path: PathBuf,
    /// The entry's target as the spec spells it.
    pub target: String,
    /// Which directory the spec was read from.
    pub origin: SpecOrigin,
}

impl Tracker {
    /// The spec's label as a sentence names it, such as `package 'ruby'` or
    /// `standalone dotfile 'gemrc'`, as parts with the spec's name as a name.
    #[must_use]
    pub(crate) fn label(&self) -> Parts<'_> {
        let kind = match self.origin {
            SpecOrigin::DotfilesDirectory => "standalone dotfile '",
            SpecOrigin::PackageDirectory | SpecOrigin::Memory => "package '",
        };
        Parts::new()
            .text(kind)
            .name(self.spec_name.as_str())
            .text("'")
    }
}

/// Every spec among `packages` with an entry for `target`, in any environment or
/// none, one per spec.
///
/// Targets are compared once `~` is expanded against `home` and `.` and `..` are
/// resolved as text, so `~/.gemrc` and its absolute form are one target.
pub fn trackers_of<H: HomeDir + ?Sized>(
    packages: &[Package],
    home: &H,
    target: &TargetPath,
) -> Vec<Tracker> {
    // Every scope, not only the current environment's: an entry under another
    // environment deploys the file on the machines using it, and nothing across
    // specs overrides another spec's entry.
    packages
        .iter()
        .filter_map(|package| {
            let (_, entry) = package
                .dotfiles_with_scope()
                .into_iter()
                .find(|(_, entry)| expand_target_path(home, entry.target()) == *target)?;
            let spec_name = package
                .spec_name_as_spelled()
                .unwrap_or_else(|| package.name().to_string());
            Some(Tracker {
                spec_name,
                spec_path: package.path().clone(),
                target: entry.target().to_string(),
                origin: package.origin(),
            })
        })
        .collect()
}

// The refusal of a track whose file another spec already deploys.
fn tracked_elsewhere_refusal(target: &str, trackers: &[Tracker]) -> Message {
    let named = Parts::joined(trackers, " and ", |tracker| {
        tracker
            .label()
            .text(" (")
            .path(tracker.spec_path.as_path())
            .text(")")
    });
    let remedy = match trackers {
        [
            Tracker {
                spec_name,
                origin: SpecOrigin::PackageDirectory,
                ..
            },
        ] if let Some(command) = SuggestedCommand::track_dotfile(spec_name, target) => Parts::new()
            .text("Track it there with `")
            .command(command)
            .text("`, or remove that entry first."),
        _ => Parts::new().text("Remove the entry from the spec that should not deploy it first."),
    };
    Parts::new()
        .text("Cannot track '")
        .path(Path::new(target))
        .text("': ")
        .append(named)
        .text(" already deploys it, and two specs deploying one file are both refused by apply. ")
        .append(remedy)
        .into()
}

/// Why a name cannot be a directory under the repository, or `None` if it can.
fn unsafe_name_failure(name: &str) -> Option<OperationFailure> {
    if crate::package::is_valid_spec_name(name) {
        return None;
    }
    Some(OperationFailure::Track(
        Parts::new()
            .text("Invalid name '")
            .name(name)
            .text("': ")
            .text(crate::package::SPEC_NAME_RULE)
            .into(),
    ))
}

/// Why `name` cannot be the copy directory beside `spec_path`, or `None` if it can.
///
/// The directory must sit exactly one component below the spec's own, which rules
/// out both a name that climbs out and one that resolves to the spec's directory
/// itself.
// A name reaches the package path from a spec file in the repository, and
// `spec_name_from_file_name` splits on the last dot: `...yml` yields `..` and
// `..yml` yields `.`. The first writes the copy outside the package directory and
// records a `source:` starting `../` that apply's containment guard then refuses
// forever; the second writes it beside the specs and records a `source:` naming a
// directory that is not there.
//
// The spec-name rule refuses both stems at load as well. This guard asks about
// position instead of characters, so it holds even if that rule admits a name
// that climbs out.
fn unusable_copy_directory(spec_path: &Path, name: &str) -> Option<OperationFailure> {
    let refuse = |why: Parts<'_>| {
        Some(OperationFailure::Track(
            Parts::new()
                .text("Cannot track into '")
                .name(name)
                .text("': ")
                .append(why)
                .into(),
        ))
    };

    // A spec with no parent cannot have a directory beside it. `Path::parent` is
    // `None` only for a root or an empty path, and an empty one also yields `Some`
    // for a bare file name, so both are refused here rather than defaulted to `.`:
    // defaulting would compose the copy against the process's working directory.
    let Some(base_dir) = spec_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return refuse(Parts::new().text("its spec has no directory to write beside"));
    };

    let composed = base_dir.join(name);
    if !is_within(&composed, base_dir) {
        return refuse(
            Parts::new()
                .text("the copy would be written outside '")
                .path(base_dir)
                .text("'"),
        );
    }
    if normalize_path(&composed) == normalize_path(base_dir) {
        return refuse(
            Parts::new()
                .text("the copy would be written straight into '")
                .path(base_dir)
                .text("' rather than a directory of its own"),
        );
    }
    None
}

// Both track handlers word a refused track. Same `FileSystemError` apply renders,
// plus the remedy that only applies while the entry does not exist yet.
fn track_refusal(refusal: &FileSystemError) -> Message {
    refusal
        .parts()
        .text(". Replace the symlink with a regular file, or track the path it points to.")
        .into()
}

// Both track handlers word a refused copy *into* the dotfiles repository.
//
// Destructures rather than rendering the `FileSystemError`: every variant says
// "target" in its `Display`, meaning the path selfie deploys out to. This path is
// the reverse -- selfie is copying the user's file in, to a path it composed --
// so interpolating the error would send the user to inspect the wrong file.
//
// The remedy differs from `track_refusal`'s for the same reason: what the user
// can do here is clear the repository path or pick another name.
fn repository_write_refusal(source_path: &Path, refusal: &FileSystemError) -> Message {
    let what = match refusal {
        FileSystemError::SymlinkedTarget { points_to, .. } => match points_to {
            Some(dest) => Parts::new()
                .text("it is a symlink to '")
                .path(dest.as_path())
                .text("'"),
            None => Parts::new().text("it is a symlink"),
        },
        FileSystemError::IrregularTarget { kind, .. } => {
            Parts::new().text(format!("it is a {kind}"))
        }
        // Not a refusal: a permission problem, a full disk. Rendered as-is,
        // because the filesystem's own message is the useful one and it makes no
        // claim about a target.
        other => {
            return Parts::new()
                .text("Cannot write source file: ")
                .append(other.parts())
                .into();
        }
    };

    // "the tracked copy at" rather than naming a directory: `handle_track_
    // standalone` composes this under `dotfiles_directory` and
    // `handle_track_for_package` alongside the package YAML, so any sentence
    // naming one of the two is wrong at the other call site.
    Parts::new()
        .text("Cannot write the tracked copy at '")
        .path(source_path)
        .text("': ")
        .append(what)
        .text(". Remove it, or track under a different name.")
        .into()
}

/// Why an already-tracked target cannot be written to, or `None` if it can.
///
/// Asks both stats in the order the answer depends on and renders the sentence for
/// whichever answers. `None` is the ordinary case and says nothing is wrong with the
/// target.
// Ask this rather than composing the two questions at a call site: which one
// answers first is a rule of the dotfile service, and an adapter restating it can
// drift from it.
//
// One answer, not both: a symlink to a socket satisfies each check and would
// otherwise warn twice with the same sentence. `guard_refusal` also skips the second
// stat when the first already answered.
pub fn already_tracked_refusal<F: FileSystem>(
    filesystem: &F,
    target: &TargetPath,
) -> Option<Message> {
    guard_refusal(filesystem, target)
        .as_ref()
        .map(already_tracked_refusal_warning)
}

/// How every command reports an already-tracked target it cannot write to.
// A target already in the spec that is not a regular file.
//
// Not `track_refusal`, whose remedy is about creating an entry: "track the path
// it points to" describes something the user cannot do once the entry exists.
//
// Claims nothing about what a later apply does, because that differs by entry
// kind: a repository-file entry is refused, while a secret-bearing one is written
// by `write_file_private`, which replaces a symlink at the final component. This
// breaks the silence and leaves the verdict to the command that has one.
fn already_tracked_refusal_warning(refusal: &FileSystemError) -> Message {
    refusal
        .parts()
        .text(". The entry stays as it is, and this command wrote nothing.")
        .into()
}

// Where an entry's repository file sits, for the already-tracked answer.
//
// `source` is relative to the spec's own directory, which is the same rule the
// copy is composed under. A provider entry has no file in the repository, so
// there is nothing to resolve and the spec itself is the closest true answer.
fn tracked_copy_path(spec_path: &Path, entry: &DotfileEntry) -> PathBuf {
    let spec_dir = spec_path.parent().unwrap_or_else(|| Path::new("."));
    match entry.source() {
        Some(source) => spec_dir.join(source),
        None => spec_path.to_path_buf(),
    }
}

/// What became of a copy a track had written, once a later step failed.
enum CopyRemoval {
    /// Gone. The path still held what this call wrote.
    Removed,
    /// Left alone: the path could not be confirmed to hold what this call wrote,
    /// so removing it might have deleted a file selfie did not create.
    Unconfirmed,
    /// Removal was attempted on selfie's own copy and failed.
    Failed(FileSystemError),
}

// Remove a copy this call wrote, and only that.
//
// The guard before the copy is advisory: it answers about the path at the moment
// it is asked, and the write and this removal are two later moments. Between them
// something else can occupy the path -- concurrent selfie runs are unsupported,
// but "unsupported" is not "cannot happen", and the cost of being wrong here is
// deleting a file that belongs to someone else. So ownership is established by
// content rather than assumed from the earlier guard.

// The file system compares and removes in one call, the rule a spec removal
// uses too. Its look is still one moment before the unlink, so this narrows the
// window rather than closing it. What it buys is that the ordinary case is
// provably selfie's own file and every other case is reported instead of acted
// on, which is the safe direction for a delete.
fn remove_own_copy<F: FileSystem>(filesystem: &F, path: &Path, written: &str) -> CopyRemoval {
    match filesystem.remove_file_if_holding(&repository_path(path), written.as_bytes()) {
        Ok(Removal::Removed) => CopyRemoval::Removed,
        // Content that differs, or anything but a regular file, is not selfie's
        // to delete.
        Ok(Removal::Kept) => CopyRemoval::Unconfirmed,
        // Content it could not read back, or a removal that failed, leaves the
        // file, and the error says which.
        Err(e) => CopyRemoval::Failed(e),
    }
}

// What became of the copy, worded once so the two failures that compensate cannot
// describe the same outcome differently.
fn copy_fate<'a>(copy: &'a Path, removal: &'a CopyRemoval) -> Parts<'a> {
    let the_copy = Parts::new().text("The copy at '").path(copy);
    match removal {
        CopyRemoval::Removed => the_copy.text("' was removed."),
        CopyRemoval::Unconfirmed => the_copy.text(
            "' was left alone: it no longer holds what selfie wrote, so removing it could have \
             deleted another file. Check it, and remove it yourself if it is not wanted.",
        ),
        CopyRemoval::Failed(e) => the_copy
            .text("' could not be checked or removed: ")
            .append(e.parts())
            .text(
                ". Check it, and remove it yourself before retrying if it is the copy selfie \
                 wrote.",
            ),
    }
}

// Why a track could not save the spec, and what became of the copy it had
// already written.
//
// Does not name the spec: every variant reaching here names it already -- the
// rewrite refusals by construction, and a write failure through the writer's own
// error -- and a message repeating a path the error carries reads twice as long
// as it is. `a_failed_spec_save_names_the_spec_exactly_once` holds that by
// counting rather than by checking presence, so a variant that stops naming the
// spec fails a test instead of going quiet.
fn spec_save_failure(error: &PackageRepoError, copy: &Path, removal: &CopyRemoval) -> Message {
    // A file system error and a listing error append their parts, so the path each
    // names is a path part. Every other repository error enters as its own words,
    // and a path it names stays inside that text until PackageRepoError is
    // Composed (selfie-z00o).
    let cause = match error {
        PackageRepoError::FileSystemError(error) => Parts::new()
            .text("File system error: ")
            .append(error.parts()),
        PackageRepoError::PackageListError(error) => error.parts(),
        other => Parts::new().text(other.to_string()),
    };
    // Several errors end their own sentence, so the frame does not add a second
    // period. Only one is stripped: an error ending in an ellipsis keeps it.
    Parts::new()
        .text("Cannot save the spec: ")
        .append(cause.without_final('.'))
        .text(". ")
        .append(copy_fate(copy, removal))
        .into()
}

// The same rule refused at track time, where it is a failure rather than a
// skipped entry and the remedy is worth stating -- the user is standing at the
// path they named and can retype it. Sibling of `track_refusal` above.
fn track_target_refusal(target: &str, rejection: TargetRejection) -> Message {
    // The stop between the two belongs here rather than on `message()`: that one
    // also reads mid-sentence after "Dotfile " and "Skipping 'X': ", where a
    // trailing period would be wrong.
    Parts::new()
        .text("Cannot track '")
        .path(Path::new(target))
        .text("': ")
        .text(rejection.message())
        .text(". ")
        .text(rejection.suggestion())
        .into()
}

// The result of a track that `message` refuses or fails.
fn track_failed(message: impl Into<Message>) -> OperationResult {
    OperationResult::Failure(OperationFailure::Track(message.into()))
}

/// Which of the two specs a track writes into.
///
/// The only difference between the two entry points once their setup is done.
enum SpecKind {
    /// A spec this track creates. A file already at its path refuses the track.
    New,
    /// A spec that already exists and was loaded, whose entries this track's
    /// target may already be one of.
    Existing,
}

/// The spec a track writes, before this track's own entry is added to it.
struct TrackSpec {
    /// Names the spec, and the directory inside the spec's own that the copy
    /// goes in.
    name: String,
    spec_path: PathBuf,
    package: Package,
    kind: SpecKind,
}

/// Handle `track_standalone`: copy the target file into the dotfiles directory,
/// create a new YAML spec, and record initial deploy state.
pub(super) async fn handle_track_standalone<R, F>(
    name: &str,
    target_path: &str,
    others: impl FnOnce() -> Others,
    dotfiles_repo: &R,
    filesystem: &F,
    sender: &EventSender,
    config: &SelfieConfig,
) -> OperationResult
where
    R: PackageRepository,
    F: FileSystem,
{
    // Reject names with path separators or traversal components
    if let Some(failure) = unsafe_name_failure(name) {
        return OperationResult::Failure(failure);
    }

    let dotfiles_dir = config.dotfiles_directory();

    // `write_file_no_follow` creates missing parent directories, so this
    // check precedes every target check and every write: tracking into a
    // directory that is not there would otherwise create it, turning a
    // mistyped `dotfiles_directory` into a new directory holding one spec.
    if let Err(error) = dotfiles_repo.find_package_files(name) {
        return OperationResult::Failure(OperationFailure::UnusableDotfilesDirectory(
            crate::package::collect::directory::TrackDirectoryRefusal::Unreadable(error),
        ));
    }

    let spec_path = dotfiles_dir.join(format!("{name}.yml"));

    // Deliberately not `set_source`, which leaves `top_level_keys` as `NoSource`, so
    // the two top-level guards have nothing to judge. The other two are quiet for
    // reasons of their own: this package declares no environments, and the entry
    // `handle_track` adds below is built rather than parsed, so it records no
    // unknown keys.
    let package = crate::package::PackageBuilder::default()
        .name(name)
        .path(spec_path.clone())
        .build();

    handle_track(
        TrackSpec {
            name: name.to_string(),
            spec_path,
            package,
            kind: SpecKind::New,
        },
        target_path,
        others(),
        dotfiles_repo,
        filesystem,
        sender,
        config,
    )
    .await
}

/// Handle `track_for_package`: load an existing package, copy the target file
/// alongside the YAML, add a dotfiles entry, save, and record deploy state.
pub(super) async fn handle_track_for_package<R, F>(
    package_name: &str,
    target_path: &str,
    others: impl FnOnce() -> Others,
    repo: &R,
    filesystem: &F,
    sender: &EventSender,
    config: &SelfieConfig,
) -> OperationResult
where
    R: PackageRepository,
    F: FileSystem,
{
    // Carried with its type rather than stringified, as every other
    // single-package path carries it, so an adapter keyed on the typed error
    // reaches this command too. Needs no frame of its own: `PackageError` names
    // the package and the directory it searched.
    let package_blob = match repo.get_package(package_name) {
        Ok(blob) => blob,
        Err(e) => {
            return OperationResult::Failure(crate::package::port::with_name_hint(repo, e).into());
        }
    };

    let spec_path = package_blob.file_path().to_path_buf();

    // The spec file's own stem, not the argument. Package lookup folds case, so
    // `track-dotfile BAT` resolves `packages/bat.yml`, and the copy belongs beside
    // that spec: composing the directory from the argument puts one package's
    // copies under two directories on a case-sensitive filesystem, and on a
    // case-insensitive one leaves the recorded `source:` spelling disagreeing with
    // the directory it names.
    //
    // Lowercased, because that is what `spec_name_of` returns: a spec named
    // `Bat.yml` puts its copies in `packages/bat/`. The directory and the recorded
    // `source:` both read this one name, so they agree either way.
    let Some(name) = crate::package::spec_name_of(&spec_path) else {
        return track_failed(
            Parts::new()
                .text("Cannot track into '")
                .path(spec_path.as_path())
                .text("': its file name does not name a package"),
        );
    };

    handle_track(
        TrackSpec {
            name,
            spec_path,
            package: package_blob.into_package(),
            kind: SpecKind::Existing,
        },
        target_path,
        others(),
        repo,
        filesystem,
        sender,
        config,
    )
    .await
}

/// Track `target_path` into `spec`: refuse what cannot be tracked, copy the file
/// into the repository beside the spec, add the entry, save the spec, and record
/// the deployment.
///
/// One body for both entry points, because every check and every write they
/// perform is the same one. [`SpecKind`] carries the single difference.
async fn handle_track<R, F>(
    mut spec: TrackSpec,
    target_path: &str,
    others: Others,
    repo: &R,
    filesystem: &F,
    sender: &EventSender,
    config: &SelfieConfig,
) -> OperationResult
where
    R: PackageRepository,
    F: FileSystem,
{
    // Expand the target, or refuse it if selfie could never deploy to it.
    //
    // First of the three refusals, and ahead of the guard for a reason of its own:
    // this one touches no filesystem at all, while the guard's stats and the read
    // would each resolve a relative path against the *process working directory*
    // -- which is what made track record entries every later apply refuses
    // (selfie-q9t3). It therefore also sits ahead of all three writes.
    //
    // Ahead of the already-tracked answer below as well: an entry recording a
    // target that can never deploy is not a reason to report it as tracked.
    let expanded_target = match deploy_target(filesystem, target_path) {
        Ok(path) => path,
        Err(rejection) => {
            return track_failed(track_target_refusal(target_path, rejection));
        }
    };

    // An entry for this target that apply deploys in the current environment,
    // whether shared or under `environments.<env>.dotfiles`: appending a shared
    // entry beside one would give that environment two entries for one target.
    // An entry scoped to another environment does not count, since nothing
    // deploys it here; the shared entry added instead is what that environment's
    // entry overrides. Each target goes through `expand_target_path`, not the
    // rule: this compares a recorded entry rather than writing to it.
    //
    // A `SpecKind::New` spec has no entries, so this answers `None` for one
    // without a branch of its own.
    let already_tracked = spec
        .package
        .dotfiles_for_environment(filesystem, config.environment())
        .into_iter()
        .find(|entry| expand_target_path(filesystem, entry.target()) == expanded_target);

    if let Some(entry) = already_tracked {
        // Nothing is written here, so a target selfie cannot write to is reported
        // rather than refused -- and it has to be reported here, because this is
        // the one track answer that reaches neither the refusals below nor a
        // deploy. With matching content drift answers `None` and has no line to
        // carry a reason either, so both commands were silent about it.
        if let Some(warning) = already_tracked_refusal(filesystem, &expanded_target) {
            sender.send_composed_warning(warning).await;
        }

        // The entry's own paths, not the argument and not the target: "already
        // tracking X" should name what the spec says, which is what a later apply
        // will use, and `source_path` means the file in the repository in every
        // other arm of this event.
        return OperationResult::Success(OperationSuccess::DotfileTracked {
            name: spec.name,
            source_path: tracked_copy_path(&spec.spec_path, &entry),
            target_path: entry.target().to_string(),
            was_already_tracked: true,
            environment: config.environment().to_string(),
            steps_completed: StepCount::new(1, 1),
        });
    }

    // Another spec with an entry for this file, in any environment, makes this
    // track a second one, and apply refuses both. Specs this check could not read
    // are named and the track goes on: refusing it for an unrelated broken spec
    // helps nobody, and apply still refuses a collision it would make.
    for unchecked in &others.unchecked {
        sender
            .send_warning(unchecked.sentence(Checking::Track))
            .await;
    }
    // The spec this track writes is not another spec: an existing one answered
    // above, and a new one's path is refused below if a file is already there.
    let others_but_this: Vec<Package> = others
        .packages
        .into_iter()
        .filter(|package| *package.path() != spec.spec_path)
        .collect();
    let trackers = trackers_of(&others_but_this, filesystem, &expanded_target);
    if !trackers.is_empty() {
        return track_failed(tracked_elsewhere_refusal(target_path, &trackers));
    }

    // Ahead of the read and every write, so a link or a fifo gets its own sentence
    // and remedy: accepting a link would copy its destination into the dotfiles
    // directory, where `sync push` commits it, and the read would refuse one only as
    // unreadable. After the already-tracked answer above, because refusing an
    // idempotent no-op helps nobody.
    //
    // A fifo is not given `track_refusal`: its "replace the symlink with a regular
    // file, or track the path it points to" fits neither half, since a fifo points at
    // nothing. Naming a different target is the remedy that applies.
    match guard_refusal(filesystem, &expanded_target) {
        None => {}
        Some(link @ FileSystemError::SymlinkedTarget { .. }) => {
            return track_failed(track_refusal(&link));
        }
        Some(refusal) => {
            return track_failed(
                refusal
                    .parts()
                    .text(". Point the entry at a regular file instead."),
            );
        }
    }

    // The one read of the target, through the classifier apply and drift use, so a
    // directory and a vanished file are named for what they are.
    //
    // Text, as apply reads the repository copy back: a file that is not UTF-8 would
    // track and then never deploy.
    let content = match read_target_state(filesystem, &expanded_target) {
        TargetState::Readable(bytes) => match String::from_utf8(bytes) {
            Ok(content) => content,
            Err(e) => {
                return track_failed(Parts::new().text(format!(
                    "Cannot read target file: it is not UTF-8 text ({e})"
                )));
            }
        },
        TargetState::Absent | TargetState::BelowNonDirectory { .. } => {
            return track_failed(
                Parts::new()
                    .text("Target file does not exist: ")
                    .path(expanded_target.path()),
            );
        }
        TargetState::Directory => {
            return track_failed(
                Parts::new()
                    .text("Cannot track the target: ")
                    .append(directory_at_target(&expanded_target)),
            );
        }
        // A link or fifo put there after the guard, refused in the guard's words.
        TargetState::Link(link) => {
            return track_failed(track_refusal(&link.refusal()));
        }
        TargetState::Irregular(refusal) => {
            return track_failed(
                refusal
                    .parts()
                    .text(". Point the entry at a regular file instead."),
            );
        }
        TargetState::Unreadable(e) => {
            return track_failed(
                Parts::new()
                    .text("Cannot read target file: ")
                    .append(FileSystemError::IoError(e.into()).parts()),
            );
        }
        TargetState::Undetermined(e) => {
            return track_failed(
                Parts::new()
                    .text("Cannot read target file: ")
                    .append(e.parts()),
            );
        }
    };

    // Ahead of every write below. Track ends by recording the deployment, and a
    // state file it could not load is one it must not write over, so the copy
    // and the spec are not created for a record that cannot be kept.
    let mut recorder =
        match load_deploy_state(filesystem, config.state_directory().map(PathBuf::as_path)) {
            StateLoad::Usable(loaded) => {
                if let Some(warning) = loaded.directory_warning() {
                    sender.send_composed_warning(warning.clone()).await;
                }
                Recorder::new(loaded)
            }
            StateLoad::Unusable(failure) => {
                return OperationResult::Failure(OperationFailure::DeployState(
                    DeployStateFailure::Unusable(failure),
                ));
            }
        };

    let filename = expanded_target
        .path()
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    // Only for a spec this track would create. The other kind was loaded from
    // this path, so something being there is what was expected.
    if matches!(spec.kind, SpecKind::New) && repo.path_is_occupied(&spec.spec_path) {
        // Says what was checked: this one path. A spec under the same name with
        // another extension is the name checks' to find, not this guard's.
        return track_failed(
            Parts::new()
                .text("Something is already at ")
                .path(spec.spec_path.as_path())
                .text(", where the spec for '")
                .name(spec.name.as_str())
                .text("' would be written. Remove it first or choose a different name."),
        );
    }

    // Asked here, beside the directory it protects, so both entry points are covered
    // by one check rather than by one each.
    if let Some(failure) = unusable_copy_directory(&spec.spec_path, &spec.name) {
        return OperationResult::Failure(failure);
    }

    // The copy goes in a directory named for the spec, beside the spec itself:
    // `dotfiles/bat/config` for `dotfiles/bat.yml`, `packages/bat/config` for
    // `packages/bat.yml`. One formula, because the two entry points compose the
    // same shape from different roots.
    //
    // Refused above, so this cannot fire. Written as a refusal rather than a default
    // because composing the copy against the process's working directory is the one
    // outcome worth never reaching by accident.
    let Some(base_dir) = spec.spec_path.parent() else {
        return track_failed(
            Parts::new()
                .text("Cannot track into '")
                .path(spec.spec_path.as_path())
                .text("': its spec has no directory to write beside"),
        );
    };
    let source_dir = base_dir.join(&spec.name);
    let source_path = source_dir.join(&filename);
    let relative_source = format!("{}/{filename}", spec.name);

    let already_exists = || {
        track_failed(
            Parts::new()
                .text("Source file already exists at ")
                .path(source_path.as_path())
                .text(". Remove it first, or track a different file."),
        )
    };
    if filesystem.path_exists(&source_path) {
        return already_exists();
    }

    // Written back after every refusal above and just before the first write, so
    // a refused track writes nothing, and one whose state cannot be written stops
    // before the copy and the spec exist.
    let mut loaded = match recorder.writable(filesystem) {
        Ok(loaded) => loaded,
        Err(failure) => {
            return OperationResult::Failure(OperationFailure::DeployState(
                DeployStateFailure::NothingTracked(failure),
            ));
        }
    };

    // Replaces nothing: a file that appeared since the check above is the user's,
    // and is refused as one the check found.
    match filesystem.write_file_no_follow(
        &repository_path(&source_path),
        content.as_bytes(),
        IfPresent::Keep,
    ) {
        Ok(Placed::Written) => {}
        Ok(Placed::Kept) => return already_exists(),
        Err(e) => {
            return track_failed(repository_write_refusal(&source_path, &e));
        }
    }

    let recorded_target = portable_target(filesystem, &expanded_target);
    // Added unconditionally: the already-tracked answer above has ruled out an
    // entry for this target in the current environment.
    spec.package
        .add_dotfile(DotfileEntry::new(&relative_source, &recorded_target));

    // A new spec is created, not written over, within `IfPresent::Keep`'s
    // guarantee: one that appeared since the check above is left in place, and the
    // save fails.
    let saved = match spec.kind {
        SpecKind::New => repo.create_package(&spec.package, &spec.spec_path),
        SpecKind::Existing => repo.save_package(&spec.package, &spec.spec_path),
    };
    if let Err(e) = saved {
        // Only the file, and only selfie's own. `remove_own_copy` establishes the
        // second by content; the directory is the part selfie leaves, because it
        // cannot tell one it created from one that was already there and an empty
        // directory refuses nothing on a retry.
        let removal = remove_own_copy(filesystem, &source_path, &content);
        return track_failed(spec_save_failure(&e, &source_path, &removal));
    }

    let checksum = compute_checksum(content.as_bytes());
    // The folded name from the spec's path, as apply records it. `spec.name` is
    // the name as typed, which may differ in case.
    loaded.state_mut().record_deployment(
        &expanded_target.state_key(),
        &relative_source,
        &checksum,
        crate::package::spec_name_of(&spec.spec_path).as_deref(),
        // The copy sits beside the spec, so its path is relative to the spec's
        // directory. One directory configured as both is the package directory,
        // as collection reads it.
        if base_dir == config.package_directory().as_path() {
            Some(crate::package::event::BaseKind::PackageDirectory)
        } else if base_dir == config.dotfiles_directory().as_path() {
            Some(crate::package::event::BaseKind::DotfilesDirectory)
        } else {
            None
        },
    );
    // Last, and nothing is rolled back for it: the copy and the entry are both
    // correct and only the record is missing, so the failure names what exists and
    // what recovers it rather than undoing two good writes. It sends the user to
    // `selfie apply`, whose in-sync skip arm records the entry without asking. It
    // says nothing about re-running track: with the entry saved, a second
    // `track_for_package` answers "already tracking" having recorded nothing, and
    // a second standalone track is refused by the spec-collision guard, so naming
    // either is wrong at the other entry point.
    if let Err(error) = save_deploy_state(filesystem, &loaded) {
        return OperationResult::Failure(OperationFailure::DeployState(
            DeployStateFailure::TrackUnrecorded {
                error,
                name: spec.name.clone(),
                target: recorded_target.clone(),
                spec_path: spec.spec_path.clone(),
                copy: source_path.clone(),
            },
        ));
    }

    // The recorded form, not the argument: an adapter that echoed the caller's
    // path would name a target the spec does not contain.
    OperationResult::Success(OperationSuccess::DotfileTracked {
        name: spec.name,
        source_path,
        target_path: recorded_target,
        was_already_tracked: false,
        environment: config.environment().to_string(),
        steps_completed: StepCount::new(1, 1),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::MockFileSystem;
    use crate::package::port::{UntrustedLevel, UntrustedLevels};

    // selfie-yw7i. Track copies the user's file *into* the dotfiles repository, so
    // a refusal is about a repository path -- but every `FileSystemError` variant
    // here says "target" in its own `Display`, having been written for a dotfile
    // target, the path selfie deploys *out* to. Rendering one verbatim tells
    // someone who ran `selfie dotfiles track ~/.gemrc` that their "target" is a
    // symlink, when the symlink is the copy destination they never named.
    //
    // Asserted as an absence for the same reason as the `save_package` sibling:
    // the regression to guard is a reversion to `Cannot write source file: {e}`,
    // which puts the word straight back while still naming a path.
    #[test]
    fn a_refused_repository_write_does_not_call_it_a_target() {
        let source = Path::new("/dotfiles/gemrc/.gemrc");

        for refusal in [
            FileSystemError::SymlinkedTarget {
                path: source.to_path_buf(),
                points_to: Some(PathBuf::from("/tmp/planted")),
            },
            FileSystemError::IrregularTarget {
                path: source.to_path_buf(),
                kind: "named pipe (fifo)",
            },
        ] {
            let message = repository_write_refusal(source, &refusal).to_string();
            assert!(
                !message.contains("target"),
                "refusal calls a repository path a target: {message}"
            );
            assert!(
                message.contains("/dotfiles/gemrc/.gemrc"),
                "refusal does not name the repository path: {message}"
            );
            // The remedy `track_refusal` gives is about a target and is wrong
            // here: there is no target to point at.
            assert!(
                !message.contains("track the path it points to"),
                "refusal offers the target-side remedy: {message}"
            );
        }
    }

    // The loader refuses `.` and `..` as spec names before a track reaches this
    // guard, so only a direct call exercises it. It asks about position, and
    // must refuse both names whatever the name rule admits.
    #[test]
    fn the_copy_directory_guard_refuses_names_that_leave_or_are_the_spec_directory() {
        let spec = Path::new("/repo/packages/x.yml");

        let climbs = unusable_copy_directory(spec, "..").expect("'..' must be refused");
        assert!(climbs.to_string().contains("outside"), "got: {climbs}");

        let same = unusable_copy_directory(spec, ".").expect("'.' must be refused");
        assert!(
            same.to_string()
                .contains("rather than a directory of its own"),
            "got: {same}"
        );

        assert!(unusable_copy_directory(spec, "python3.11").is_none());
    }

    // The copy is removed only when the path still holds what this call wrote.
    // The guard before the copy is advisory, so ownership has to be established
    // rather than inherited from it: without the comparison, a file another
    // process put at the path between the write and this removal is deleted.
    //
    // The file system keeping what it found is the assertion: the comparison and
    // the unlink are one port call, tested where they are implemented.
    #[test]
    fn a_copy_whose_content_changed_is_not_removed() {
        let copy = Path::new("/dotfiles/gemrc/gemrc");
        let mut fs = MockFileSystem::default();
        fs.expect_remove_file_if_holding()
            .times(1)
            .returning(|_, _| Ok(Removal::Kept));
        fs.expect_remove_file().never();

        let removal = remove_own_copy(&fs, copy, "gem: --no-document");

        assert!(
            matches!(removal, CopyRemoval::Unconfirmed),
            "a foreign file must not be claimed"
        );
        let message = copy_fate(copy, &removal).to_string();
        assert!(
            message.contains("left alone"),
            "the user is not told the copy survived: {message}"
        );
    }

    // The control, and the reason the test above is not vacuous: with the bytes
    // selfie wrote still at the path, the removal happens. Without this, refusing
    // to remove anything at all would pass that test.
    #[test]
    fn a_copy_that_still_holds_what_was_written_is_removed() {
        let copy = Path::new("/dotfiles/gemrc/gemrc");
        let mut fs = MockFileSystem::default();
        fs.expect_remove_file_if_holding()
            .times(1)
            .withf(|path, expected| {
                path.path() == Path::new("/dotfiles/gemrc/gemrc")
                    && expected == b"gem: --no-document"
            })
            .returning(|_, _| Ok(Removal::Removed));

        let removal = remove_own_copy(&fs, copy, "gem: --no-document");

        assert!(
            matches!(removal, CopyRemoval::Removed),
            "selfie's own copy must be removed"
        );
        assert!(
            copy_fate(copy, &removal)
                .to_string()
                .contains("was removed"),
            "the copy's fate is not stated"
        );
    }

    // A path selfie cannot read back is a path it cannot claim, so it is left
    // rather than removed on the assumption that the read failure is benign, and
    // the user is told why.
    #[test]
    fn a_copy_that_cannot_be_read_back_is_not_removed() {
        let copy = Path::new("/dotfiles/gemrc/gemrc");
        let mut fs = MockFileSystem::default();
        fs.expect_remove_file_if_holding().returning(|_, _| {
            Err(FileSystemError::IoError(
                std::io::Error::other("/dotfiles/gemrc/gemrc: permission denied").into(),
            ))
        });
        fs.expect_remove_file().never();

        let removal = remove_own_copy(&fs, copy, "gem: --no-document");

        assert!(matches!(removal, CopyRemoval::Failed(_)));
        // selfie cannot claim what it could not read, so the user is asked to check
        // the file rather than told to delete it.
        let message = copy_fate(copy, &removal).to_string();
        assert!(
            message.contains("permission denied"),
            "the user is not told why the copy survived: {message}"
        );
        assert!(
            message.contains("Check it, and remove it yourself before retrying if it is the copy"),
            "the remedy does not ask the user to check the file: {message}"
        );
    }

    // selfie-ir68.20. Which stat answers first is this module's rule, and the one
    // function is where it is now decided, so this is where it is pinned. A target
    // that is both a symlink and, through the link, an irregular file must be
    // reported as the symlink, because the followed stat would send the user to
    // inspect the file behind the link instead of the link they planted.
    #[test]
    fn an_already_tracked_refusal_reports_the_symlink_before_what_it_points_at() {
        let target = repository_path(Path::new("/home/u/.config/app/config"));

        let mut fs = MockFileSystem::default();
        fs.expect_symlink_refusal().returning(|path| {
            Some(FileSystemError::SymlinkedTarget {
                path: path.path().to_path_buf(),
                points_to: Some(PathBuf::from("/tmp/pipe")),
            })
        });
        // Answers too, and must not be the answer given.
        fs.expect_irregular_target_refusal().returning(|path| {
            Some(FileSystemError::IrregularTarget {
                path: path.path().to_path_buf(),
                kind: "named pipe (fifo)",
            })
        });

        let warning = already_tracked_refusal(&fs, &target)
            .expect("both stats answered")
            .to_string();
        assert!(
            warning.contains("symlink"),
            "the symlink must answer first: {warning}"
        );
        assert!(
            !warning.contains("named pipe"),
            "the followed stat answered instead of the link: {warning}"
        );
    }

    // The control: an ordinary file is not refused, so a caller can tell the two
    // apart. Without it, a function returning `Some` for everything would pass the
    // test above.
    #[test]
    fn an_already_tracked_regular_file_is_not_refused() {
        let target = repository_path(Path::new("/home/u/.config/app/config"));

        let mut fs = MockFileSystem::default();
        fs.expect_symlink_refusal().returning(|_| None);
        fs.expect_irregular_target_refusal().returning(|_| None);

        assert!(already_tracked_refusal(&fs, &target).is_none());
    }

    // Exactly once, counted rather than checked for presence. The message leaves
    // naming the spec to the error, so a variant that stops naming it drops the
    // count to zero and fails here rather than shipping a failure that names no
    // file; re-adding a path to the frame takes it to two, which is the
    // duplication this wording exists to avoid.
    #[test]
    fn a_failed_spec_save_names_the_spec_exactly_once() {
        let spec = "/dotfiles/gemrc.yml";
        let error = PackageRepoError::FileSystemError(FileSystemError::IoError(
            std::io::Error::other(format!("{spec}: Permission denied (os error 13)")).into(),
        ));

        let message = spec_save_failure(
            &error,
            Path::new("/dotfiles/gemrc/gemrc"),
            &CopyRemoval::Removed,
        )
        .to_string();

        assert_eq!(
            message.matches(spec).count(),
            1,
            "the spec must be named exactly once: {message}"
        );
        assert!(
            message.contains("/dotfiles/gemrc/gemrc"),
            "the copy is not named: {message}"
        );
        assert!(
            message.contains("was removed"),
            "the copy's fate is not stated: {message}"
        );
    }

    // The refusal of a spec that changed after the load carries its own path, so
    // the frame must not add a second mention.
    #[test]
    fn a_spec_save_refused_as_changed_names_the_spec_exactly_once() {
        let spec = "/dotfiles/gemrc.yml";
        let error = PackageRepoError::ChangedSinceLoad {
            path: PathBuf::from(spec),
            refused: crate::package::port::RefusedChange::Write,
        };

        let message = spec_save_failure(
            &error,
            Path::new("/dotfiles/gemrc/gemrc"),
            &CopyRemoval::Removed,
        )
        .to_string();

        assert_eq!(
            message.matches(spec).count(),
            1,
            "the spec must be named exactly once: {message}"
        );
        assert!(
            !message.contains(".."),
            "the frame doubled a period: {message}"
        );
    }

    // A rewrite refusal names the spec in its refusal and in its remedy, however
    // many levels it lists, and the frame adds no mention of its own.
    #[test]
    fn a_spec_save_refused_at_several_levels_adds_no_mention_of_the_spec() {
        let spec = "/packages/creds.yml";
        let error = PackageRepoError::UntrustedKeys {
            path: PathBuf::from(spec),
            levels: UntrustedLevels::new(vec![
                UntrustedLevel::DotfileFields {
                    fields: vec!["dotfiles[0].var".to_string()],
                    entries: 1,
                },
                UntrustedLevel::EnvironmentFields(vec!["environments.work.audt".to_string()]),
            ])
            .unwrap(),
        };

        let message = spec_save_failure(
            &error,
            Path::new("/packages/creds/token"),
            &CopyRemoval::Removed,
        )
        .to_string();

        assert_eq!(
            message.matches(spec).count(),
            error.to_string().matches(spec).count(),
            "the frame named the spec again: {message}"
        );
        assert!(
            message.contains("environments.work.audt"),
            "a level is missing: {message}"
        );
    }

    // A copy that survived says so, and says it differently. The two arms are
    // opposite advice -- one needs nothing from the user, the other needs a file
    // deleted before a retry can work -- so a reader must be able to tell them
    // apart.
    #[test]
    fn a_failed_spec_save_says_when_the_copy_survived() {
        let error = PackageRepoError::UntrustedKeys {
            path: PathBuf::from("/packages/creds.yml"),
            levels: UntrustedLevels::new(vec![UntrustedLevel::DotfileFields {
                fields: vec!["dotfiles[0].var".to_string()],
                entries: 1,
            }])
            .unwrap(),
        };
        let removal = FileSystemError::IoError(
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "Permission denied").into(),
        );

        let survived = spec_save_failure(
            &error,
            Path::new("/packages/creds/token"),
            &CopyRemoval::Failed(removal),
        )
        .to_string();
        let removed = spec_save_failure(
            &error,
            Path::new("/packages/creds/token"),
            &CopyRemoval::Removed,
        )
        .to_string();

        assert_ne!(
            survived, removed,
            "a copy that survived reads the same as one that was removed"
        );
        assert!(
            !survived.contains("was removed"),
            "a surviving copy is called removed: {survived}"
        );
        assert!(
            survived.contains("Check it, and remove it yourself before retrying"),
            "the remedy is missing: {survived}"
        );
        // The error's own sentence ends in a period, and the frame adds none.
        assert!(!removed.contains(".."), "a doubled period: {removed}");
    }

    // Only the one period the frame would double is dropped: an error ending in an
    // ellipsis keeps it whole.
    #[test]
    fn a_failed_spec_save_keeps_an_ellipsis_the_error_ends_with() {
        let error = PackageRepoError::FileSystemError(FileSystemError::IoError(
            std::io::Error::other("the disk filled up...").into(),
        ));

        let message = spec_save_failure(
            &error,
            Path::new("/dotfiles/gemrc/gemrc"),
            &CopyRemoval::Removed,
        )
        .to_string();

        assert!(
            message.contains("the disk filled up... The copy"),
            "the ellipsis was cut: {message}"
        );
    }

    // The control: a failure that is not a refusal keeps the filesystem's own
    // message, so the rephrasing is narrow rather than swallowing every write
    // error. Its `Display` is allowed to say whatever it says.
    #[test]
    fn a_repository_write_failure_that_is_not_a_refusal_is_passed_through() {
        let message = repository_write_refusal(
            Path::new("/dotfiles/gemrc/.gemrc"),
            &FileSystemError::IoError(
                std::io::Error::new(std::io::ErrorKind::PermissionDenied, "Permission denied")
                    .into(),
            ),
        )
        .to_string();
        assert!(message.contains("Permission denied"), "got: {message}");
    }

    // Each sentence a track builds outside its handlers, whole.
    #[test]
    fn the_track_builders_are_worded_whole() {
        let spec = Path::new("/repo/my packages/x.yml");
        let copy = Path::new("/d/g c/cfg");
        let fs_error =
            || FileSystemError::IoError(std::io::Error::other("/d/g.yml: denied").into());
        let save = |removal: &CopyRemoval| {
            spec_save_failure(
                &PackageRepoError::FileSystemError(fs_error()),
                copy,
                removal,
            )
            .to_string()
        };

        assert_eq!(
            unusable_copy_directory(spec, "..").map(|f| f.to_string()),
            Some(
                "Cannot track into '..': the copy would be written outside '/repo/my packages'"
                    .to_string()
            )
        );
        assert_eq!(
            unusable_copy_directory(spec, ".").map(|f| f.to_string()),
            Some(
                "Cannot track into '.': the copy would be written straight into '/repo/my \
                 packages' rather than a directory of its own"
                    .to_string()
            )
        );
        assert_eq!(
            unusable_copy_directory(Path::new("x.yml"), "n").map(|f| f.to_string()),
            Some("Cannot track into 'n': its spec has no directory to write beside".to_string())
        );
        assert_eq!(
            repository_write_refusal(
                copy,
                &FileSystemError::SymlinkedTarget {
                    path: copy.to_path_buf(),
                    points_to: Some(PathBuf::from("/tmp/my planted")),
                }
            )
            .to_string(),
            "Cannot write the tracked copy at '/d/g c/cfg': it is a symlink to '/tmp/my \
             planted'. Remove it, or track under a different name."
        );
        assert_eq!(
            repository_write_refusal(
                copy,
                &FileSystemError::IrregularTarget {
                    path: copy.to_path_buf(),
                    kind: "socket",
                }
            )
            .to_string(),
            "Cannot write the tracked copy at '/d/g c/cfg': it is a socket. Remove it, or track \
             under a different name."
        );
        assert_eq!(
            repository_write_refusal(copy, &fs_error()).to_string(),
            "Cannot write source file: IO error: /d/g.yml: denied"
        );
        assert_eq!(
            save(&CopyRemoval::Removed),
            "Cannot save the spec: File system error: IO error: /d/g.yml: denied. The copy at '/d/g c/cfg' was \
             removed."
        );
        assert_eq!(
            save(&CopyRemoval::Unconfirmed),
            "Cannot save the spec: File system error: IO error: /d/g.yml: denied. The copy at '/d/g c/cfg' was left \
             alone: it no longer holds what selfie wrote, so removing it could have deleted \
             another file. Check it, and remove it yourself if it is not wanted."
        );
        assert_eq!(
            save(&CopyRemoval::Failed(fs_error())),
            "Cannot save the spec: File system error: IO error: /d/g.yml: denied. The copy at '/d/g c/cfg' could \
             not be checked or removed: IO error: /d/g.yml: denied. Check it, and remove it \
             yourself before retrying if it is the copy selfie wrote."
        );
        assert_eq!(
            unsafe_name_failure("../x").map(|f| f.to_string()),
            Some(format!(
                "Invalid name '../x': {}",
                crate::package::SPEC_NAME_RULE
            ))
        );
    }

    // Every path and name a track's failure names is a part of its own kind,
    // its own or one inside a nested cause, whichever builder words it.
    #[test]
    fn every_value_in_a_track_failure_is_a_part() {
        use crate::parts::values_of;

        let spec = Path::new("/r\u{200B}epo/x.yml");
        let copy = Path::new("/d\u{200B}/cfg");
        let at = Path::new("/a\u{200B}t");
        let fs_error = || {
            FileSystemError::IoError(crate::fs::IoFailure::at(
                at,
                std::io::Error::other("denied"),
            ))
        };
        let tracker = |spec_name: &str, origin| Tracker {
            spec_name: spec_name.to_string(),
            spec_path: PathBuf::from(format!("/p\u{200B}/{spec_name}.yml")),
            target: "~/t\u{200B}".to_string(),
            origin,
        };
        let failure_message = |failure: Option<OperationFailure>| match failure {
            Some(OperationFailure::Track(message)) => message,
            other => panic!("not a track failure: {other:?}"),
        };
        let values = |message: &Message| values_of(message);

        assert_eq!(
            values(&tracked_elsewhere_refusal(
                "~/t\u{200B}",
                &[
                    tracker("b\u{200B}at", SpecOrigin::PackageDirectory),
                    tracker("g\u{200B}em", SpecOrigin::DotfilesDirectory),
                ],
            )),
            [
                "~/t\u{200B}",
                "b\u{200B}at",
                "/p\u{200B}/b\u{200B}at.yml",
                "g\u{200B}em",
                "/p\u{200B}/g\u{200B}em.yml"
            ]
        );
        assert_eq!(
            values(&failure_message(unsafe_name_failure("../\u{200B}"))),
            ["../\u{200B}"]
        );
        assert_eq!(
            values(&failure_message(unusable_copy_directory(spec, ".."))),
            ["..", "/r\u{200B}epo"]
        );
        assert_eq!(values(&track_refusal(&fs_error())), ["/a\u{200B}t"]);
        assert_eq!(
            values(&repository_write_refusal(
                copy,
                &FileSystemError::SymlinkedTarget {
                    path: copy.to_path_buf(),
                    points_to: Some(at.to_path_buf()),
                }
            )),
            ["/d\u{200B}/cfg", "/a\u{200B}t"]
        );
        assert_eq!(
            values(&repository_write_refusal(copy, &fs_error())),
            ["/a\u{200B}t"]
        );
        assert_eq!(
            values(&spec_save_failure(
                &PackageRepoError::FileSystemError(fs_error()),
                copy,
                &CopyRemoval::Failed(fs_error()),
            )),
            ["/a\u{200B}t", "/d\u{200B}/cfg", "/a\u{200B}t"]
        );
        assert_eq!(
            values(&track_target_refusal(
                "t\u{200B}",
                TargetRejection::Relative
            )),
            ["t\u{200B}"]
        );
    }

    // The remedy for a target one package deploys is a command part, so the CLI
    // shows it runnable with its note rather than as text.
    #[test]
    fn a_tracked_elsewhere_remedy_is_a_command_part() {
        let message = tracked_elsewhere_refusal(
            "~/t\u{200B}",
            &[Tracker {
                spec_name: "bat".to_string(),
                spec_path: PathBuf::from("/p/bat.yml"),
                target: "~/t\u{200B}".to_string(),
                origin: SpecOrigin::PackageDirectory,
            }],
        );

        let commands: Vec<String> = message
            .parts()
            .into_iter()
            .filter_map(|part| match part {
                crate::parts::Part::Command(command) => Some(command.to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(commands, ["selfie package track-dotfile bat '~/t\u{200B}'"]);
    }

    // A cause ending in a path gets exactly one stop from the frame: only a text
    // loses a final stop, so a path keeps every character it has, a final dot
    // included.
    #[test]
    fn a_cause_ending_in_a_path_gets_one_stop() {
        use crate::fs::{AbsentReason, DirectoryState};
        let save = |points_to: &str| {
            spec_save_failure(
                &PackageRepoError::PackageListError(crate::package::port::PackageListError::new(
                    PathBuf::from("/d/dots"),
                    DirectoryState::Absent(AbsentReason::DanglingSymlink {
                        points_to: Some(PathBuf::from(points_to)),
                    }),
                )),
                Path::new("/d/dots/cfg"),
                &CopyRemoval::Removed,
            )
            .to_string()
        };

        assert!(
            save("/d/gone").contains("it points at /d/gone. The copy at"),
            "{}",
            save("/d/gone")
        );
        assert!(
            save("/d/gone.").contains("it points at /d/gone.. The copy at"),
            "{}",
            save("/d/gone.")
        );
    }

    // An already-tracked target that is a symlink or a fifo is reported in these
    // words, whole.
    #[test]
    fn an_already_tracked_refusal_is_worded_whole() {
        let target = repository_path(Path::new("/home/u/my rc"));
        let warning = |link: bool| {
            let mut fs = MockFileSystem::default();
            fs.expect_symlink_refusal().returning(move |path| {
                link.then(|| FileSystemError::SymlinkedTarget {
                    path: path.path().to_path_buf(),
                    points_to: Some(PathBuf::from("/home/u/my vault")),
                })
            });
            fs.expect_irregular_target_refusal().returning(|path| {
                Some(FileSystemError::IrregularTarget {
                    path: path.path().to_path_buf(),
                    kind: "named pipe (fifo)",
                })
            });
            already_tracked_refusal(&fs, &target).map(|warning| warning.to_string())
        };

        assert_eq!(
            warning(true),
            Some(
                "/home/u/my rc: target is a symlink to '/home/u/my vault' and selfie will not \
                 write through it. The entry stays as it is, and this command wrote nothing."
                    .to_string()
            )
        );
        assert_eq!(
            warning(false),
            Some(
                "/home/u/my rc: target resolves to a named pipe (fifo) and selfie will not write \
                 to it. The entry stays as it is, and this command wrote nothing."
                    .to_string()
            )
        );
    }

    // An already-tracked target and a link's destination are path parts.
    #[test]
    fn an_already_tracked_refusal_names_its_paths_as_parts() {
        let target = repository_path(Path::new("/h/r\u{200B}c"));
        let mut fs = MockFileSystem::default();
        fs.expect_symlink_refusal().returning(|path| {
            Some(FileSystemError::SymlinkedTarget {
                path: path.path().to_path_buf(),
                points_to: Some(PathBuf::from("/h/v\u{200B}ault")),
            })
        });
        fs.expect_irregular_target_refusal().returning(|_| None);

        assert_eq!(
            crate::parts::values_of(&already_tracked_refusal(&fs, &target).expect("refused")),
            ["/h/r\u{200B}c", "/h/v\u{200B}ault"]
        );
    }
}
