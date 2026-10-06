//! Deploying the secret-bearing entries of one package.
//!
//! Resolved content stays in memory throughout: compared against the target
//! directly, written owner-only, never recorded in deploy state and never put in
//! an event.

use std::path::Path;

use tokio_util::sync::CancellationToken;

use crate::{
    commands::CommandRunner,
    config::SelfieConfig,
    dotfile_service::{
        port::{ConflictDetail, ConflictResolution, put_to_resolver},
        resolve::{ResolvedContent, resolve_content},
    },
    fs::{
        filesystem::{FileSystem, FileSystemError},
        target::TargetPath,
    },
    package::{
        DotfileEntry,
        event::{ConflictReport, EventSender, LinkAtTarget, Refusal, SkipReason, StepEnding},
    },
};

use super::classify::{SecretEntry, secret_target_link};
use super::port::ApplyOptions;
use super::refusal::{
    Link, TargetState, classify_link, classify_write, read_target_state, readable_or_refusal,
    refusal_for,
};

/// The program a command runs: its first word after any leading `NAME=value`
/// assignments, as the shell reads it, or `None` for a command the shell would
/// reject, such as one with an unclosed quote.
// Split as the shell splits, so a quoted assignment value containing spaces is one
// word. A full path and a wrapper such as `sh -c` or `env` are taken as written.
pub(super) fn program_of(command: &str) -> Option<String> {
    shlex::split(command)?
        .into_iter()
        .find(|word| !is_assignment(word))
}

/// Whether `word` is a shell variable assignment, `NAME=value`.
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Every program `entry` would run: its command's, or each binding's.
pub(super) fn programs_of(entry: &DotfileEntry) -> Vec<String> {
    match entry.command() {
        Some(command) => program_of(command).into_iter().collect(),
        None => entry
            .vars()
            .values()
            .filter_map(|c| program_of(c))
            .collect(),
    }
}

/// A conflict report describing shape without revealing content.
///
/// Line counts distinguish a rotated value (1 line vs 1 line) from a hand-edited
/// file (1 line vs 12 lines), which is the distinction a user needs in order to
/// choose between overwrite and skip. They are the most this can say: anything
/// derived from the bytes themselves is content.
fn secret_conflict_report(incoming: &[u8], current: &[u8]) -> ConflictReport {
    // Each piece `split_inclusive` yields is one line with its newline, so a
    // trailing newline ends a line rather than starting one, as `wc -l` counts. An
    // unterminated last line is a piece too, which `wc -l` would drop: "0 lines" for
    // content that is not empty would read as an empty file.
    let count = |b: &[u8]| b.split_inclusive(|&c| c == b'\n').count();
    ConflictReport::Hidden {
        resolved_lines: count(incoming),
        current_lines: count(current),
    }
}

/// Outcome of handling one secret-bearing entry.
pub(super) enum SecretOutcome {
    Deployed,
    Skipped,
    Conflicted,
    /// Refused or failed without a command failing; the caller decides whether
    /// to abort based on `stop_on_error`.
    Failed,
    /// A command the entry ran failed. Refused like `Failed`, and carries the
    /// failed command's program so the caller can hold back that program's later
    /// commands in this run.
    CommandFailed(String),
}

/// A phase either lets the apply continue, or ends it with an outcome.
///
/// `?` then reads as "stop here if this phase decided the entry's fate", which
/// is what every one of these steps does.
type Phase<T = ()> = Result<T, SecretOutcome>;

/// What the looks and the read found at a secret target just before the write.
enum Found {
    /// A symlink, which is replaced whatever it points at.
    Link(Link),
    /// The bytes of the regular file at the target, or `None` when nothing is
    /// there. `None` never stands for a target that could not be read: that is
    /// refused before a `Found` exists.
    Current(Option<Vec<u8>>),
}

/// Say that a symlinked target was replaced, naming the link and its destination.
///
/// Worded as what happened, and sent only after the write succeeds, so it can
/// never precede a refusal or a failed write.
// Deliberately not `refusal_for`: nothing was refused. The entry deployed, and
// wording it as a refusal would have the user looking for a failure that is not
// there.
//
// Names the link alone when the destination could not be read. Printing "unknown"
// would be a fact about selfie rather than about their file.
fn replaced_link_warning(target: &SecretEntry<'_>, link: &Link) -> String {
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
    /// The name of the package the entries belong to, for a refusal to name.
    pub(super) package: &'a str,
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
    /// Deploy one secret-bearing entry that `classify_entry` has already accepted,
    /// so everything refusable without running a command has been refused.
    ///
    /// Short-circuits a preview, resolves, then decides against what is already
    /// on disk.
    pub(super) async fn apply(&self, target: &SecretEntry<'_>) -> SecretOutcome {
        match self.run(target).await {
            Ok(outcome) | Err(outcome) => outcome,
        }
    }

    async fn run(&self, target: &SecretEntry<'_>) -> Phase<SecretOutcome> {
        self.short_circuit_dry_run(target).await?;

        let resolved = self.resolve(target).await?;
        for warning in &resolved.warnings {
            self.sender.send_warning(warning).await;
        }

        // Both questions again, immediately before the read, because the answers
        // taken when the entry was classified are older than the resolve that ran in between. A
        // link that appeared meanwhile is replaced like any other; a fifo, socket or
        // device node that appeared, at the target or behind a new link, refuses,
        // since the read would meet it and the writer refuse it.
        let found = match self.look(target.entry.target(), &target.path).await? {
            Some(link) => Found::Link(link),
            None => self.read_before_write(target).await?,
        };

        // A link is replaced whatever is behind it, so there is nothing to compare and
        // nothing to put to a resolver, and both are skipped.
        //
        // Skipping `settle_in_sync` matters as much as skipping the read: the mode of a
        // link's destination says nothing about the file that replaces the link, and
        // the outcome must not depend on it (ADR-0005 decision 3).
        let current = match found {
            Found::Link(link) => {
                let outcome = self.write(target, &resolved).await;
                if matches!(outcome, SecretOutcome::Deployed) {
                    self.sender
                        .send_warning(replaced_link_warning(target, &link))
                        .await;
                }
                return Ok(outcome);
            }
            Found::Current(current) => current,
        };
        self.settle_in_sync(target, &resolved, current.as_deref())
            .await?;
        self.settle_conflict(target, &resolved, current.as_deref())
            .await?;

        Ok(self.write(target, &resolved).await)
    }

    /// Read the target immediately before the write, and settle what the read found:
    /// a link to replace, a target to compare, or a refusal. A target that could not
    /// be read is refused, as the repository-file path refuses one, so it never
    /// reaches a resolver.
    ///
    /// Never replaces a link the read found without a look confirming it is still
    /// one, so whatever took its place is compared, not written over.
    async fn read_before_write(&self, target: &SecretEntry<'_>) -> Phase<Found> {
        let source = target.entry.target();
        let state = match self.read_target(target) {
            // Planted after the last look. Looked at once more, which refuses a link
            // to a fifo in the looks' own words at any timing.
            TargetState::Link(_) => match self.look(source, &target.path).await? {
                Some(link) => return Ok(Found::Link(link)),
                // No link any more: something else took its place since the read,
                // perhaps the user's own file. Read again and settle that instead;
                // replacing the link the read saw would overwrite it unasked.
                None => self.read_target(target),
            },
            state => state,
        };

        // The repository-file path's classifier and refusal, so the two paths cannot
        // answer differently about one file. Everything refused here appeared while
        // the command ran, since the check before it refuses what was already there.
        // A link here is one again where the look just found none, and a target that
        // changes under every look is refused rather than chased.
        match readable_or_refusal(source, &target.path, state) {
            Ok(current) => Ok(Found::Current(current)),
            Err(refusal) => {
                self.refuse(source, refusal).await;
                Err(SecretOutcome::Failed)
            }
        }
    }

    /// Ask both of the guard's questions of `path` again, sending the refusal when
    /// there is one: the link to replace, or `None` for a target that is not a
    /// link. `source` is the target as the package file spells it.
    async fn look(&self, source: &str, path: &TargetPath) -> Phase<Option<Link>> {
        match secret_target_link(self.filesystem, source, path) {
            Ok(link) => Ok(link),
            Err(refusal) => {
                self.refuse(source, refusal).await;
                Err(SecretOutcome::Failed)
            }
        }
    }

    /// Report `refusal` of the entry whose target the package file spells
    /// `entry_target`.
    async fn refuse(&self, entry_target: &str, refusal: Refusal) {
        self.sender
            .send_dotfile_refused(self.package, entry_target, refusal)
            .await;
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
    async fn short_circuit_dry_run(&self, target: &SecretEntry<'_>) -> Phase {
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
        let link = match &target.link {
            Some(link) => match link.destination() {
                Some(dest) => LinkAtTarget::To(dest.to_path_buf()),
                None => LinkAtTarget::DestinationUnknown,
            },
            None => LinkAtTarget::NoLink,
        };
        let reason = SkipReason::SecretDryRun {
            commands: target.entry.command_count(),
            link,
        };
        self.sender
            .send_dotfile_skipped(&target.source, target.path.display(), reason)
            .await;
        Err(SecretOutcome::Skipped)
    }

    /// Run the entry's commands and produce its content.
    async fn resolve(&self, target: &SecretEntry<'_>) -> Phase<ResolvedContent> {
        let step = self
            .sender
            .send_waiting(format!(
                "Running the commands that produce {}",
                target.entry.target()
            ))
            .await;
        let resolved = resolve_content(
            target.entry,
            self.base_dir,
            self.filesystem,
            self.runner,
            self.config.command_timeout(),
            self.token,
        )
        .await;
        // A template that escapes or cannot be read is refused before any binding
        // runs, so the step ran nothing.
        let refusal = resolved
            .as_ref()
            .err()
            .and_then(|e| e.as_refusal(target.entry.target()));
        let ending = match &resolved {
            Ok(_) => StepEnding::Succeeded,
            Err(_) if self.token.is_cancelled() => StepEnding::Cancelled,
            Err(_) if refusal.is_some() => StepEnding::NotRun,
            Err(_) => StepEnding::Failed,
        };
        self.sender.send_step_ended(step, ending).await;
        match resolved {
            Ok(resolved) => Ok(resolved),
            Err(e) => {
                // Safe to surface: `ResolveError`'s Display names commands, var
                // names, and — on failure only — truncated stderr. It never
                // carries resolved content.
                //
                // A template that escapes or cannot be read is a condition of the
                // entry's files, and is refused as classify refuses it. The resolve
                // reads the template before any binding runs, so no command ran.
                match refusal {
                    Some(refusal) => self.refuse(target.entry.target(), refusal).await,
                    None => {
                        self.sender
                            .send_warning(format!(
                                "Failed to resolve '{}': {e}",
                                target.entry.target()
                            ))
                            .await;
                    }
                }
                Err(match e.failed_command(target.entry).and_then(program_of) {
                    Some(program) => SecretOutcome::CommandFailed(program),
                    None => SecretOutcome::Failed,
                })
            }
        }
    }

    /// What is at the target: absent, readable, a directory, a link, a fifo, socket or
    /// device node, or unreadable.
    ///
    /// Conflating any two of those loses a credential.
    fn read_target(&self, target: &SecretEntry<'_>) -> TargetState {
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
        target: &SecretEntry<'_>,
        resolved: &ResolvedContent,
        current: Option<&[u8]>,
    ) -> Phase {
        let Some(bytes) = current else {
            return Ok(());
        };
        if bytes != resolved.bytes.as_slice() {
            return Ok(());
        }

        match self.filesystem.is_owner_only(&target.path) {
            // A link put there since the read: replaced like any link, never
            // reported in sync on the strength of a file it no longer is.
            Err(refusal @ FileSystemError::SymlinkedTarget { .. }) => {
                if let Ok(Some(link)) = classify_link(Some(refusal)) {
                    let outcome = self.write(target, resolved).await;
                    if matches!(outcome, SecretOutcome::Deployed) {
                        self.sender
                            .send_warning(replaced_link_warning(target, &link))
                            .await;
                    }
                    return Err(outcome);
                }
            }
            // The content read above succeeded, so a mode that cannot be read is
            // close to unreachable, and is not a reason to rewrite on a guess. Neither
            // error says anything about what is at the target.
            Ok(true) | Err(FileSystemError::IoError(_) | FileSystemError::HomeDirNotFound) => {
                self.sender
                    .send_dotfile_skipped(&target.source, target.path.display(), SkipReason::InSync)
                    .await;
                return Err(SecretOutcome::Skipped);
            }
            // What is at the target bars the write, as a guard would have found.
            Err(
                refusal @ (FileSystemError::IrregularTarget { .. }
                | FileSystemError::BelowNonDirectory { .. }
                | FileSystemError::DirectoryTarget { .. }),
            ) => {
                self.refuse(
                    target.entry.target(),
                    refusal_for(target.entry.target(), &refusal),
                )
                .await;
                return Err(SecretOutcome::Failed);
            }
            Ok(false) => {}
        }

        // Same content, written the one way that establishes the mode atomically.
        if let Err(e) = self
            .filesystem
            .write_file_private(&target.path, &resolved.bytes)
        {
            match classify_write(target.entry.target(), &target.path, &e) {
                Some(refusal) => self.refuse(target.entry.target(), refusal).await,
                // The error already names the target; naming it here too would
                // print the path twice.
                None => {
                    self.sender
                        .send_warning(format!("Failed to tighten permissions: {e}"))
                        .await;
                }
            }
            return Err(SecretOutcome::Failed);
        }

        self.sender
            .send_dotfile_skipped(
                &target.source,
                target.path.display(),
                SkipReason::PermissionsTightened,
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
        target: &SecretEntry<'_>,
        resolved: &ResolvedContent,
        current: Option<&[u8]>,
    ) -> Phase {
        let Some(current) = current else {
            return Ok(());
        };
        let report = secret_conflict_report(&resolved.bytes, current);

        let declined = match self.ask_resolver(target, resolved, current, &report).await {
            Some(ConflictResolution::Accept) => return Ok(()),
            Some(ConflictResolution::Skip) => true,
            None => false,
        };

        // Only the summary reaches the event. The values went to the resolver
        // and nowhere else.
        self.sender
            .send_dotfile_conflict(&target.source, target.path.display(), report, declined)
            .await;
        Err(SecretOutcome::Conflicted)
    }

    /// Put the conflict to the injected resolver, if there is one, and return
    /// its answer: `None` when there is no resolver, or when the resolver did
    /// not return one, as when it panicked.
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
        target: &SecretEntry<'_>,
        resolved: &ResolvedContent,
        current: &[u8],
        report: &ConflictReport,
    ) -> Option<ConflictResolution> {
        let Some(resolver) = &self.options.conflict_resolver else {
            return None;
        };
        // Rendered only once a resolver will read it.
        let summary = report.to_string();

        let path = target.path.display().to_string();
        let source = target.source.clone();
        let incoming = resolved.bytes.clone();
        let current = current.to_vec();

        put_to_resolver(resolver, move |r| {
            r.resolve(
                &path,
                ConflictDetail::Secret {
                    source: &source,
                    summary: &summary,
                    incoming: &incoming,
                    current: &current,
                },
            )
        })
        .await
    }

    /// Write the resolved content and report it.
    ///
    /// Owner-only and atomic: no window in which the credential is
    /// world-readable, and no interrupted write leaving a truncated one behind.
    async fn write(&self, target: &SecretEntry<'_>, resolved: &ResolvedContent) -> SecretOutcome {
        if let Err(e) = self
            .filesystem
            .write_file_private(&target.path, &resolved.bytes)
        {
            match classify_write(target.entry.target(), &target.path, &e) {
                Some(refusal) => self.refuse(target.entry.target(), refusal).await,
                // The error already names the target; naming it here too would
                // print the path twice.
                None => {
                    self.sender
                        .send_warning(format!("Failed to write: {e}"))
                        .await;
                }
            }
            return SecretOutcome::Failed;
        }

        self.sender
            // Never a copy. ADR-0003 keeps nothing derived from a credential on
            // disk, and the former content of a secret target is the credential
            // itself -- worse to persist than the checksum that ADR already
            // refuses. Owner-only permissions do not change that.
            .send_dotfile_deployed(&target.source, target.path.display(), None)
            .await;

        // No deploy state is recorded: a stored checksum of a credential is a
        // confirmation oracle. See ADR-0003.
        SecretOutcome::Deployed
    }
}

#[cfg(test)]
mod tests {
    // Leading assignments are the environment, not the program, however many.
    #[test]
    fn a_program_is_the_first_word_after_any_assignments() {
        let program = |command| super::program_of(command);
        assert_eq!(program("op read a").as_deref(), Some("op"));
        assert_eq!(program("OP_ACCOUNT=me op read a").as_deref(), Some("op"));
        assert_eq!(program("A=1 _B=2 gh auth token").as_deref(), Some("gh"));
        // Not an assignment: nothing before the `=`, or a name that is not one.
        assert_eq!(program("=x op").as_deref(), Some("=x"));
        assert_eq!(program("1A=x op").as_deref(), Some("1A=x"));
    }

    // A quoted assignment value is one word however many spaces it holds, in
    // single or double quotes, so the program is still the word after it.
    #[test]
    fn a_quoted_assignment_value_with_spaces_is_one_word() {
        let program = |command| super::program_of(command);
        assert_eq!(
            program("VAULT_ADDR='https://x/path with spaces' op read x").as_deref(),
            Some("op")
        );
        assert_eq!(
            program("VAULT_ADDR=\"https://x/path with spaces\" op read x").as_deref(),
            Some("op")
        );
        // A command the shell would reject names no program.
        assert_eq!(program("VAULT_ADDR='unclosed op read x"), None);
    }

    use super::*;

    // selfie-ir68.21. Both sides render a line count, on the only line a user gets
    // before choosing whether to overwrite a credential that nothing recorded and
    // nothing can recover. Each side is asserted at one line AND at two, by
    // swapping the arguments, so a fix to one site cannot pass by being checked at
    // the other.

    // The fixtures are unterminated, so `wc -l` alone would count each one short.
    #[test]
    fn a_one_line_side_reads_line_and_a_two_line_side_reads_lines() {
        let one: &[u8] = b"token";
        let two: &[u8] = b"token\nsecond";

        let summary = secret_conflict_report(one, two).to_string();
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

        let swapped = secret_conflict_report(two, one).to_string();
        assert!(
            swapped.contains("resolved output : 2 lines\n"),
            "the resolved side is not plural: {swapped}"
        );
        assert!(
            swapped.contains("current target  : 1 line\n"),
            "the current side is not singular: {swapped}"
        );
    }

    // selfie-ir68.24. A trailing newline ends a line and does not start one, so an
    // ordinary one-line file is "1 line", and an empty one is "0 lines". Asserted
    // on both sides by swapping the arguments.
    #[test]
    fn a_trailing_newline_does_not_start_a_line() {
        for (content, expected) in [
            (&b"token\n"[..], "1 line\n"),
            (&b"token\nsecond\n"[..], "2 lines\n"),
            (&b""[..], "0 lines\n"),
        ] {
            let other: &[u8] = b"x";
            let resolved = secret_conflict_report(content, other).to_string();
            assert!(
                resolved.contains(&format!("resolved output : {expected}")),
                "{content:?}: {resolved}"
            );
            let current = secret_conflict_report(other, content).to_string();
            assert!(
                current.contains(&format!("current target  : {expected}")),
                "{content:?}: {current}"
            );
        }
    }

    // A link selfie could not read still names the link, with no destination clause.
    //
    // Driven here rather than through a real link: `read_link` succeeds on a dangling
    // link, so a file-system fixture always reaches the `Some` arm and this one is
    // unreachable from an integration test.
    #[test]
    fn a_replacement_warning_omits_a_destination_it_could_not_read() {
        let entry = DotfileEntry::new("creds.tpl", "~/.config/app/creds");
        let target = SecretEntry {
            entry: &entry,
            source: crate::package::event::DotfileSource::Command("op read x".to_string()),
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
