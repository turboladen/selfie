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
    paths::{is_within, normalize_path},
};

use super::refusal::{TargetState, directory_at_target, guard_refusal, read_target_state};
use super::state_file::{
    Recorder, StateLoad, StateSaveError, load_deploy_state, save_deploy_state,
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
    /// The spec, as a sentence names it: `package 'ruby'` or `standalone dotfile
    /// 'gemrc'`.
    #[must_use]
    pub fn label(&self) -> String {
        match self.origin {
            SpecOrigin::DotfilesDirectory => format!("standalone dotfile '{}'", self.spec_name),
            SpecOrigin::PackageDirectory | SpecOrigin::Memory => {
                format!("package '{}'", self.spec_name)
            }
        }
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
fn tracked_elsewhere_refusal(target: &str, trackers: &[Tracker]) -> String {
    let named: Vec<String> = trackers
        .iter()
        .map(|tracker| format!("{} ({})", tracker.label(), tracker.spec_path.display()))
        .collect();
    let remedy = match trackers {
        [
            Tracker {
                spec_name,
                origin: SpecOrigin::PackageDirectory,
                ..
            },
        ] if let (Some(spec_name), Some(target)) = (
            crate::fs::shell_word(spec_name),
            crate::fs::shell_word(target),
        ) =>
        {
            format!(
                "Track it there with `selfie package track-dotfile {spec_name} {target}`, or \
                 remove that entry first."
            )
        }
        _ => "Remove the entry from the spec that should not deploy it first.".to_string(),
    };
    format!(
        "Cannot track '{target}': {} already deploys it, and two specs deploying one file are \
         both refused by apply. {remedy}",
        named.join(" and ")
    )
}

/// Why a name cannot be a directory under the repository, or `None` if it can.
fn unsafe_name_failure(name: &str) -> Option<OperationFailure> {
    if crate::package::is_valid_spec_name(name) {
        return None;
    }
    Some(OperationFailure::Generic(format!(
        "Invalid name '{name}': {}",
        crate::package::SPEC_NAME_RULE
    )))
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
    let refuse = |why: &str| {
        Some(OperationFailure::Generic(format!(
            "Cannot track into '{name}': {why}"
        )))
    };

    // A spec with no parent cannot have a directory beside it. `Path::parent` is
    // `None` only for a root or an empty path, and an empty one also yields `Some`
    // for a bare file name, so both are refused here rather than defaulted to `.`:
    // defaulting would compose the copy against the process's working directory.
    let Some(base_dir) = spec_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return refuse("its spec has no directory to write beside");
    };

    let composed = base_dir.join(name);
    if !is_within(&composed, base_dir) {
        return refuse(&format!(
            "the copy would be written outside '{}'",
            base_dir.display()
        ));
    }
    if normalize_path(&composed) == normalize_path(base_dir) {
        return refuse(&format!(
            "the copy would be written straight into '{}' rather than a directory of its own",
            base_dir.display()
        ));
    }
    None
}

// Both track handlers word a refused track. Same `FileSystemError` apply renders,
// plus the remedy that only applies while the entry does not exist yet.
fn track_refusal(refusal: &FileSystemError) -> String {
    format!("{refusal}. Replace the symlink with a regular file, or track the path it points to.")
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
fn repository_write_refusal(source_path: &Path, refusal: &FileSystemError) -> String {
    let what = match refusal {
        FileSystemError::SymlinkedTarget { points_to, .. } => match points_to {
            Some(dest) => format!("it is a symlink to '{}'", dest.display()),
            None => "it is a symlink".to_string(),
        },
        FileSystemError::IrregularTarget { kind, .. } => format!("it is a {kind}"),
        // Not a refusal: a permission problem, a full disk. Rendered as-is,
        // because the filesystem's own message is the useful one and it makes no
        // claim about a target.
        other => return format!("Cannot write source file: {other}"),
    };

    // "the tracked copy at" rather than naming a directory: `handle_track_
    // standalone` composes this under `dotfiles_directory` and
    // `handle_track_for_package` alongside the package YAML, so any sentence
    // naming one of the two is wrong at the other call site.
    format!(
        "Cannot write the tracked copy at '{}': {what}. \
         Remove it, or track under a different name.",
        source_path.display()
    )
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
) -> Option<String> {
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
fn already_tracked_refusal_warning(refusal: &FileSystemError) -> String {
    format!("{refusal}. The entry stays as it is, and this command wrote nothing.")
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
fn copy_fate(copy: &Path, removal: &CopyRemoval) -> String {
    let copy = copy.display();
    match removal {
        CopyRemoval::Removed => format!("The copy at '{copy}' was removed."),
        CopyRemoval::Unconfirmed => format!(
            "The copy at '{copy}' was left alone: it no longer holds what selfie wrote, so \
             removing it could have deleted another file. Check it, and remove it yourself if \
             it is not wanted."
        ),
        CopyRemoval::Failed(e) => format!(
            "The copy at '{copy}' could not be checked or removed: {e}. Check it, and remove it \
             yourself before retrying if it is the copy selfie wrote."
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
fn spec_save_failure(error: &PackageRepoError, copy: &Path, removal: &CopyRemoval) -> String {
    // Several errors end their own sentence, so the frame does not add a second
    // period. Only one is stripped: an error ending in an ellipsis keeps it.
    let error = error.to_string();
    format!(
        "Cannot save the spec: {}. {}",
        error.strip_suffix('.').unwrap_or(&error),
        copy_fate(copy, removal)
    )
}

// Why a track's copy and spec are in place with no deployment recorded, and how
// to finish it.
//
// Nothing is rolled back for this: both writes are correct and only the record is
// missing, so undoing them would throw away work to tidy a record.
//
// Sends the user to `selfie apply`, which finishes the job: the entry is
// untracked and its target already matches the copy, so apply's in-sync skip arm
// records it without asking.

// Says nothing about re-running track, which does different things at the two
// entry points and neither of them useful: with the entry saved, a second
// `track_for_package` answers "already tracking" and exits 0 having recorded
// nothing, while a second standalone track is refused by the spec-collision
// guard. Naming either would be wrong at the other call site.
fn unrecorded_track_failure(
    error: &StateSaveError,
    name: &str,
    recorded_target: &str,
    spec_path: &Path,
    copy: &Path,
) -> String {
    format!(
        "Tracked '{recorded_target}': the copy at '{}' and the entry in '{}' are written. \
         The deployment was not recorded: {error}. Run `selfie apply {name}` once the state \
         file can be written, and it records the deployment.",
        copy.display(),
        spec_path.display()
    )
}

// The same rule refused at track time, where it is a failure rather than a
// skipped entry and the remedy is worth stating -- the user is standing at the
// path they named and can retype it. Sibling of `track_refusal` above.
fn track_target_refusal(target: &str, rejection: TargetRejection) -> String {
    // The stop between the two belongs here rather than on `message()`: that one
    // also reads mid-sentence after "Dotfile " and "Skipping 'X': ", where a
    // trailing period would be wrong.
    format!(
        "Cannot track '{target}': {}. {}",
        rejection.message(),
        rejection.suggestion()
    )
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
        return OperationResult::Failure(OperationFailure::Generic(
            crate::package::collect::directory::track_listing_refusal(&error),
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
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "Cannot track into '{}': its file name does not name a package",
            spec_path.display()
        )));
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
            return OperationResult::Failure(OperationFailure::Generic(track_target_refusal(
                target_path,
                rejection,
            )));
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
            sender.send_warning(warning).await;
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
        return OperationResult::Failure(OperationFailure::Generic(tracked_elsewhere_refusal(
            target_path,
            &trackers,
        )));
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
            return OperationResult::Failure(OperationFailure::Generic(track_refusal(&link)));
        }
        Some(refusal) => {
            return OperationResult::Failure(OperationFailure::Generic(format!(
                "{refusal}. Point the entry at a regular file instead."
            )));
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
                return OperationResult::Failure(OperationFailure::Generic(format!(
                    "Cannot read target file: it is not UTF-8 text ({e})"
                )));
            }
        },
        TargetState::Absent | TargetState::BelowNonDirectory { .. } => {
            return OperationResult::Failure(OperationFailure::Generic(format!(
                "Target file does not exist: {}",
                expanded_target.display()
            )));
        }
        TargetState::Directory => {
            return OperationResult::Failure(OperationFailure::Generic(format!(
                "Cannot track the target: {}",
                directory_at_target(&expanded_target)
            )));
        }
        // A link or fifo put there after the guard, refused in the guard's words.
        TargetState::Link(link) => {
            return OperationResult::Failure(OperationFailure::Generic(track_refusal(
                &link.refusal(),
            )));
        }
        TargetState::Irregular(refusal) => {
            return OperationResult::Failure(OperationFailure::Generic(format!(
                "{refusal}. Point the entry at a regular file instead."
            )));
        }
        TargetState::Unreadable(e) => {
            return OperationResult::Failure(OperationFailure::Generic(format!(
                "Cannot read target file: {}",
                FileSystemError::IoError(e)
            )));
        }
        TargetState::Undetermined(e) => {
            return OperationResult::Failure(OperationFailure::Generic(format!(
                "Cannot read target file: {e}"
            )));
        }
    };

    // Ahead of every write below. Track ends by recording the deployment, and a
    // state file it could not load is one it must not write over, so the copy
    // and the spec are not created for a record that cannot be kept.
    let mut recorder =
        match load_deploy_state(filesystem, config.state_directory().map(PathBuf::as_path)) {
            StateLoad::Usable(loaded) => {
                if let Some(warning) = loaded.directory_warning() {
                    sender.send_warning(warning.to_string()).await;
                }
                Recorder::new(loaded)
            }
            StateLoad::Unusable(failure) => {
                return OperationResult::Failure(OperationFailure::Generic(failure.to_string()));
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
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "Something is already at {}, where the spec for '{}' would be written. Remove it \
             first or choose a different name.",
            spec.spec_path.display(),
            spec.name
        )));
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
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "Cannot track into '{}': its spec has no directory to write beside",
            spec.spec_path.display()
        )));
    };
    let source_dir = base_dir.join(&spec.name);
    let source_path = source_dir.join(&filename);
    let relative_source = format!("{}/{filename}", spec.name);

    let already_exists = || {
        OperationResult::Failure(OperationFailure::Generic(format!(
            "Source file already exists at {}. Remove it first, or track a different file.",
            source_path.display()
        )))
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
            return OperationResult::Failure(OperationFailure::Generic(format!(
                "Nothing was tracked. {failure}"
            )));
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
            return OperationResult::Failure(OperationFailure::Generic(repository_write_refusal(
                &source_path,
                &e,
            )));
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
        return OperationResult::Failure(OperationFailure::Generic(spec_save_failure(
            &e,
            &source_path,
            &removal,
        )));
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
    // what recovers it rather than undoing two good writes.
    if let Err(e) = save_deploy_state(filesystem, &loaded) {
        return OperationResult::Failure(OperationFailure::Generic(unrecorded_track_failure(
            &e,
            &spec.name,
            &recorded_target,
            &spec.spec_path,
            &source_path,
        )));
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
            let message = repository_write_refusal(source, &refusal);
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
        let message = copy_fate(copy, &removal);
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
            copy_fate(copy, &removal).contains("was removed"),
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
            Err(FileSystemError::IoError(std::sync::Arc::new(
                std::io::Error::other("/dotfiles/gemrc/gemrc: permission denied"),
            )))
        });
        fs.expect_remove_file().never();

        let removal = remove_own_copy(&fs, copy, "gem: --no-document");

        assert!(matches!(removal, CopyRemoval::Failed(_)));
        // selfie cannot claim what it could not read, so the user is asked to check
        // the file rather than told to delete it.
        let message = copy_fate(copy, &removal);
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

        let warning = already_tracked_refusal(&fs, &target).expect("both stats answered");
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
        let error =
            PackageRepoError::FileSystemError(FileSystemError::IoError(std::sync::Arc::new(
                std::io::Error::other(format!("{spec}: Permission denied (os error 13)")),
            )));

        let message = spec_save_failure(
            &error,
            Path::new("/dotfiles/gemrc/gemrc"),
            &CopyRemoval::Removed,
        );

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
        );

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

    // A copy that survived says so, and says it differently. The two arms are
    // opposite advice -- one needs nothing from the user, the other needs a file
    // deleted before a retry can work -- so a reader must be able to tell them
    // apart.
    #[test]
    fn a_failed_spec_save_says_when_the_copy_survived() {
        let error = PackageRepoError::UnknownDotfileFields {
            path: PathBuf::from("/packages/creds.yml"),
            fields: "dotfiles[0].var".to_string(),
        };
        let removal = FileSystemError::IoError(std::sync::Arc::new(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Permission denied",
        )));

        let survived = spec_save_failure(
            &error,
            Path::new("/packages/creds/token"),
            &CopyRemoval::Failed(removal),
        );
        let removed = spec_save_failure(
            &error,
            Path::new("/packages/creds/token"),
            &CopyRemoval::Removed,
        );

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
            std::sync::Arc::new(std::io::Error::other("the disk filled up...")),
        ));

        let message = spec_save_failure(
            &error,
            Path::new("/dotfiles/gemrc/gemrc"),
            &CopyRemoval::Removed,
        );

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
            &FileSystemError::IoError(std::sync::Arc::new(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Permission denied",
            ))),
        );
        assert!(message.contains("Permission denied"), "got: {message}");
    }
}
