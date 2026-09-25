//! Deploying the secret-bearing entries of one package.
//!
//! Resolved content stays in memory throughout: compared against the target
//! directly, written owner-only, never recorded in deploy state and never put in
//! an event.

use std::path::Path;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::{
    commands::CommandRunner,
    config::SelfieConfig,
    dotfile_service::{
        port::{ConflictDetail, ConflictResolution},
        resolve::{ResolvedContent, check_resolvable, resolve_content},
    },
    fs::{
        filesystem::FileSystem,
        target::{TargetPath, deploy_target},
    },
    package::{ContentSource, DotfileEntry, event::EventSender},
};

use super::port::ApplyOptions;
use super::refusal::{
    Link, TargetGuard, TargetState, classify_link, directory_target_refusal, guard_target,
    read_target_state, refusal_warning, target_refusal,
};

/// Identify a secret-bearing entry by what produces it, never by its content.
///
/// Commands and var names come from the package file and are references, not
/// credentials, so they are safe to surface. Used as the `source` of the events
/// this path emits; the wording lives on [`ContentSource`] so apply, `dotfiles
/// list` and the MCP server cannot describe the same entry differently.
pub(super) fn secret_origin(content: &ContentSource<'_>) -> String {
    content.to_string()
}

/// A conflict summary describing shape without revealing content.
///
/// Line counts distinguish a rotated value (1 line vs 1 line) from a hand-edited
/// file (1 line vs 12 lines), which is the distinction a user needs in order to
/// choose between overwrite and skip. They are the most this can say: anything
/// derived from the bytes themselves is content.
fn secret_conflict_summary(origin: &str, incoming: &[u8], current: Option<&[u8]>) -> String {
    // Separators plus one, so a trailing newline reads as an extra line. Exact line
    // semantics do not matter here; the comparison between the two sides does.
    //
    // Both sides count through this one closure, so they cannot pluralize
    // differently. It is the only information a user gets before deciding whether
    // to overwrite a credential nothing recorded, so it should not read as though
    // selfie cannot count.
    let count = |b: &[u8]| {
        let n = b.iter().filter(|c| **c == b'\n').count() + 1;
        format!("{n} {}", crate::pluralize(n, "line", "lines"))
    };

    let current_side = match current {
        Some(bytes) => count(bytes),
        // Said plainly rather than shown as "0 lines", which would read as an
        // empty file and understate what an overwrite destroys.
        None => "could not be read".to_string(),
    };

    // Says that nothing is kept, because every other overwrite selfie performs
    // does keep a copy. A user who has seen that line elsewhere would otherwise
    // assume this overwrite is recoverable too, and accepting is the only way
    // past a secret conflict.
    format!(
        "  {}\n  target exists and differs from resolved output\n\n  \
         resolved output : {}\n  current target  : {current_side}\n  (content hidden)\n  \
         no copy of the current target is kept",
        origin,
        count(incoming),
    )
}

/// Outcome of handling one secret-bearing entry.
pub(super) enum SecretOutcome {
    Deployed,
    Skipped,
    Conflicted,
    /// Resolution failed; the caller decides whether to abort based on
    /// `stop_on_error`.
    Failed,
}

/// A phase either lets the apply continue, or ends it with an outcome.
///
/// `?` then reads as "stop here if this phase decided the entry's fate", which
/// is what every one of these steps does.
type Phase<T = ()> = Result<T, SecretOutcome>;

/// One secret-bearing entry, with its target resolved and classified.
struct SecretTarget<'a> {
    entry: &'a DotfileEntry,
    /// How the entry is named in events: the command, or the template and its
    /// var names. A reference drawn from the package file, never a value.
    origin: String,
    // Absolute, checked below. Unresolved is the type's job, not a caller's.
    path: TargetPath,
    /// The symlink at the target, if there is one, as of the check in
    /// `usable_target`. The deploy path re-asks before reading, because a resolve
    /// runs in between.
    link: Option<Link>,
}

/// Say that a symlinked target was replaced, naming the link and its destination.
///
/// Worded as what happened, and sent only after the write succeeds, so it can
/// never precede a refusal or a failed write.
// Deliberately not `refusal_warning`: nothing was refused. The entry deployed, and
// wording it as a refusal would have the user looking for a failure that is not
// there.
//
// Names the link alone when the destination could not be read. Printing "unknown"
// would be a fact about selfie rather than about their file.
fn replaced_link_warning(target: &SecretTarget<'_>, link: &Link) -> String {
    let what = match link.destination() {
        Some(dest) => format!(
            "'{}', which was a symlink to '{}'",
            target.path.display(),
            dest.display()
        ),
        None => format!("'{}', which was a symlink", target.path.display()),
    };

    format!(
        "Replaced {what}, with a regular file readable only by you. The credential was not written through the link. Remove the link or point the entry somewhere else if you meant it to survive."
    )
}

/// Deploying the secret-bearing entries of one package.
///
/// Resolved content stays in memory: compared against the target directly, written
/// owner-only, never recorded in deploy state, never put in an event.
// Exists so the phases below can be separate methods. Each wants most of this
// context, and as free functions they carried six or seven parameters apiece.
pub(super) struct SecretApply<'a, F, CR> {
    /// The package file's directory. Repository sources resolve against it and
    /// provider commands run in it.
    pub(super) base_dir: &'a Path,
    pub(super) filesystem: &'a F,
    pub(super) runner: &'a CR,
    pub(super) config: &'a SelfieConfig,
    pub(super) sender: &'a EventSender,
    pub(super) options: &'a ApplyOptions,
    /// The caller's live cancellation token, so Ctrl+C reaches a provider command
    /// that is blocked on a biometric or password prompt. Never a fresh token:
    /// `command_timeout` would then be the only way out of an interactive prompt.
    pub(super) token: &'a CancellationToken,
}

impl<F, CR> SecretApply<'_, F, CR>
where
    F: FileSystem,
    CR: CommandRunner,
{
    /// Deploy one secret-bearing entry.
    ///
    /// Reads as the sequence it is: refuse what can be refused without running
    /// anything, short-circuit a preview, resolve, then decide against what is
    /// already on disk.
    pub(super) async fn apply(&self, entry: &DotfileEntry, origin: String) -> SecretOutcome {
        match self.run(entry, origin).await {
            Ok(outcome) | Err(outcome) => outcome,
        }
    }

    async fn run(&self, entry: &DotfileEntry, origin: String) -> Phase<SecretOutcome> {
        let target = self.usable_target(entry, origin).await?;
        self.refuse_unresolvable(&target).await?;
        self.short_circuit_dry_run(&target).await?;

        let resolved = self.resolve(&target).await?;
        for warning in &resolved.warnings {
            self.sender.send_warning(warning).await;
        }

        // Asked again here, immediately before the read, because the answer taken in
        // `usable_target` is older than the resolve that ran in between.
        //
        // Advisory, not binding: the read below opens the target by pathname and
        // resolves it again, so a link planted between this stat and that open is
        // still followed and its destination still reaches a conflict resolver. What
        // this buys is that a link present at either ask is never read through. The
        // remaining window closes only with a non-following read on the port, which
        // selfie does not have. The write is safe either way, because the owner-only
        // writer refuses to follow.
        let link = match classify_link(self.filesystem.symlink_refusal(&target.path)) {
            Ok(link) => link,
            // Fails closed here as it does at the first ask. Falling back to the
            // earlier answer would swallow a refusal this code cannot interpret, at
            // the one point where the next statement reads the target.
            Err(refusal) => {
                self.sender
                    .send_warning(refusal_warning(target.entry.target(), &refusal))
                    .await;
                return Err(SecretOutcome::Failed);
            }
        };

        // A link is replaced whatever is behind it, so there is nothing to compare and
        // nothing to put to a resolver. Skipping all three is the fix: every one of
        // them reads or stats *through* the link.
        //
        // `settle_in_sync` matters as much as the read does. It asks `is_owner_only`,
        // which follows, so a link whose destination is already owner-only would be
        // left alone -- making the outcome depend on the destination's mode, which
        // ADR-0005 decision 3 removes.
        if let Some(link) = link {
            let outcome = self.write(&target, &resolved).await;
            if matches!(outcome, SecretOutcome::Deployed) {
                self.sender
                    .send_warning(replaced_link_warning(&target, &link))
                    .await;
            }
            return Ok(outcome);
        }
        let current = self.read_target(&target);
        // A directory put there during the resolve. The pre-command check refused
        // one already present, so this is the same refusal, arriving late; it never
        // reaches the resolver, which could only be asked to overwrite a directory.
        if matches!(current, TargetState::Directory) {
            self.sender
                .send_warning(directory_target_refusal(
                    target.entry.target(),
                    &target.path,
                ))
                .await;
            return Ok(SecretOutcome::Failed);
        }
        self.settle_in_sync(&target, &resolved, &current).await?;
        self.settle_conflict(&target, &resolved, &current).await?;

        Ok(self.write(&target, &resolved).await)
    }

    /// Expand the target, or refuse the entry naming the form that was refused.
    ///
    /// A relative target would write relative to the current directory, which is
    /// both surprising and dangerous for a credential; a `~user/…` one names a
    /// home directory selfie does not resolve.
    ///
    /// `Failed` rather than `Skipped`, and the same outcome
    /// [`refuse_unresolvable`](Self::refuse_unresolvable) returns: both are
    /// decided from the entry alone before anything runs, so returning different
    /// outcomes made `stop_on_error` end the run for one and not the other, and
    /// the documentation described the opposite. A refused entry is
    /// not a skipped one.
    async fn usable_target<'e>(
        &self,
        entry: &'e DotfileEntry,
        origin: String,
    ) -> Phase<SecretTarget<'e>> {
        let path = match deploy_target(self.filesystem, entry.target()) {
            Ok(path) => path,
            Err(rejection) => {
                self.sender
                    .send_warning(target_refusal(entry.target(), rejection))
                    .await;
                return Err(SecretOutcome::Failed);
            }
        };

        // Both questions, ahead of the classifier, because `read_target` reads
        // *through* a link and would hand the destination's bytes to a conflict
        // resolver -- someone else's file, revealed at a prompt -- and opening a fifo
        // blocks indefinitely.
        //
        // A link is replaced whatever it points at, unless it resolves to a fifo,
        // socket or device node, which the writer refuses. `Failed` rather than
        // `Skipped`, for the reason given above: this is decided from the target alone
        // before anything runs, and a refused entry is not a skipped one.
        let link = self.look(entry.target(), &path).await?;

        // The case the guard above does not cover: it excludes directories, because
        // opening one never blocks. Nothing may run for a target that provably cannot
        // be written, and a credential fetch can raise a biometric prompt, so this
        // sits ahead of every command.
        //
        // Only for a plain target. A link is replaced whatever it points at, so the
        // guard above -- which stats following the link -- is the whole of what
        // refuses one.
        if link.is_none()
            && let Some(refusal) = self.unwritable_target_refusal(entry.target(), &path)
        {
            self.sender.send_warning(refusal).await;
            return Err(SecretOutcome::Failed);
        }

        Ok(SecretTarget {
            entry,
            origin,
            path,
            link,
        })
    }

    /// Ask both of the guard's questions of `path`: the link to replace, if there is
    /// one, or `None` for a target that is not a link.
    ///
    /// A link whose destination is a fifo, socket or device node refuses the entry,
    /// as does such a target itself, since the writer refuses both. `source` is the
    /// target as the package file spells it, for the refusal.
    async fn look(&self, source: &str, path: &TargetPath) -> Phase<Option<Link>> {
        match guard_target(self.filesystem, path) {
            TargetGuard::Clear => Ok(None),
            TargetGuard::Link { link, behind: None } => Ok(Some(link)),
            TargetGuard::Link {
                behind: Some(refusal),
                ..
            }
            | TargetGuard::Refused(refusal) => {
                self.sender
                    .send_warning(refusal_warning(source, &refusal))
                    .await;
                Err(SecretOutcome::Failed)
            }
        }
    }

    /// Why a write to this target could never land, when that is the case.
    ///
    /// `source` is the target as the package file spells it, so the refusal names
    /// what the user wrote rather than the expanded path.
    ///
    /// Asked only of a target that is not a symlink. A link is replaced whatever it
    /// points at, because the rename lands on the link and never on the destination,
    /// so the only thing that refuses a link is what the writer itself refuses: a
    /// fifo, socket or device node, which
    /// [`FileSystem::irregular_target_refusal`] has already answered for.
    // Fails closed: an unclassifiable target refuses. Here the write really does land
    // on the target, so "nothing is known about it" is not a license to write a
    // credential over it.
    //
    // Framed like `refusal_warning` rather than by calling it: that takes a
    // `FileSystemError`, and every variant's `Display` embeds the path, so routing
    // this through it prints the path twice.
    fn unwritable_target_refusal(&self, source: &str, path: &TargetPath) -> Option<String> {
        match self.filesystem.is_directory(path) {
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

    /// Refuse anything decidable without running a command or reading a file.
    ///
    /// Applied before the dry-run short-circuit for the same reason the target
    /// check is: a preview that promises to run commands for an entry a real
    /// apply would refuse outright is reporting something that will never happen.
    async fn refuse_unresolvable(&self, target: &SecretTarget<'_>) -> Phase {
        if let Err(e) = check_resolvable(target.entry, self.base_dir) {
            self.sender
                .send_warning(format!(
                    "Failed to resolve '{}': {e}",
                    target.entry.target()
                ))
                .await;
            return Err(SecretOutcome::Failed);
        }
        Ok(())
    }

    /// End a dry run here, before anything is resolved.
    ///
    /// Resolving is what runs the user's commands, and a preview must not do
    /// that: it reaches a secret store and can raise a biometric or password
    /// prompt, which would make `--dry-run` an executing operation.
    ///
    /// The cost is that a dry run usually cannot say whether this entry would
    /// change — that needs the content, and the content needs the commands. It
    /// reports what it is declining to do instead.
    ///
    /// A symlinked target is the exception: it is replaced whatever the content
    /// turns out to be, so the outcome is known without resolving anything.
    async fn short_circuit_dry_run(&self, target: &SecretTarget<'_>) -> Phase {
        if !self.options.dry_run {
            return Ok(());
        }

        // Reported as the outcome class a real run would reach, which for a link is
        // a replacement. It still says commands will run: a preview must not imply
        // the credential is already known.
        //
        // Counted as a skip, because that is how a preview counts every deploy it
        // would make -- the repository-file path does the same -- so a dry run never
        // reports a deployment for a run that wrote nothing.
        let commands = target.entry.command_count();
        let reason = match &target.link {
            Some(link) => {
                let dest = match link.destination() {
                    Some(dest) => format!(" to '{}'", dest.display()),
                    None => String::new(),
                };
                format!(
                    "dry run: would run {commands} command(s), then replace the symlink{dest} with a regular file readable only by you"
                )
            }
            None => format!(
                "dry run: would run {commands} command(s); content not resolved, so no comparison is possible"
            ),
        };
        self.sender
            .send_dotfile_skipped(&target.origin, target.path.display(), reason)
            .await;
        Err(SecretOutcome::Skipped)
    }

    /// Run the entry's commands and produce its content.
    async fn resolve(&self, target: &SecretTarget<'_>) -> Phase<ResolvedContent> {
        match resolve_content(
            target.entry,
            self.base_dir,
            self.filesystem,
            self.runner,
            self.config.command_timeout(),
            self.token,
        )
        .await
        {
            Ok(resolved) => Ok(resolved),
            Err(e) => {
                // Safe to surface: `ResolveError`'s Display names commands, var
                // names, and — on failure only — truncated stderr. It never
                // carries resolved content.
                self.sender
                    .send_warning(format!(
                        "Failed to resolve '{}': {e}",
                        target.entry.target()
                    ))
                    .await;
                Err(SecretOutcome::Failed)
            }
        }
    }

    /// What is at the target: absent, readable, a directory, or unreadable.
    ///
    /// Conflating any two of those loses a credential.
    fn read_target(&self, target: &SecretTarget<'_>) -> TargetState {
        read_target_state(self.filesystem, &target.path)
    }

    /// Settle a target whose content already matches — including its mode.
    ///
    /// Matching content is not the whole guarantee. `write_file_private` is the
    /// only thing that establishes owner-only permissions, so returning here
    /// without it would leave a pre-existing world-readable target
    /// world-readable while reporting it as managed. That is exactly the
    /// adoption case this design's safety rests on, and the docs promise mode
    /// `0600` with no "unless the content already matched" attached.
    ///
    /// Tightening is conditional: rewriting a correct file on every apply would
    /// churn its inode and mtime and make "already in sync" a lie. A failure to
    /// read the mode is treated as "nothing to do" rather than rewriting on a
    /// guess — the content read above already succeeded, so it is close to
    /// unreachable.
    async fn settle_in_sync(
        &self,
        target: &SecretTarget<'_>,
        resolved: &ResolvedContent,
        current: &TargetState,
    ) -> Phase {
        let TargetState::Readable(bytes) = current else {
            return Ok(());
        };
        if bytes != &resolved.bytes {
            return Ok(());
        }

        if self.filesystem.is_owner_only(&target.path).unwrap_or(true) {
            self.sender
                .send_dotfile_skipped(&target.origin, target.path.display(), "already in sync")
                .await;
            return Err(SecretOutcome::Skipped);
        }

        // Same content, written the one way that establishes the mode atomically.
        if let Err(e) = self
            .filesystem
            .write_file_private(&target.path, &resolved.bytes)
        {
            // The error already names the target; naming it here too would print
            // the path twice.
            self.sender
                .send_warning(format!("Failed to tighten permissions: {e}"))
                .await;
            return Err(SecretOutcome::Failed);
        }

        self.sender
            .send_dotfile_skipped(
                &target.origin,
                target.path.display(),
                "already in sync (permissions tightened to owner-only)",
            )
            .await;
        Err(SecretOutcome::Skipped)
    }

    /// Settle a target that exists and differs.
    ///
    /// `auto_accept` is deliberately NOT consulted, unlike the repository-file
    /// path. It is a caller-settable parameter — the MCP server exposes it to an
    /// assistant — and honoring it would let a non-interactive caller silently
    /// overwrite a hand-edited credentials file with provider output, with no
    /// human ever seeing the conflict. A credential is not recoverable
    /// afterwards, because nothing about it was recorded.
    ///
    /// The spec is explicit: provider conflicts are never auto-resolved in
    /// non-interactive contexts; they are reported and skipped. The only way
    /// past this point is an interactive resolver actively returning Accept.
    ///
    /// Returning `Ok` means the caller may write: either the resolver accepted,
    /// or there was nothing at the target to begin with.
    async fn settle_conflict(
        &self,
        target: &SecretTarget<'_>,
        resolved: &ResolvedContent,
        current: &TargetState,
    ) -> Phase {
        if matches!(current, TargetState::Absent) {
            return Ok(());
        }

        // `None` for an unreadable target: there is nothing to describe or
        // reveal. The resolver is still consulted, because replacing a file only
        // needs write permission on its directory — so an overwrite may well be
        // possible and the user is entitled to choose it.
        let current: Option<&[u8]> = match current {
            TargetState::Readable(bytes) => Some(bytes),
            _ => None,
        };
        let summary = secret_conflict_summary(&target.origin, &resolved.bytes, current);

        if self.ask_resolver(target, resolved, current, &summary).await {
            return Ok(());
        }

        // Only the summary reaches the event. The values went to the resolver
        // and nowhere else.
        self.sender
            .send_dotfile_conflict(&target.origin, target.path.display(), &summary)
            .await;
        Err(SecretOutcome::Conflicted)
    }

    /// Put the conflict to the injected resolver, if there is one.
    ///
    /// The resolver is blocking and needs `'static`, so the values are moved in
    /// as owned buffers and the borrowed `ConflictDetail` is built inside the
    /// closure. That does not prevent a resolver copying the values — only
    /// retaining the borrow — but it keeps them off the `'static` boundary, so
    /// any copy is one an adapter took on purpose.
    ///
    /// `incoming` is a clone because `resolved.bytes` is still needed to write
    /// with if the answer is Accept. That is a second copy of the secret in
    /// memory, consistent with the documented absence of any scrubbing
    /// guarantee.
    async fn ask_resolver(
        &self,
        target: &SecretTarget<'_>,
        resolved: &ResolvedContent,
        current: Option<&[u8]>,
        summary: &str,
    ) -> bool {
        let Some(resolver) = &self.options.conflict_resolver else {
            return false;
        };

        let resolver = Arc::clone(resolver);
        let path = target.path.display().to_string();
        let incoming = resolved.bytes.clone();
        let current = current.unwrap_or_default().to_vec();
        let summary = summary.to_string();

        tokio::task::spawn_blocking(move || {
            resolver.resolve(
                &path,
                ConflictDetail::Secret {
                    summary: &summary,
                    incoming: &incoming,
                    current: &current,
                },
            )
        })
        .await
        .unwrap_or(ConflictResolution::Skip)
            == ConflictResolution::Accept
    }

    /// Write the resolved content and report it.
    ///
    /// Owner-only and atomic: no window in which the credential is
    /// world-readable, and no interrupted write leaving a truncated one behind.
    async fn write(&self, target: &SecretTarget<'_>, resolved: &ResolvedContent) -> SecretOutcome {
        if let Err(e) = self
            .filesystem
            .write_file_private(&target.path, &resolved.bytes)
        {
            // The error already names the target; naming it here too would print
            // the path twice.
            self.sender
                .send_warning(format!("Failed to write: {e}"))
                .await;
            return SecretOutcome::Failed;
        }

        self.sender
            // Never a copy. ADR-0003 keeps nothing derived from a credential on
            // disk, and the former content of a secret target is the credential
            // itself -- worse to persist than the checksum that ADR already
            // refuses. Owner-only permissions do not change that.
            .send_dotfile_deployed(&target.origin, target.path.display(), None)
            .await;

        // No deploy state is recorded: a stored checksum of a credential is a
        // confirmation oracle. See ADR-0003.
        SecretOutcome::Deployed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // selfie-ir68.21. Both sides render a line count, on the only line a user gets
    // before choosing whether to overwrite a credential that nothing recorded and
    // nothing can recover. Each side is asserted at one line AND at two, by
    // swapping the arguments, so a fix to one site cannot pass by being checked at
    // the other.

    // The fixtures are unterminated on purpose. The counter is separators plus one,
    // so "token\n" reads as two lines and a terminated fixture never produces the
    // singular. That counting is its own question, filed separately.
    #[test]
    fn a_one_line_side_reads_line_and_a_two_line_side_reads_lines() {
        let one: &[u8] = b"token";
        let two: &[u8] = b"token\nsecond";

        let summary = secret_conflict_summary("op read x", one, Some(two));
        // The trailing newline is part of the assertion: "1 line" is a prefix of
        // "1 lines", so a match without it would hold for the bug.
        assert!(
            summary.contains("resolved output : 1 line\n"),
            "the resolved side is not singular: {summary}"
        );
        assert!(
            summary.contains("current target  : 2 lines\n"),
            "the current side is not plural: {summary}"
        );

        let swapped = secret_conflict_summary("op read x", two, Some(one));
        assert!(
            swapped.contains("resolved output : 2 lines\n"),
            "the resolved side is not plural: {swapped}"
        );
        assert!(
            swapped.contains("current target  : 1 line\n"),
            "the current side is not singular: {swapped}"
        );
    }

    // An unreadable target says so rather than counting, and pluralizing must not
    // have disturbed that arm.
    #[test]
    fn an_unreadable_current_target_is_still_said_plainly() {
        let summary = secret_conflict_summary("op read x", b"token", None);
        assert!(
            summary.contains("current target  : could not be read"),
            "got: {summary}"
        );
    }

    // A link selfie could not read still names the link, with no destination clause.
    //
    // Driven here rather than through a real link: `read_link` succeeds on a dangling
    // link, so a file-system fixture always reaches the `Some` arm and this one is
    // unreachable from an integration test.
    #[test]
    fn a_replacement_warning_omits_a_destination_it_could_not_read() {
        let entry = DotfileEntry::new("creds.tpl", "~/.config/app/creds");
        let target = SecretTarget {
            entry: &entry,
            origin: "command: op read x".to_string(),
            path: crate::fs::target::repository_path(std::path::Path::new(
                "/home/u/.config/app/creds",
            )),
            // Deliberately no link, and not what either call below reads. The warning
            // takes its link as the second argument, because the deploy path hands it
            // the answer from the ask before the read rather than this one, which is
            // older than the resolve. A warning reading the field instead would name no
            // destination here, and the first assertion below would fail.
            link: None,
        };
        let link_to = |points_to: Option<&str>| {
            classify_link(Some(crate::fs::FileSystemError::SymlinkedTarget {
                path: std::path::PathBuf::from("/home/u/.config/app/creds"),
                points_to: points_to.map(std::path::PathBuf::from),
            }))
            .expect("a symlink refusal")
            .expect("is a link")
        };

        // One target, two links: the calls differ only in the argument, so the
        // difference between the messages isolates the destination clause.
        let with_destination =
            replaced_link_warning(&target, &link_to(Some("/home/u/.ssh/id_ed25519")));
        let without = replaced_link_warning(&target, &link_to(None));

        assert!(
            with_destination.contains("symlink to '/home/u/.ssh/id_ed25519'"),
            "got: {with_destination}"
        );
        // The pair is the assertion: the same call without a destination keeps the
        // link and drops only the clause naming where it pointed.
        assert!(without.contains("which was a symlink,"), "got: {without}");
        assert!(
            !without.contains("symlink to"),
            "no destination clause when the link could not be read: {without}"
        );
        assert!(
            without.contains("/home/u/.config/app/creds"),
            "the link itself is still named: {without}"
        );
    }
}
