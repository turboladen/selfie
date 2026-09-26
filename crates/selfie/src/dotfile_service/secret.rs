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
        resolve::{ResolvedContent, resolve_content},
    },
    fs::{
        filesystem::{FileSystem, FileSystemError},
        target::TargetPath,
    },
    package::{DotfileEntry, event::EventSender},
};

use super::classify::{SecretEntry, secret_target_link};
use super::port::ApplyOptions;
use super::refusal::{
    Link, TargetState, classify_link, directory_target_refusal, read_target_state, refusal_warning,
};

/// The program a command runs: its first word after any leading `NAME=value`
/// assignments, as the shell reads it.
// A full path, quoting, and a wrapper such as `sh -c` or `env` are taken as written.
pub(super) fn program_of(command: &str) -> Option<&str> {
    command.split_whitespace().find(|word| !is_assignment(word))
}

/// Whether `word` is a shell variable assignment, `NAME=value`.
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Every program `entry` would run: its command's, or each binding's.
pub(super) fn programs_of(entry: &DotfileEntry) -> Vec<&str> {
    match entry.command() {
        Some(command) => program_of(command).into_iter().collect(),
        None => entry
            .vars()
            .values()
            .filter_map(|c| program_of(c))
            .collect(),
    }
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
    /// Anything a write may land on after a comparison: nothing there, a readable
    /// file, or one that could not be read.
    State(TargetState),
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
            Found::State(current) => current,
        };
        self.settle_in_sync(target, &resolved, &current).await?;
        self.settle_conflict(target, &resolved, &current).await?;

        Ok(self.write(target, &resolved).await)
    }

    /// Read the target immediately before the write, and settle what the read found:
    /// a link to replace, a target to compare, or a refusal.
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

        let refusal = match state {
            TargetState::Absent | TargetState::Readable(_) | TargetState::Unreadable(_) => {
                return Ok(Found::State(state));
            }
            // A link again, where the look just found none: a target changing under
            // every look is refused rather than chased.
            TargetState::Link(link) => refusal_warning(source, &link.refusal()),
            TargetState::Irregular(refusal) => refusal_warning(source, &refusal),
            // A directory put there during the resolve. The pre-command check refused
            // one already present, so this is the same refusal, arriving late; the
            // resolver could only be asked to overwrite a directory.
            TargetState::Directory => directory_target_refusal(source, &target.path),
        };
        self.sender.send_warning(refusal).await;
        Err(SecretOutcome::Failed)
    }

    /// Ask both of the guard's questions of `path` again, sending the refusal when
    /// there is one: the link to replace, or `None` for a target that is not a
    /// link. `source` is the target as the package file spells it.
    async fn look(&self, source: &str, path: &TargetPath) -> Phase<Option<Link>> {
        match secret_target_link(self.filesystem, source, path) {
            Ok(link) => Ok(link),
            Err(refusal) => {
                self.sender.send_warning(refusal).await;
                Err(SecretOutcome::Failed)
            }
        }
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
    async fn resolve(&self, target: &SecretEntry<'_>) -> Phase<ResolvedContent> {
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
                Err(match e.failed_command(target.entry).and_then(program_of) {
                    Some(program) => SecretOutcome::CommandFailed(program.to_string()),
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
        current: &TargetState,
    ) -> Phase {
        let TargetState::Readable(bytes) = current else {
            return Ok(());
        };
        if bytes != &resolved.bytes {
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
            Ok(true) | Err(_) => {
                self.sender
                    .send_dotfile_skipped(&target.origin, target.path.display(), "already in sync")
                    .await;
                return Err(SecretOutcome::Skipped);
            }
            Ok(false) => {}
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
        target: &SecretEntry<'_>,
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
        target: &SecretEntry<'_>,
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
    async fn write(&self, target: &SecretEntry<'_>, resolved: &ResolvedContent) -> SecretOutcome {
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
    // Leading assignments are the environment, not the program, however many.
    #[test]
    fn a_program_is_the_first_word_after_any_assignments() {
        assert_eq!(super::program_of("op read a"), Some("op"));
        assert_eq!(super::program_of("OP_ACCOUNT=me op read a"), Some("op"));
        assert_eq!(super::program_of("A=1 _B=2 gh auth token"), Some("gh"));
        // Not an assignment: nothing before the `=`, or a name that is not one.
        assert_eq!(super::program_of("=x op"), Some("=x"));
        assert_eq!(super::program_of("1A=x op"), Some("1A=x"));
    }

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
        let target = SecretEntry {
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
