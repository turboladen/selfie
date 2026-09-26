//! Whether an entry may go on to be deployed or compared, asked the same way by
//! apply and drift.
//!
//! One order of checks serves every command, so an entry that fails more than one
//! gets the same first reason from each: what the entry is, the target rule,
//! containment, then what is at the target. Nothing here runs a command or reads
//! content that a refusal could have spared.

use std::path::{Path, PathBuf};

use crate::{
    dotfile_service::deploy::resolve_source_path,
    fs::{
        filesystem::{FileSystem, repository_read_refusal},
        target::{TargetPath, deploy_target, repository_path},
    },
    package::{ContentSource, DotfileEntry, event::EventSender},
    paths::is_within,
};

use super::refusal::{
    Link, TargetGuard, directory_target_refusal, guard_refusal, guard_target, readable_target,
    refusal_warning, target_refusal,
};

/// An entry refused before it was deployed or compared, with the warning that
/// says why.
pub(super) struct Refused(String);

impl Refused {
    /// Send the warning naming the refusal.
    pub(super) async fn send(self, sender: &EventSender) {
        sender.send_warning(self.0).await;
    }
}

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
    pub(super) origin: String,
    /// The expanded target, which the target rule accepted.
    pub(super) path: TargetPath,
    /// The symlink at the target as of classification, which the writer
    /// replaces, if there is one. Stale once a command has run.
    pub(super) link: Option<Link>,
}

/// Classify `entry`, or refuse it with the warning apply and drift both give.
///
/// Reads nothing but what is at the target, and runs no command. `base_dir` is the
/// directory of the package file the entry came from.
pub(super) fn classify_entry<'e, F: FileSystem>(
    filesystem: &F,
    base_dir: &Path,
    entry: &'e DotfileEntry,
) -> Result<Classified<'e>, Refused> {
    // Refused before anything runs. For a template that means the binding
    // commands -- real credential fetches, which can raise a biometric prompt --
    // never execute for a file that provably cannot be rendered.
    let content = entry
        .content_source()
        .map_err(|invalid| Refused(format!("Skipping '{}': {invalid}", entry.target())))?;

    // The one target rule. A relative target would write relative to CWD, which is
    // surprising and potentially dangerous; a `~user/…` one names a home directory
    // selfie does not resolve.
    let target = deploy_target(filesystem, entry.target())
        .map_err(|rejection| Refused(target_refusal(entry.target(), rejection)))?;

    match content {
        ContentSource::RepoFile(source) => {
            let Some(source_path) = within_package(base_dir, source) else {
                return Err(Refused(format!(
                    "Skipping '{source}': source path escapes YAML base directory"
                )));
            };

            // Ahead of every read of the target, not merely ahead of the write.
            // Reading a fifo blocks until a writer opens it, and a character device
            // would be read from, then written to. A symlink is refused whatever it
            // points at and whether or not its content already matches: reading it
            // would checksum the destination, a file selfie was never asked to
            // manage, and a repository-file entry never writes through a link, so
            // there is no outcome the read could change.
            if let Some(refusal) = guard_refusal(filesystem, &target) {
                return Err(Refused(refusal_warning(source, &refusal)));
            }

            Ok(Classified::RepoFile(RepoFile {
                source,
                source_path,
                target,
            }))
        }
        secret @ (ContentSource::Template { .. } | ContentSource::Provider(_)) => {
            // Decided from the entry alone, so it is asked after the target rule and
            // ahead of anything that looks at the file system.
            if let ContentSource::Template { source, .. } = secret
                && within_package(base_dir, source).is_none()
            {
                return Err(Refused(format!(
                    "Failed to resolve '{}': dotfile template '{source}' escapes the package \
                     directory",
                    entry.target()
                )));
            }

            // Both questions, before any command runs: what a link or a fifo at
            // the target means is decided here, not by the read after the fetch,
            // which could only refuse either. A link is replaced whatever it points
            // at, unless it resolves to a fifo, socket or device node, which the
            // writer refuses.
            let link = secret_target_link(filesystem, entry.target(), &target).map_err(Refused)?;

            // The case the guard does not cover: it excludes directories, because
            // opening one never blocks. Nothing may run for a target that provably
            // cannot be written, and a credential fetch can raise a biometric
            // prompt, so this sits ahead of every command.
            //
            // Only for a plain target. A link is replaced whatever it points at, so
            // the guard -- which stats following the link -- is the whole of what
            // refuses one.
            if link.is_none()
                && let Some(refusal) =
                    unwritable_target_refusal(filesystem, entry.target(), &target)
            {
                return Err(Refused(refusal));
            }

            Ok(Classified::SecretBearing(SecretEntry {
                entry,
                origin: secret.to_string(),
                path: target,
                link,
            }))
        }
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

/// Ask both of the guard's questions of a secret-bearing entry's target: the link
/// to replace, if there is one, or `None` for a target that is not a link.
///
/// # Errors
///
/// The warning refusing the entry, for a fifo, socket or device node at the
/// target or behind a link, since the writer refuses both. `source` is the target
/// as the package file spells it.
pub(super) fn secret_target_link<F: FileSystem>(
    filesystem: &F,
    source: &str,
    path: &TargetPath,
) -> Result<Option<Link>, String> {
    match guard_target(filesystem, path) {
        TargetGuard::Clear => Ok(None),
        TargetGuard::Link { link, behind: None } => Ok(Some(link)),
        TargetGuard::Link {
            behind: Some(refusal),
            ..
        }
        | TargetGuard::Refused(refusal) => Err(refusal_warning(source, &refusal)),
    }
}

/// Why a secret write to this target could never land, when that is the case.
///
/// `source` is the target as the package file spells it, so the refusal names
/// what the user wrote rather than the expanded path. Asked only of a target that
/// is not a symlink.
// Fails closed: an unclassifiable target refuses. Here the write really does land
// on the target, so "nothing is known about it" is not a license to write a
// credential over it.
//
// Framed like `refusal_warning` rather than by calling it: that takes a
// `FileSystemError`, and every variant's `Display` embeds the path, so routing
// this through it prints the path twice.
fn unwritable_target_refusal<F: FileSystem>(
    filesystem: &F,
    source: &str,
    path: &TargetPath,
) -> Option<String> {
    match filesystem.is_directory(path) {
        Ok(false) => None,
        Ok(true) => Some(format!(
            "{} No command was run.",
            directory_target_refusal(source, path)
        )),
        Err(e) => Some(format!(
            "Skipping '{source}': selfie could not determine what is at the target, so it will not write a credential there. No command was run. The check failed with: {e}"
        )),
    }
}

/// What a repository-file entry's source and target hold, read for a decision.
pub(super) struct RepoRead {
    /// The source file's content.
    pub(super) source_content: String,
    /// The target's bytes, or `None` for nothing there.
    pub(super) current: Option<Vec<u8>>,
}

/// Read a classified repository-file entry's source and target, or refuse the
/// entry with the warning apply and drift both give.
pub(super) fn read_repo_file<F: FileSystem>(
    filesystem: &F,
    entry: &RepoFile<'_>,
) -> Result<RepoRead, Refused> {
    let RepoFile {
        source,
        source_path,
        target,
    } = entry;

    // Immediately ahead of the read, which is what this guards: a fifo source
    // blocks `read_file` until a writer arrives and hangs the command.
    if let Some(refusal) = filesystem.irregular_target_refusal(&repository_path(source_path)) {
        return Err(Refused(format!(
            "Skipping '{source}': {}. Replace it with a regular file.",
            repository_read_refusal(&refusal)
        )));
    }

    let source_content = filesystem.read_file(source_path).map_err(|e| {
        Refused(format!(
            "Cannot read source '{}': {e}",
            source_path.display()
        ))
    })?;

    // Ahead of the decision, like the fifo refusal, so it holds under
    // `auto_accept`, under an interactive resolver, and in a dry run. The bytes
    // read here are also what the conflict diff shows, so the checksum and the diff
    // cannot disagree about the target.
    let current = readable_target(filesystem, source, target).map_err(Refused)?;

    Ok(RepoRead {
        source_content,
        current,
    })
}
