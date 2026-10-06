//! Whether an entry may go on to be deployed or compared, asked the same way by
//! apply and drift.
//!
//! One order of checks serves every command, so an entry that fails more than one
//! gets the same first reason from each: what the entry is, the target rule,
//! containment and, for a template entry, the template itself, then what is at the
//! target. Nothing here runs a command.

use std::path::{Path, PathBuf};

use crate::{
    config::SelfieConfig,
    dotfile_service::{deploy::resolve_source_path, resolve::read_template},
    fs::{
        filesystem::{FileSystem, FileSystemError, repository_read_refusal},
        target::{HomeDir, TargetPath, deploy_target, repository_path},
    },
    package::{
        ContentSource, DotfileEntry, Package, ScopedEntry, SpecOrigin, TargetCollision,
        event::{BaseKind, Condition, DotfileSource, Location, Refusal, RepoPath, SourceBase},
    },
    paths::is_within,
};

use super::refusal::{
    Link, TargetGuard, Unrun, guard_refusal, guard_target, guarded, link_at, located,
    path_state_refusal, readable_target, refusal_for, repository_condition, target_refusal,
    unreadable_target_refusal,
};

/// An entry that passed every check decidable before its content is read or
/// resolved.
pub(super) enum Classified<'e> {
    /// Copied from a file in the package repository.
    RepoFile(RepoFile<'e>),
    /// Produced by running commands.
    SecretBearing(SecretEntry<'e>),
}

/// A repository-file entry whose target and source have been checked.
pub(super) struct RepoFile<'e> {
    /// The entry's `source` as the package file spells it.
    pub(super) source: &'e str,
    /// How events name the source: relative to its base directory.
    pub(super) event_source: DotfileSource,
    /// `source` resolved against the package file's directory, and inside it.
    pub(super) source_path: PathBuf,
    /// The expanded target, which the target rule accepted and which is neither a
    /// symlink nor a fifo, socket or device node.
    pub(super) target: TargetPath,
}

/// A secret-bearing entry whose target has been checked.
pub(super) struct SecretEntry<'e> {
    /// The entry this classification is of.
    pub(super) entry: &'e DotfileEntry,
    /// How the entry is named in events: the command, or the template and its
    /// var names. A reference drawn from the package file, never a value.
    pub(super) source: DotfileSource,
    /// The expanded target, which the target rule accepted.
    pub(super) path: TargetPath,
    /// The symlink at the target as of classification, which the writer
    /// replaces, if there is one. Stale once a command has run.
    pub(super) link: Option<Link>,
}

/// What the caller will do with an entry that passes: deploy it, or only check it.
///
/// Both refuse the same entries. They differ in how a secret-bearing entry's
/// refusal is worded, and in how much a check looks at a secret target: it reads
/// nothing behind a symlink, since it reports such an entry as unverified either
/// way.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Purpose {
    Deploy,
    Check,
}

/// The home directory, looked up once for a whole run.
pub(super) struct ResolvedHome(Option<PathBuf>);

impl ResolvedHome {
    /// Ask `home` now, and answer every later question with what it said.
    pub(super) fn of<H: HomeDir + ?Sized>(home: &H) -> Self {
        Self(home.home().ok())
    }
}

impl HomeDir for ResolvedHome {
    fn home(&self) -> Result<PathBuf, FileSystemError> {
        self.0.clone().ok_or(FileSystemError::HomeDirNotFound)
    }
}

/// A package's entries that share a target in the environment in use, which
/// [`classify_entry`] refuses.
pub(super) struct PackageCollisions<'p> {
    package: &'p str,
    groups: Vec<TargetCollision<'p>>,
}

impl<'p> PackageCollisions<'p> {
    /// The collisions in `package` for `environment`.
    pub(super) fn of(package: &'p Package, home: &ResolvedHome, environment: &str) -> Self {
        Self {
            package: package.name(),
            groups: package.target_collisions(home, Some(environment)),
        }
    }
}

/// Classify `entry` for `purpose`, or refuse it with the warning it calls for.
///
/// Reads what is at the target and, for a template entry, the template; runs no
/// command. `base_dir` is the directory of the package file the entry came from,
/// and an entry among `collisions` is refused.
#[allow(clippy::too_many_arguments)]
pub(super) fn classify_entry<'e, F: FileSystem>(
    filesystem: &F,
    base_dir: &Path,
    source_base: Option<&SourceBase>,
    scoped: ScopedEntry<'e>,
    collisions: &PackageCollisions<'_>,
    purpose: Purpose,
) -> Result<Classified<'e>, Refusal> {
    let entry = scoped.entry;

    // First, so every entry sharing the target is refused with the same reason
    // whatever else is wrong with one of them, and none of them runs a command.
    // Refusing all of them, rather than deploying one, leaves nothing for the
    // deploy state to flip between: which entry would win depends only on list
    // order, which is not something the user chose.
    if let Some(collision) = collisions.groups.iter().find(|c| c.contains(&scoped)) {
        let consequence = match purpose {
            Purpose::Deploy => "so none of them is applied",
            Purpose::Check => "so drift cannot compare them",
        };
        return Err(Refusal {
            condition: Condition::Collision,
            at: Location::Entry,
            message: format!(
                "Skipping '{}' in package '{}': {}, {consequence}. {}",
                entry.target(),
                collisions.package,
                collision.describe(),
                collision.remedy()
            ),
        });
    }

    // Refused before anything runs. For a template that means the binding
    // commands -- real credential fetches, which can raise a biometric prompt --
    // never execute for a file that provably cannot be rendered.
    let content = entry.content_source().map_err(|invalid| Refusal {
        condition: Condition::InvalidEntry,
        at: Location::Entry,
        message: format!("Skipping '{}': {invalid}", entry.target()),
    })?;

    // The one target rule. A relative target would write relative to CWD, which is
    // surprising and potentially dangerous; a `~user/…` one names a home directory
    // selfie does not resolve.
    let target = deploy_target(filesystem, entry.target())
        .map_err(|rejection| target_refusal(entry.target(), rejection))?;

    match content {
        ContentSource::RepoFile(source) => {
            let Some(source_path) = within_package(base_dir, source) else {
                return Err(Refusal {
                    condition: Condition::Escapes,
                    at: Location::Source,
                    message: format!(
                        "Skipping '{source}': source path escapes YAML base directory"
                    ),
                });
            };

            // Ahead of every read of the target, not merely ahead of the write.
            // Reading a fifo blocks until a writer opens it, and a character device
            // would be read from, then written to. A symlink is refused whatever it
            // points at and whether or not its content already matches: reading it
            // would checksum the destination, a file selfie was never asked to
            // manage, and a repository-file entry never writes through a link, so
            // there is no outcome the read could change.
            if let Some(refusal) = guard_refusal(filesystem, &target) {
                return Err(refusal_for(source, &refusal));
            }

            Ok(Classified::RepoFile(RepoFile {
                source,
                event_source: file_source(source_base, &source_path),
                source_path,
                target,
            }))
        }
        secret @ (ContentSource::Template { .. } | ContentSource::Provider(_)) => {
            // Decided from the entry alone, so it is asked after the target rule and
            // ahead of anything that looks at the file system.
            //
            // The template is read here too, which runs nothing, so a template
            // that is missing, unreadable or a fifo is refused by a check and a
            // dry run exactly where the deploy would refuse it.
            let event_source = match secret {
                ContentSource::Template { source, vars } => {
                    // Refused, under either purpose: nothing has run, so nothing
                    // failed to resolve.
                    let Some(path) = within_package(base_dir, source) else {
                        return Err(Refusal {
                            condition: Condition::Escapes,
                            at: Location::Template,
                            message: format!(
                                "Skipping '{}': dotfile template '{source}' escapes the package \
                                 directory",
                                entry.target()
                            ),
                        });
                    };
                    if let Err(e) = read_template(filesystem, source, &path) {
                        return Err(e.refusal(entry.target()));
                    }
                    template_source(source_base, &path, vars.keys().cloned().collect())
                }
                ContentSource::Provider(command) => DotfileSource::Command(command.to_string()),
                ContentSource::RepoFile(_) => unreachable!("a repository file takes the arm above"),
            };

            let link = match purpose {
                Purpose::Deploy => deployable_secret_target(filesystem, entry.target(), &target)?,
                Purpose::Check => checkable_secret_target(filesystem, entry.target(), &target)?,
            };

            Ok(Classified::SecretBearing(SecretEntry {
                entry,
                source: event_source,
                path: target,
                link,
            }))
        }
    }
}

/// The directory `package`'s specs are read from, as events name it: `None` for
/// a package with no spec file behind it.
pub(super) fn source_base(config: &SelfieConfig, package: &Package) -> Option<SourceBase> {
    match package.origin() {
        SpecOrigin::PackageDirectory => Some(SourceBase {
            kind: BaseKind::PackageDirectory,
            directory: config.package_directory().clone(),
        }),
        SpecOrigin::DotfilesDirectory => Some(SourceBase {
            kind: BaseKind::DotfilesDirectory,
            directory: config.dotfiles_directory(),
        }),
        SpecOrigin::Memory => None,
    }
}

/// How events name the repository file at `path`: relative to `base` when it
/// lies inside it, and in full otherwise.
pub(super) fn file_source(base: Option<&SourceBase>, path: &Path) -> DotfileSource {
    let (base, path) = relative_to(base, path);
    DotfileSource::File(RepoPath { base, path })
}

/// How events name the template at `path`, which substitutes `vars`, placed as
/// [`file_source`] places a file.
pub(super) fn template_source(
    base: Option<&SourceBase>,
    path: &Path,
    vars: Vec<String>,
) -> DotfileSource {
    let (base, path) = relative_to(base, path);
    DotfileSource::Template {
        file: RepoPath { base, path },
        vars,
    }
}

// `path` relative to `base` with the base, when it lies inside it, else in
// full with no base.
fn relative_to(base: Option<&SourceBase>, path: &Path) -> (Option<SourceBase>, PathBuf) {
    match base.and_then(|base| Some((base, path.strip_prefix(&base.directory).ok()?))) {
        Some((base, relative)) => (Some(base.clone()), relative.to_path_buf()),
        None => (None, path.to_path_buf()),
    }
}

/// `source` resolved against the package file's directory, or `None` when it
/// escapes that directory. The one containment rule for every path an entry reads
/// from the package: a repository file's source and a template alike.
// Lexical: catches a written `..`, not a planted symlink. See
// `crate::paths::is_within`.
pub(super) fn within_package(base_dir: &Path, source: &str) -> Option<PathBuf> {
    let path = resolve_source_path(base_dir, source);
    is_within(&path, base_dir).then_some(path)
}

/// The link at a secret-bearing entry's target that a deploy would replace, or the
/// refusal for a target no write could land on.
fn deployable_secret_target<F: FileSystem>(
    filesystem: &F,
    source: &str,
    target: &TargetPath,
) -> Result<Option<Link>, Refusal> {
    // Both questions, before any command runs: what a link or a fifo at the target
    // means is decided here, not by the read after the fetch, which could only
    // refuse either. A link is replaced whatever it points at, unless it resolves
    // to a fifo, socket or device node, which the writer refuses.
    let link = secret_target_link(filesystem, source, target)?;

    // The case the guard does not cover: it excludes directories, because opening
    // one never blocks. Nothing may run for a target that provably cannot be
    // written, and a credential fetch can raise a biometric prompt, so this sits
    // ahead of every command.
    //
    // Only for a plain target. A link is replaced whatever it points at, so the
    // guard -- which stats following the link -- is the whole of what refuses one.
    if link.is_none()
        && let Some(refusal) = pre_command_refusal(filesystem, source, target, Purpose::Deploy)
    {
        return Err(refusal);
    }
    Ok(link)
}

/// The link at a secret-bearing entry's target, or the refusal a deploy would
/// also give, worded for a command that writes nothing.
// A link is reported as unverified however it resolves, so nothing behind it is
// asked: `guard_refusal` answers a link without the following stat, which can
// block on a destination sitting on a hung mount. A deploy asks, because it
// refuses a link to a fifo; a check has no answer to give that would change.
//
// The questions are `guard_refusal`'s, asked the same way; only a link's answer
// differs, being unverified here rather than refused.
fn checkable_secret_target<F: FileSystem>(
    filesystem: &F,
    source: &str,
    target: &TargetPath,
) -> Result<Option<Link>, Refusal> {
    match link_at(filesystem, target) {
        Err(unrecognized) => return Err(refusal_for(source, &unrecognized)),
        Ok(Some(link)) => return Ok(Some(link)),
        Ok(None) => {}
    }
    // A plain target on a hung mount still blocks here, as apply's stat of it does.
    if let Some(refusal) = filesystem.irregular_target_refusal(target) {
        return Err(refusal_for(source, &refusal));
    }
    match pre_command_refusal(filesystem, source, target, Purpose::Check) {
        Some(refusal) => Err(refusal),
        None => Ok(None),
    }
}

/// Ask both of the guard's questions of a secret-bearing entry's target: the link
/// to replace, if there is one, or `None` for a target that is not a link.
///
/// # Errors
///
/// The refusal of the entry, for a fifo, socket or device node at the
/// target or behind a link, since the writer refuses both. `source` is the target
/// as the package file spells it.
pub(super) fn secret_target_link<F: FileSystem>(
    filesystem: &F,
    source: &str,
    path: &TargetPath,
) -> Result<Option<Link>, Refusal> {
    match guard_target(filesystem, path) {
        TargetGuard::Clear => Ok(None),
        TargetGuard::Link { link, behind: None } => Ok(Some(link)),
        TargetGuard::Link {
            behind: Some(refusal),
            ..
        }
        | TargetGuard::Refused(refusal) => Err(refusal_for(source, &refusal)),
    }
}

/// Why selfie refuses a secret deploy to this target before running any command,
/// when it does, worded for `purpose`: a directory at the target, a component above
/// it that is not a directory, a target that exists and will not open for reading,
/// or one selfie could not classify.
///
/// `source` is the target as the package file spells it, so the refusal names
/// what the user wrote rather than the expanded path. Asked only of a target that
/// is not a symlink.
// Fails closed: an unclassifiable target refuses. Here the write really does land
// on the target, so "nothing is known about it" is not a license to write a
// credential over it.
//
// A target that will not open is often still writable, since a rename needs only
// the directory. It is refused because selfie will not overwrite a credential it
// cannot see, and it is refused here so that no provider command, and no prompt it
// raises, runs for it. The read after the command refuses one that changed meanwhile.
fn pre_command_refusal<F: FileSystem>(
    filesystem: &F,
    source: &str,
    path: &TargetPath,
    purpose: Purpose,
) -> Option<Refusal> {
    // Only a deploy would have run a command or written a credential, so only it
    // says it did not.
    let extra = match purpose {
        Purpose::Deploy => Unrun {
            unwritten: ", so selfie will not write a credential there",
            unrun: " No command was run.",
        },
        Purpose::Check => Unrun::NOTHING,
    };
    // One question, so a directory that appears between two stats cannot be missed.
    // Its absent reasons tell an empty path from one below a regular file or a
    // dangling link, where nothing can be and no write can land.
    let state = filesystem.directory_state(path.path());
    if let Some(refusal) = path_state_refusal(source, path, &state, extra) {
        return Some(refusal);
    }
    filesystem.open_for_read_refusal(path).map(|error| {
        let mut refusal = unreadable_target_refusal(source, path, &error);
        // The error ends its sentence without a period, so one goes before
        // the next sentence.
        if !extra.unrun.is_empty() {
            refusal.message.push('.');
            refusal.message.push_str(extra.unrun);
        }
        refusal
    })
}

/// What a repository-file entry's source and target hold, read for a decision.
pub(super) struct RepoRead {
    /// The source file's content.
    pub(super) source_content: String,
    /// The target's bytes, or `None` for nothing there.
    pub(super) current: Option<Vec<u8>>,
}

/// Read a classified repository-file entry's source and target, or refuse the
/// entry with the refusal apply and drift both give.
pub(super) fn read_repo_file<F: FileSystem>(
    filesystem: &F,
    entry: &RepoFile<'_>,
) -> Result<RepoRead, Refusal> {
    let RepoFile {
        source,
        source_path,
        target,
        ..
    } = entry;

    // Immediately ahead of the read, which is what this guards: a fifo source
    // blocks `read_file` until a writer arrives and hangs the command.
    if let Some(refusal) = filesystem.irregular_target_refusal(&repository_path(source_path)) {
        return Err(Refusal::found(
            located(guarded(&refusal), Location::Source),
            format!(
                "Skipping '{source}': {}. Replace it with a regular file.",
                repository_read_refusal(&refusal)
            ),
        ));
    }

    let source_content = filesystem.read_repository_file(source_path).map_err(|e| {
        Refusal::found(
            (repository_condition(&e), Location::Source),
            format!("Cannot read source '{}': {e}", source_path.display()),
        )
    })?;

    // Ahead of the decision, like the fifo refusal, so it holds under
    // `auto_accept`, under an interactive resolver, and in a dry run. The bytes
    // read here are also what the conflict diff shows, so the checksum and the diff
    // cannot disagree about the target.
    let current = readable_target(filesystem, source, target)?;

    Ok(RepoRead {
        source_content,
        current,
    })
}
