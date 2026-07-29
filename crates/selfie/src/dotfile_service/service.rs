//! DotfileService implementation
//!
//! This module provides the concrete implementation of the [`DotfileService`] trait.
//! It coordinates between the package repository (for loading package dotfiles),
//! the file system (for reading/writing dotfiles), and the application config
//! to perform dotfile deployment operations.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    commands::CommandRunner,
    config::SelfieConfig,
    dotfile_service::{
        deploy::{DeployDecision, compute_checksum, deploy_decision, resolve_source_path},
        diff::unified_diff,
        port::{ConflictDetail, ConflictResolution},
        resolve::{ResolvedContent, check_resolvable, resolve_content},
        state::{DeployState, DriftType},
    },
    fs::filesystem::{FileSystem, FileSystemError},
    package::{
        ContentSource, DotfileEntry, Package,
        event::{
            EventSender, EventStream, OperationContext, OperationFailure, OperationResult,
            OperationSuccess, PackageEvent, StepCount, metadata::OperationType,
        },
        port::PackageRepository,
    },
    paths::{is_within, normalize_path},
};

use super::port::{ApplyOptions, DotfileService};

/// Default deploy state filename
const DEPLOY_STATE_FILENAME: &str = "deploy-state.yml";

/// Concrete implementation of the [`DotfileService`] trait
///
/// Coordinates between the package repository, file system, and application
/// configuration to deploy dotfiles and check for drift.
///
/// Supports an optional second repository for standalone dotfiles (the `dotfiles/`
/// directory). When present, both repositories are scanned during apply and drift
/// operations.
#[derive(Debug, Clone)]
pub struct DotfileServiceImpl<R, F, CR> {
    package_repository: R,
    dotfiles_repository: Option<R>,
    filesystem: F,
    /// Runs the commands that produce secret-bearing dotfile content.
    runner: CR,
    config: SelfieConfig,
}

impl<R, F, CR> DotfileServiceImpl<R, F, CR>
where
    R: PackageRepository + Clone + Send + Sync + 'static,
    F: FileSystem + Clone + Send + Sync + 'static,
    CR: CommandRunner + Clone + Send + Sync + 'static,
{
    /// Create a new dotfile service instance
    pub fn new(package_repository: R, filesystem: F, runner: CR, config: SelfieConfig) -> Self {
        Self {
            package_repository,
            dotfiles_repository: None,
            filesystem,
            runner,
            config,
        }
    }

    /// Add a standalone dotfiles repository for the `dotfiles/` directory.
    ///
    /// When set, `apply` and `check_drift` operations will scan both the main
    /// package repository and this dotfiles repository.
    #[must_use]
    pub fn with_dotfiles_repository(mut self, repo: R) -> Self {
        self.dotfiles_repository = Some(repo);
        self
    }

    /// Collect packages from both the main package repository and the optional
    /// dotfiles repository, returning a combined list and any non-fatal warnings.
    ///
    /// Warnings are returned (rather than emitted directly) because collection
    /// happens before the event channel exists — callers emit them as
    /// `PackageEvent::Warning` once the stream is set up.
    fn collect_all_packages(
        package_repo: &R,
        dotfiles_repo: Option<&R>,
    ) -> Result<(Vec<Package>, Vec<String>), String> {
        let mut warnings = Vec::new();

        // A package file that does not parse is dropped by `valid_packages`, and
        // silence there is dangerous for this command specifically: apply is what
        // people run, and a dotfile that quietly stops deploying surfaces much
        // later as an authentication failure nobody traces back to a typo. The
        // run would otherwise report success having done nothing at all.
        let note_unparsable = |output: &crate::package::port::ListPackagesOutput,
                               warnings: &mut Vec<String>| {
            for invalid in output.invalid_packages() {
                warnings.push(format!(
                    "Skipping unparsable package file {}: {invalid}",
                    invalid.package_path().display()
                ));
            }
        };

        let mut packages = match package_repo.list_packages() {
            Ok(output) => {
                note_unparsable(&output, &mut warnings);
                output.valid_packages().cloned().collect::<Vec<_>>()
            }
            Err(e) => return Err(format!("Failed to load packages: {e}")),
        };

        let packages_count = packages.len();

        if let Some(dotfiles) = dotfiles_repo {
            match dotfiles.list_packages() {
                Ok(output) => {
                    note_unparsable(&output, &mut warnings);
                    packages.extend(output.valid_packages().cloned());
                }
                Err(e) => {
                    warnings.push(format!("Failed to load standalone dotfiles: {e}"));
                }
            }
        }

        // Detect duplicate names across packages/ and dotfiles/ directories.
        // Packages from packages/ take precedence (they appear first in the vec).
        if packages.len() > packages_count {
            let mut seen = std::collections::HashSet::new();
            for pkg in &packages[..packages_count] {
                seen.insert(pkg.name().to_string());
            }

            let mut deduped_dotfiles = Vec::new();
            for pkg in packages.drain(packages_count..) {
                if seen.contains(pkg.name()) {
                    warnings.push(format!(
                        "Duplicate name '{}' found in both packages/ and dotfiles/ — \
                         using the packages/ version",
                        pkg.name()
                    ));
                } else {
                    seen.insert(pkg.name().to_string());
                    deduped_dotfiles.push(pkg);
                }
            }
            packages.extend(deduped_dotfiles);
        }

        Ok((packages, warnings))
    }

    /// Create an event stream from an async operation.
    ///
    /// Delegates to the shared [`crate::package::event::create_event_stream`] utility.
    fn create_event_stream<Func, Fut>(f: Func) -> EventStream
    where
        Func: FnOnce(mpsc::Sender<PackageEvent>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        crate::package::event::create_event_stream(f)
    }
}

/// Resolve the deploy state file path.
///
/// Uses the configured `state_directory` if available, otherwise defaults to
/// `~/.local/state/selfie/deploy-state.yml` (XDG_STATE_HOME) via the filesystem abstraction.
///
/// # Errors
///
/// Returns `FileSystemError` if the home directory cannot be resolved.
fn deploy_state_path<F: FileSystem>(
    filesystem: &F,
    config: &SelfieConfig,
) -> Result<PathBuf, FileSystemError> {
    // Use configured state directory if available
    if let Some(state_dir) = config.state_directory() {
        return Ok(state_dir.join(DEPLOY_STATE_FILENAME));
    }

    // Default to XDG_STATE_HOME/selfie (~/.local/state/selfie) per the XDG Base
    // Directory Specification. Deploy state is per-machine, non-portable data —
    // exactly what XDG_STATE_HOME is designed for.
    //
    // expand_path canonicalizes, which fails if the directory doesn't exist yet.
    // To avoid requiring ~/.local/state/selfie to already exist on first run,
    // expand just "~" (which should always exist) and join the rest.
    let home = filesystem.expand_path(&PathBuf::from("~")).map_err(|_| {
        FileSystemError::IoError(std::sync::Arc::new(std::io::Error::other(
            "Cannot determine home directory for deploy state file",
        )))
    })?;
    let parent = home.join(".local").join("state").join("selfie");
    Ok(parent.join(DEPLOY_STATE_FILENAME))
}

/// Load the deploy state from disk, or return an empty state
fn load_deploy_state<F: FileSystem>(filesystem: &F, config: &SelfieConfig) -> DeployState {
    let path = match deploy_state_path(filesystem, config) {
        Ok(p) => p,
        Err(_) => return DeployState::empty(),
    };
    if !filesystem.path_exists(&path) {
        return DeployState::empty();
    }
    match filesystem.read_file(&path) {
        Ok(content) => serde_saphyr::from_str(&content).unwrap_or_else(|_| DeployState::empty()),
        Err(_) => DeployState::empty(),
    }
}

/// Save the deploy state to disk, owner-only where the platform allows it.
///
/// The contents are not credentials, but they map each repository-file dotfile
/// selfie manages on this machine to its target path, with checksums of the
/// repository files behind them — those it deployed, and those it found already
/// matching. At the process umask default that is typically
/// world-readable — a reconnaissance aid on a shared host, readable by people who
/// cannot read several of the files it describes.
///
/// Secret-bearing entries are *not* in here: they record nothing at all, per
/// ADR-0003. So this is not a complete list of what selfie manages, and tightening
/// it does not make ADR-0003's argument moot — that turns on what a stored checksum
/// of a credential would be, and there still is none.
///
/// Owner-only comes from `write_file_private`, the same method the secret-bearing
/// targets use, so the two cannot drift apart again. See its documentation for what
/// that does and does not guarantee away from Unix.
fn save_deploy_state<F: FileSystem>(
    filesystem: &F,
    config: &SelfieConfig,
    state: &DeployState,
) -> Result<(), FileSystemError> {
    let path = deploy_state_path(filesystem, config)?;
    let yaml = serde_saphyr::to_string(state).map_err(|e| {
        FileSystemError::IoError(std::sync::Arc::new(std::io::Error::other(e.to_string())))
    })?;
    filesystem.write_file_private(&path, yaml.as_bytes())
}

/// Check that a name is safe for use as a filesystem path component.
///
/// Rejects names containing path separators, `..`, or characters outside
/// the alphanumeric + hyphen + underscore set used for package names.
fn is_safe_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
}

/// Expand a deploy target without resolving its final component.
///
/// Expands a leading `~` and resolves `.`/`..` textually, and deliberately does
/// **not** canonicalize. Both writers this feeds --
/// [`FileSystem::write_file_no_follow`] for repository files and
/// [`FileSystem::write_file_private`] for credentials -- refuse or replace a
/// symlink at the path **as given**, so handing either an already-resolved path
/// forfeits the protection completely: `canonicalize` returns the link's
/// destination, and the writer is then looking at an ordinary file.
///
/// This is security-load-bearing, not tidiness, and it is measured rather than
/// argued (selfie-4m9): reintroducing the canonicalization here fails the
/// credential path's own symlink guard, both expansion tests, and every
/// repository-file refusal test **whose link resolves**. Do not "simplify" it back
/// to `expand_path` or `canonicalize`.
///
/// A *dangling* link survives that mutation, which is worth understanding rather
/// than treating as a gap in coverage: `canonicalize` fails on one, so the path
/// falls through unresolved and the refusal still fires. It is the same asymmetry
/// `.claude/rules/secrets.md` states -- canonicalization forfeits the guarantee
/// exactly when it *succeeds*.
///
/// No count of failing tests is given, deliberately. Two successive versions of
/// this comment carried one and both went stale as soon as a test was added -- the
/// second time inside the very change that corrected the first. A tally is a
/// measurement with an expiry date and every edit to the suite expires it, so this
/// says which tests fail and why instead.
///
/// Note the two halves of that fix are **not** symmetric, and an earlier version of
/// this comment claimed they were. Refusing at the write site alone would leave the
/// hole wide open, because a canonicalized path never reaches the writer as a link
/// at all. Expanding correctly alone leaves apply's own check
/// (`handle_apply`, before the deploy decision) catching every non-racing case --
/// which is why reverting the writer to `write_file` fails no test but the one
/// written for it. The writer is the TOCTOU defense; this expansion is what any of
/// it depends on.
///
/// A symlinked *parent* directory is still followed. That limitation is inherent
/// to both writers and is documented rather than papered over.
///
/// The converse also follows from resolving `..` textually: `a/link/../b` becomes
/// `a/b` here, where canonicalizing would have followed `link` first and landed
/// somewhere else. Both are consequences of not touching the filesystem, and the
/// textual answer is the one that matches what the user wrote.
///
/// Serves repository-file and secret-bearing entries alike, so it is on the
/// credential path. See `.claude/rules/secrets.md`; selfie-zv4b tracks making the
/// distinction a compile error rather than a comment.
pub fn expand_target_path<F: FileSystem>(filesystem: &F, target: &str) -> PathBuf {
    let raw = if target.starts_with('~') {
        // Expand only the leading `~` component; everything after it is joined
        // unresolved, which is the whole point. A home directory always exists,
        // so canonicalizing just that part succeeds.
        //
        // The named form `~user/x` is NOT supported: `expand_path` runs
        // `shellexpand::tilde`, which expands a bare `~` and `~/…` only and
        // returns `~user` unchanged, so canonicalizing it fails and the entry
        // falls through to the literal path below and is then skipped as
        // relative. Same as the repository-file path, which has never handled it
        // either.
        let (tilde, rest) = match target.split_once('/') {
            Some((tilde, rest)) => (tilde, Some(rest)),
            None => (target, None),
        };

        match filesystem.expand_path(Path::new(tilde)) {
            Ok(home) => match rest {
                Some(rest) => home.join(rest),
                None => home,
            },
            // No home directory, or no such user. Falling back to the literal path
            // leaves it relative, and the caller's absolute-path guard then refuses
            // it — better to skip than to write a credential into a directory
            // literally named `~` beneath the current directory.
            Err(_) => PathBuf::from(target),
        }
    } else {
        PathBuf::from(target)
    };

    normalize_path(&raw)
}

impl<R, F, CR> DotfileService for DotfileServiceImpl<R, F, CR>
where
    R: PackageRepository + Clone + std::fmt::Debug + Send + Sync + 'static,
    F: FileSystem + Clone + std::fmt::Debug + Send + Sync + 'static,
    CR: CommandRunner + Clone + std::fmt::Debug + Send + Sync + 'static,
{
    async fn apply_all(&self, options: ApplyOptions) -> EventStream {
        let collected =
            Self::collect_all_packages(&self.package_repository, self.dotfiles_repository.as_ref());
        let fs = self.filesystem.clone();
        let runner = self.runner.clone();
        let config = self.config.clone();

        Self::create_event_stream(move |tx| async move {
            let sender = EventSender::new_with_context(
                tx,
                OperationType::DotfileApply,
                String::new(),
                config.environment().to_string(),
                OperationContext::default(),
            );

            sender.send_started().await;

            let result = match collected {
                Ok((packages, warnings)) => {
                    for warning in warnings {
                        sender.send_warning(&warning).await;
                    }
                    handle_apply(&packages, &fs, &runner, &config, &sender, &options, None).await
                }
                Err(e) => {
                    OperationResult::Failure(crate::package::event::OperationFailure::Generic(e))
                }
            };

            sender.send_completed(result).await;
        })
    }

    async fn apply(&self, name: &str, options: ApplyOptions) -> EventStream {
        let collected =
            Self::collect_all_packages(&self.package_repository, self.dotfiles_repository.as_ref());
        let fs = self.filesystem.clone();
        let runner = self.runner.clone();
        let config = self.config.clone();
        let name = name.to_string();

        Self::create_event_stream(move |tx| async move {
            let sender = EventSender::new_with_context(
                tx,
                OperationType::DotfileApply,
                name.clone(),
                config.environment().to_string(),
                OperationContext::default(),
            );

            sender.send_started().await;

            let result = match collected {
                Ok((packages, warnings)) => {
                    for warning in warnings {
                        sender.send_warning(&warning).await;
                    }
                    handle_apply(
                        &packages,
                        &fs,
                        &runner,
                        &config,
                        &sender,
                        &options,
                        Some(&name),
                    )
                    .await
                }
                Err(e) => {
                    OperationResult::Failure(crate::package::event::OperationFailure::Generic(e))
                }
            };

            sender.send_completed(result).await;
        })
    }

    async fn check_drift(&self) -> EventStream {
        let collected =
            Self::collect_all_packages(&self.package_repository, self.dotfiles_repository.as_ref());
        let fs = self.filesystem.clone();
        let config = self.config.clone();

        Self::create_event_stream(move |tx| async move {
            let sender = EventSender::new_with_context(
                tx,
                OperationType::DotfileDrift,
                String::new(),
                config.environment().to_string(),
                OperationContext::default(),
            );

            sender.send_started().await;

            let result = match collected {
                Ok((packages, warnings)) => {
                    for warning in warnings {
                        sender.send_warning(&warning).await;
                    }
                    handle_check_drift(&packages, &fs, &config, &sender).await
                }
                Err(e) => {
                    OperationResult::Failure(crate::package::event::OperationFailure::Generic(e))
                }
            };

            sender.send_completed(result).await;
        })
    }

    async fn track_standalone(&self, name: &str, target_path: &str) -> EventStream {
        let dotfiles_repo = self.dotfiles_repository.clone();
        let fs = self.filesystem.clone();
        let config = self.config.clone();
        let name = name.to_string();
        let target_path = target_path.to_string();

        Self::create_event_stream(move |tx| async move {
            let sender = EventSender::new_with_context(
                tx,
                OperationType::DotfileTrack,
                name.clone(),
                config.environment().to_string(),
                OperationContext::default(),
            );
            sender.send_started().await;

            let result =
                handle_track_standalone(&name, &target_path, dotfiles_repo.as_ref(), &fs, &config);

            sender.send_completed(result).await;
        })
    }

    async fn track_for_package(&self, package_name: &str, target_path: &str) -> EventStream {
        let repo = self.package_repository.clone();
        let fs = self.filesystem.clone();
        let config = self.config.clone();
        let package_name = package_name.to_string();
        let target_path = target_path.to_string();

        Self::create_event_stream(move |tx| async move {
            let sender = EventSender::new_with_context(
                tx,
                OperationType::DotfileTrack,
                package_name.clone(),
                config.environment().to_string(),
                OperationContext::default(),
            );
            sender.send_started().await;

            let result = handle_track_for_package(&package_name, &target_path, &repo, &fs, &config);

            sender.send_completed(result).await;
        })
    }
}

/// Identify a secret-bearing entry by what produces it, never by its content.
///
/// Commands and var names come from the package file and are references, not
/// credentials, so they are safe to surface. Used as the `source` of the events
/// this path emits. The wording lives on [`ContentSource`] so that apply, `selfie
/// dotfiles list`, and the MCP server cannot describe the same entry differently.
///
/// Takes the content source rather than the entry so there is no refusal wording
/// to choose here: what this returns goes into the event stream and thence into
/// MCP's JSON, and an entry that cannot deploy has no business being named as
/// though it had a source. Callers hold the matched `ContentSource` already.
fn secret_origin(content: &ContentSource<'_>) -> String {
    content.to_string()
}

/// A conflict summary describing shape without revealing content.
///
/// Line counts distinguish a rotated value (1 line vs 1 line) from a hand-edited
/// file (1 line vs 12 lines), which is the distinction a user needs in order to
/// choose between overwrite and skip. They are the most this can say: anything
/// derived from the bytes themselves is content.
fn secret_conflict_summary(origin: &str, incoming: &[u8], current: Option<&[u8]>) -> String {
    // Counts separators plus one, so a trailing newline reads as an extra line.
    // Exact line semantics do not matter here; the comparison between the two
    // sides does.
    let lines = |b: &[u8]| b.iter().filter(|c| **c == b'\n').count() + 1;

    let current_side = match current {
        Some(bytes) => format!("{} lines", lines(bytes)),
        // Said plainly rather than shown as "0 lines", which would read as an
        // empty file and understate what an overwrite destroys.
        None => "exists but could not be read".to_string(),
    };

    format!(
        "  {}\n  target exists and differs from resolved output\n\n  \
         resolved output : {} lines\n  current target  : {current_side}\n  (content hidden)",
        origin,
        lines(incoming),
    )
}

/// What is at a secret-bearing entry's target when apply reaches it.
///
/// Kept distinct from `Option<Vec<u8>>` because "absent" and "present but
/// unreadable" call for opposite handling: the first is safe to write, the second
/// must never be overwritten without asking.
enum TargetState {
    Absent,
    Readable(Vec<u8>),
    Unreadable,
}

/// Outcome of handling one secret-bearing entry.
enum SecretOutcome {
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

/// One entry's identity, settled once so every phase names the same things.
struct SecretTarget<'a> {
    entry: &'a DotfileEntry,
    /// How the entry is named in events: the command, or the template and its
    /// var names. A reference drawn from the package file, never a value.
    origin: String,
    /// Expanded and absolute — and deliberately not canonicalized, so
    /// `write_file_private` still sees an unresolved final component.
    path: PathBuf,
}

/// Deploying the secret-bearing entries of one package.
///
/// The seven phases below were one function. Splitting them needed this struct
/// first: each phase wants most of this context, so as free functions they all
/// carried six or seven parameters, which is what kept them welded together.
///
/// Holds resolved content in memory only: it is compared against the target
/// directly, written with owner-only permissions, and never recorded in deploy
/// state. Nothing derived from it reaches an event. See ADR-0003.
struct SecretApply<'a, F, CR> {
    /// The package file's directory. Repository sources resolve against it and
    /// provider commands run in it.
    base_dir: &'a Path,
    filesystem: &'a F,
    runner: &'a CR,
    config: &'a SelfieConfig,
    sender: &'a EventSender,
    options: &'a ApplyOptions,
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
    async fn apply(&self, entry: &DotfileEntry, origin: String) -> SecretOutcome {
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

        let current = self.read_target(&target);
        self.settle_in_sync(&target, &resolved, &current).await?;
        self.settle_conflict(&target, &resolved, &current).await?;

        Ok(self.write(&target, &resolved).await)
    }

    /// Expand the target and refuse it if it is not absolute.
    ///
    /// A relative target would write relative to the current directory, which is
    /// both surprising and dangerous for a credential.
    async fn usable_target<'e>(
        &self,
        entry: &'e DotfileEntry,
        origin: String,
    ) -> Phase<SecretTarget<'e>> {
        let path = expand_target_path(self.filesystem, entry.target());

        if !path.is_absolute() {
            self.sender
                .send_warning(format!(
                    "Skipping '{}': target path '{}' is not absolute; targets must be absolute or start with '~/'",
                    entry.target(),
                    path.display()
                ))
                .await;
            return Err(SecretOutcome::Skipped);
        }

        Ok(SecretTarget {
            entry,
            origin,
            path,
        })
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
    /// The cost is that a dry run cannot say whether this entry would change —
    /// that needs the content, and the content needs the commands. It reports
    /// what it is declining to do instead.
    async fn short_circuit_dry_run(&self, target: &SecretTarget<'_>) -> Phase {
        if self.options.dry_run {
            self.sender
                .send_dotfile_skipped(
                    &target.origin,
                    target.path.display(),
                    format!(
                        "dry run: would run {} command(s); content not resolved, so no \
                         comparison is possible",
                        target.entry.command_count()
                    ),
                )
                .await;
            return Err(SecretOutcome::Skipped);
        }
        Ok(())
    }

    /// Run the entry's commands and produce its content.
    async fn resolve(&self, target: &SecretTarget<'_>) -> Phase<ResolvedContent> {
        match resolve_content(
            target.entry,
            self.base_dir,
            self.filesystem,
            self.runner,
            self.config.command_timeout(),
            &CancellationToken::new(),
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

    /// What is at the target: absent, readable, or present but unreadable.
    ///
    /// Conflating any two of those loses a credential. Read as raw bytes because
    /// neither side is guaranteed to be UTF-8, and a lossy decode would report
    /// two different files as identical.
    fn read_target(&self, target: &SecretTarget<'_>) -> TargetState {
        if !self.filesystem.path_exists(&target.path) {
            return TargetState::Absent;
        }

        match self.filesystem.read_file_bytes(&target.path) {
            Ok(bytes) => TargetState::Readable(bytes),
            // An unreadable file is still a file, and it may well be the
            // credential we would be destroying. Treating it as absent would
            // overwrite it with no prompt, which is what the repository-file
            // path avoids by routing an unreadable target into a conflict.
            Err(_) => TargetState::Unreadable,
        }
    }

    /// Settle a target whose content already matches — including its mode.
    ///
    /// Matching content is not the whole guarantee. `write_file_private` is the
    /// only thing that establishes owner-only permissions, so returning here
    /// without it would leave a pre-existing world-readable target
    /// world-readable while reporting it as managed. That is exactly the
    /// adoption case ADR-0003 cites as the reason this design is safe, and the
    /// docs promise mode `0600` with no "unless the content already matched"
    /// attached.
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
            self.sender
                .send_warning(format!(
                    "Failed to tighten permissions on '{}': {e}",
                    target.path.display()
                ))
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
            self.sender
                .send_warning(format!("Failed to write '{}': {e}", target.path.display()))
                .await;
            return SecretOutcome::Failed;
        }

        self.sender
            .send_dotfile_deployed(&target.origin, target.path.display())
            .await;

        // No deploy state is recorded: a stored checksum of a credential is a
        // confirmation oracle. See ADR-0003.
        SecretOutcome::Deployed
    }
}

/// How a refused deploy is reported, worded once.
///
/// Two sites produce it — the check in `handle_apply` and the write itself, which
/// still refuses if a link appears in between — and only the first is reachable in
/// a test, so the second would be free to drift if each formatted its own.
fn refusal_warning(source: &str, refusal: &FileSystemError) -> String {
    format!("Skipping '{source}': {refusal}")
}

/// Describes a single config file deployment operation
struct DeployUnit<'a> {
    source_path: &'a Path,
    target_path: &'a Path,
    source_content: &'a str,
    source_checksum: &'a str,
    source_key: &'a str,
}

/// Deploy a single config file to its target path, updating state and emitting events.
async fn perform_deploy<F: FileSystem>(
    filesystem: &F,
    deploy_state: &mut DeployState,
    sender: &EventSender,
    unit: &DeployUnit<'_>,
    dry_run: bool,
) -> Result<(), ()> {
    if dry_run {
        sender
            .send_dotfile_skipped(
                unit.source_path.display(),
                unit.target_path.display(),
                "dry run",
            )
            .await;
        return Ok(());
    }

    sender
        .send_dotfile_deploying(unit.source_path.display(), unit.target_path.display())
        .await;

    // Refuses a symlinked target rather than writing through it: the content would
    // otherwise land wherever the link points, which may be a path chosen by
    // whoever planted it. The target reaches here unresolved -- see
    // `expand_target_path` -- which is what lets the writer see the link at all.
    if let Err(e) =
        filesystem.write_file_no_follow(unit.target_path, unit.source_content.as_bytes())
    {
        // A refusal is not a failure. "Failed to write" would read as something
        // going wrong rather than as selfie declining, and the error already names
        // both the target and where the link points.
        //
        // Reaching the refusal arm here means the link appeared between the check
        // in `handle_apply` and this write, so no test exercises it — which is why
        // it shares `refusal_warning` with the checked path rather than repeating
        // the wording, where the two could drift apart unnoticed.
        let message = match &e {
            FileSystemError::SymlinkedTarget { .. } => refusal_warning(unit.source_key, &e),
            _ => format!("Failed to write '{}': {e}", unit.target_path.display()),
        };
        sender.send_warning(message).await;
        // `Err` has the caller count this as skipped and leaves the deploy state
        // untouched, so nothing is recorded as deployed that was not. An entry
        // already in the state keeps its previous checksums and is stale rather than
        // untracked, which is the honest record: for a refusal nothing was written,
        // and for an IO failure the target may have been truncated or partly written
        // first, since this writer truncates in place unlike `write_file_private`.
        // Recording either as a fresh deployment is what would make a later drift
        // check call the damage clean.
        return Err(());
    }
    deploy_state.record_deployment(
        unit.source_key,
        &unit.target_path.to_string_lossy(),
        unit.source_checksum,
    );

    sender
        .send_dotfile_deployed(unit.source_path.display(), unit.target_path.display())
        .await;
    Ok(())
}

/// Core logic for applying config files
async fn handle_apply<F, CR>(
    packages: &[Package],
    filesystem: &F,
    runner: &CR,
    config: &SelfieConfig,
    sender: &EventSender,
    options: &ApplyOptions,
    filter_name: Option<&str>,
) -> OperationResult
where
    F: FileSystem,
    CR: CommandRunner,
{
    let mut deploy_state = load_deploy_state(filesystem, config);

    let mut deployed_count: usize = 0;
    let mut skipped_count: usize = 0;
    let mut conflict_count: usize = 0;

    // Set when `stop_on_error` aborts the run. Held rather than returned so the
    // deploy state below is still saved.
    //
    // Note this flag currently governs secret-resolution failures only. The
    // repository-file failure paths in this loop have always continued past an
    // error, and changing that is a behavior change beyond this feature.
    let mut stopped: Option<String> = None;

    'packages: for package in packages {
        // If filtering by name, skip non-matching packages
        if let Some(name) = filter_name
            && package.name() != name
        {
            continue;
        }

        let dotfiles = package.dotfiles_for_environment(config.environment());
        if dotfiles.is_empty() {
            continue;
        }

        // Source paths resolve relative to the YAML file's parent directory,
        // so packages/fnm.yaml with source "fnm/init.fish" → packages/fnm/init.fish
        let base_dir = package
            .path()
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();

        let secret_apply = SecretApply {
            base_dir: &base_dir,
            filesystem,
            runner,
            config,
            sender,
            options,
        };

        for entry in &dotfiles {
            let source = match entry.content_source() {
                Ok(ContentSource::RepoFile(source)) => source,

                // Secret-bearing entries resolve their content by running
                // commands, compare it in memory, and record nothing.
                Ok(content @ (ContentSource::Template { .. } | ContentSource::Provider(_))) => {
                    match secret_apply.apply(entry, secret_origin(&content)).await {
                        SecretOutcome::Deployed => deployed_count += 1,
                        SecretOutcome::Skipped => skipped_count += 1,
                        SecretOutcome::Conflicted => conflict_count += 1,
                        SecretOutcome::Failed => {
                            skipped_count += 1;
                            if config.stop_on_error() {
                                // Break rather than return: anything already
                                // deployed in this run has been written to disk
                                // and recorded in the in-memory deploy state, and
                                // returning here would discard that record while
                                // leaving the files in place. The next drift check
                                // would then report correctly-deployed files as
                                // untracked.
                                stopped = Some(format!(
                                    "Stopped after failing to apply dotfile '{}' \
                                     (stop_on_error is enabled)",
                                    entry.target()
                                ));
                                break 'packages;
                            }
                        }
                    }
                    continue;
                }

                // Refused before anything runs. For a template that means the
                // binding commands — real credential fetches, which can raise a
                // biometric prompt — never execute for a file that provably
                // cannot be rendered.
                Err(invalid) => {
                    sender
                        .send_warning(format!("Skipping '{}': {invalid}", entry.target()))
                        .await;
                    skipped_count += 1;
                    continue;
                }
            };

            let source_path = resolve_source_path(&base_dir, source);

            // Runtime path traversal guard: verify resolved path stays within base_dir
            if !is_within(&source_path, &base_dir) {
                sender
                    .send_warning(format!(
                        "Skipping '{source}': source path escapes YAML base directory"
                    ))
                    .await;
                skipped_count += 1;
                continue;
            }

            let target_path = expand_target_path(filesystem, entry.target());

            // Enforce documented rule: target must be absolute after expansion.
            // A relative target would write relative to CWD, which is surprising
            // and potentially dangerous.
            if !target_path.is_absolute() {
                sender
                    .send_warning(format!(
                        "Skipping '{}': target path '{}' is not absolute; targets must be absolute or start with '~/'",
                        entry.target(),
                        target_path.display()
                    ))
                    .await;
                skipped_count += 1;
                continue;
            }

            // Read source file
            let source_content = match filesystem.read_file(&source_path) {
                Ok(content) => content,
                Err(e) => {
                    sender
                        .send_warning(format!(
                            "Cannot read source '{}': {e}",
                            source_path.display()
                        ))
                        .await;
                    skipped_count += 1;
                    continue;
                }
            };

            let source_checksum = compute_checksum(source_content.as_bytes());
            let target_exists = filesystem.path_exists(&target_path);

            // Read target if exists
            let target_checksum = if target_exists {
                match filesystem.read_file(&target_path) {
                    Ok(content) => compute_checksum(content.as_bytes()),
                    Err(_) => String::new(),
                }
            } else {
                String::new()
            };

            // Detect drift
            let drift = deploy_state.detect_drift(source, &source_checksum, &target_checksum);
            let decision =
                deploy_decision(&drift, target_exists, &source_checksum, &target_checksum);

            let unit = DeployUnit {
                source_path: &source_path,
                target_path: &target_path,
                source_content: &source_content,
                source_checksum: &source_checksum,
                source_key: source,
            };

            // Refuse a symlinked target before anything acts on the decision, so a
            // dry run reports what a real apply would do and an interactive resolver
            // is never asked to settle a conflict whose answer cannot be honored.
            // This also lands ahead of the conflict branch's diff, so the link
            // destination's content is never rendered for display.
            //
            // `Skip` is excluded because an in-sync target is not written to. Note
            // that branch is not inert: an untracked one is recorded as deployed
            // below, the same as any other already-matching target. That record is
            // about content, not about who wrote it — selfie did not write any
            // already-in-sync target — and it costs nothing here, because the next
            // repository edit makes the entry differ and the refusal fires then.
            //
            // What this check uniquely provides is the *preview* and the *un-asked
            // prompt*: deleting it fails those two tests and no others. Not the
            // warning -- `perform_deploy` emits the same text through
            // `refusal_warning`, so on the plain deploy path the user is told either
            // way. This check only gets there first.
            //
            // The writer's own `O_NOFOLLOW` refusal sits behind it as the TOCTOU
            // defense, reachable only when a link is planted in between, so removing
            // *that* changes nothing an ordinary test can observe while deleting the
            // only protection against a link planted mid-apply.
            // `the_writer_refuses_even_when_the_check_is_blinded` is the tripwire for
            // that half; neither layer is redundant.
            if !matches!(decision, DeployDecision::Skip(_))
                && let Some(refusal) = filesystem.symlink_refusal(&target_path)
            {
                sender.send_warning(refusal_warning(source, &refusal)).await;
                skipped_count += 1;
                continue;
            }

            match decision {
                DeployDecision::Deploy => {
                    if perform_deploy(
                        filesystem,
                        &mut deploy_state,
                        sender,
                        &unit,
                        options.dry_run,
                    )
                    .await
                    .is_ok()
                    {
                        if options.dry_run {
                            skipped_count += 1;
                        } else {
                            deployed_count += 1;
                        }
                    } else {
                        skipped_count += 1;
                    }
                }
                DeployDecision::Skip(reason) => {
                    // If this was an untracked file that's already in sync,
                    // record the state so future runs see DriftType::None.
                    if drift == DriftType::NotTracked && !options.dry_run {
                        deploy_state.record_deployment(
                            source,
                            &target_path.to_string_lossy(),
                            &source_checksum,
                        );
                    }
                    sender
                        .send_dotfile_skipped(source_path.display(), target_path.display(), &reason)
                        .await;
                    skipped_count += 1;
                }
                DeployDecision::Conflict => {
                    // Build the diff for display/resolution (needed by both
                    // the resolver and the fallback conflict event).
                    let target_content = filesystem.read_file(&target_path).unwrap_or_default();
                    let diff = unified_diff(
                        &target_content,
                        &source_content,
                        &target_path.to_string_lossy(),
                        &source_path.to_string_lossy(),
                    );

                    // Determine whether to accept: --yes flag, interactive
                    // resolver, or neither (skip with conflict event).
                    let accept = if options.auto_accept {
                        true
                    } else if let Some(resolver) = &options.conflict_resolver {
                        let src = source_path.display().to_string();
                        let tgt = target_path.display().to_string();
                        let d = diff.clone();
                        let r = Arc::clone(resolver);
                        tokio::task::spawn_blocking(move || {
                            r.resolve(
                                &tgt,
                                ConflictDetail::Diff {
                                    source: &src,
                                    diff: &d,
                                },
                            )
                        })
                        .await
                        .unwrap_or(ConflictResolution::Skip)
                            == ConflictResolution::Accept
                    } else {
                        false
                    };

                    if accept {
                        if perform_deploy(
                            filesystem,
                            &mut deploy_state,
                            sender,
                            &unit,
                            options.dry_run,
                        )
                        .await
                        .is_ok()
                        {
                            if options.dry_run {
                                skipped_count += 1;
                            } else {
                                deployed_count += 1;
                            }
                        } else {
                            skipped_count += 1;
                        }
                    } else {
                        sender
                            .send_dotfile_conflict(
                                source_path.display(),
                                target_path.display(),
                                &diff,
                            )
                            .await;
                        conflict_count += 1;
                    }
                }
            }
        }
    }

    // Save deploy state (skip in dry-run mode)
    if !options.dry_run
        && let Err(e) = save_deploy_state(filesystem, config, &deploy_state)
    {
        sender
            .send_warning(format!("Failed to save deploy state: {e}"))
            .await;
    }

    if let Some(message) = stopped {
        return OperationResult::Failure(OperationFailure::Generic(message));
    }

    let total = deployed_count + skipped_count + conflict_count;
    OperationResult::Success(OperationSuccess::DotfilesApplied {
        deployed_count,
        skipped_count,
        conflict_count,
        environment: config.environment().to_string(),
        steps_completed: StepCount::new(total, total),
    })
}

/// Core logic for checking drift
async fn handle_check_drift<F>(
    packages: &[Package],
    filesystem: &F,
    config: &SelfieConfig,
    sender: &EventSender,
) -> OperationResult
where
    F: FileSystem,
{
    let deploy_state = load_deploy_state(filesystem, config);

    let mut drift_count: usize = 0;
    let mut total_count: usize = 0;

    for package in packages {
        // Source paths resolve relative to the YAML file's parent directory
        let base_dir = package
            .path()
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();

        for entry in &package.dotfiles_for_environment(config.environment()) {
            total_count += 1;

            let source = match entry.content_source() {
                Ok(ContentSource::RepoFile(source)) => source,

                // Secret-bearing entries hold no deploy state, so there is nothing
                // to compare against. Resolving them here would run the user's
                // commands — leaking content into a read-only operation and
                // prompting for authentication from a command that should never
                // need it.
                //
                // They are reported as unverifiable rather than counted as drift.
                // Counting them would make `selfie dotfiles drift` — and the sync
                // status that reads it — permanently dirty on any machine with one
                // provider-sourced dotfile. ADR-0003 calls for identifying them
                // rather than inventing a drift classification for them.
                Ok(content @ (ContentSource::Template { .. } | ContentSource::Provider(_))) => {
                    sender
                        .send_dotfile_skipped(
                            secret_origin(&content),
                            expand_target_path(filesystem, entry.target()).display(),
                            "provider-sourced (not verifiable without resolving)",
                        )
                        .await;
                    continue;
                }

                // Refused for the same reasons apply refuses it, and worded the
                // same way. A drift check that reported an undeployable entry as
                // merely unverifiable would hide it behind the one status a user
                // is trained to ignore.
                Err(invalid) => {
                    sender
                        .send_warning(format!("Skipping '{}': {invalid}", entry.target()))
                        .await;
                    continue;
                }
            };

            let source_path = resolve_source_path(&base_dir, source);
            let target_path = expand_target_path(filesystem, entry.target());

            // Reject relative targets (same guard as handle_apply)
            if !target_path.is_absolute() {
                sender
                    .send_warning(format!(
                        "Skipping '{}': target path '{}' is not absolute",
                        entry.target(),
                        target_path.display()
                    ))
                    .await;
                continue;
            }

            // Runtime path traversal guard (same as handle_apply)
            if !is_within(&source_path, &base_dir) {
                sender
                    .send_warning(format!(
                        "Skipping '{source}': source path escapes YAML base directory"
                    ))
                    .await;
                continue;
            }

            // Read source — emit warning if missing instead of silently skipping
            let source_content = match filesystem.read_file(&source_path) {
                Ok(content) => content,
                Err(e) => {
                    sender
                        .send_warning(format!(
                            "Cannot read source '{}' for drift check: {e}",
                            source_path.display()
                        ))
                        .await;
                    continue;
                }
            };
            let source_checksum = compute_checksum(source_content.as_bytes());

            // Read target
            let target_checksum = if filesystem.path_exists(&target_path) {
                match filesystem.read_file(&target_path) {
                    Ok(content) => compute_checksum(content.as_bytes()),
                    Err(_) => String::new(),
                }
            } else {
                String::new()
            };

            let drift = deploy_state.detect_drift(source, &source_checksum, &target_checksum);

            if drift != DriftType::None {
                sender
                    .send_dotfile_drift_detected(target_path.display(), &drift)
                    .await;
                drift_count += 1;
            }
        }
    }

    OperationResult::Success(OperationSuccess::DotfileDriftChecked {
        drift_count,
        total_count,
        environment: config.environment().to_string(),
        steps_completed: StepCount::new(total_count, total_count),
    })
}

/// Handle `track_standalone`: copy target file into dotfiles dir, create a new
/// YAML spec, and record initial deploy state.
fn handle_track_standalone<R, F>(
    name: &str,
    target_path: &str,
    dotfiles_repo: Option<&R>,
    filesystem: &F,
    config: &SelfieConfig,
) -> OperationResult
where
    R: PackageRepository,
    F: FileSystem,
{
    let Some(dotfiles_repo) = dotfiles_repo else {
        return OperationResult::Failure(OperationFailure::Generic(
            "No dotfiles directory configured. Set `dotfiles_directory` in config.".to_string(),
        ));
    };

    // Reject names with path separators or traversal components
    if !is_safe_name(name) {
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "Invalid name '{name}': must contain only alphanumeric characters, hyphens, or underscores"
        )));
    }

    let dotfiles_dir = config.dotfiles_directory();

    // Expand and validate the target path
    let expanded_target = expand_target_path(filesystem, target_path);
    if !filesystem.path_exists(&expanded_target) {
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "Target file does not exist: {}",
            expanded_target.display()
        )));
    }

    // Read the target file content
    let content = match filesystem.read_file(&expanded_target) {
        Ok(c) => c,
        Err(e) => {
            return OperationResult::Failure(OperationFailure::Generic(format!(
                "Cannot read target file: {e}"
            )));
        }
    };

    // Determine source filename (just the basename of the target)
    let filename = expanded_target
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    // Check for existing spec (prevent silent overwrite)
    let spec_path = dotfiles_dir.join(format!("{name}.yml"));
    if filesystem.path_exists(&spec_path) {
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "A dotfile spec already exists at {}. Remove it first or choose a different name.",
            spec_path.display()
        )));
    }

    // Copy the file into dotfiles_dir/name/filename
    let source_dir = dotfiles_dir.join(name);
    let source_path = source_dir.join(&filename);

    if filesystem.path_exists(&source_path) {
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "Source file already exists at {}. Remove it first or choose a different name.",
            source_path.display()
        )));
    }

    if let Err(e) = filesystem.write_file(&source_path, content.as_bytes()) {
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "Cannot write source file: {e}"
        )));
    }

    // Create the YAML spec (spec_path already computed above for overwrite check)
    let package = crate::package::PackageBuilder::default()
        .name(name)
        .dotfiles(vec![DotfileEntry::new(
            format!("{name}/{filename}"),
            target_path,
        )])
        .path(spec_path.clone())
        .build();

    if let Err(e) = dotfiles_repo.save_package(&package, &spec_path) {
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "Cannot save spec: {e}"
        )));
    }

    // Record initial deploy state
    let checksum = compute_checksum(content.as_bytes());
    let mut deploy_state = load_deploy_state(filesystem, config);
    let source_key = format!("{name}/{filename}");
    deploy_state.record_deployment(&source_key, target_path, &checksum);
    if let Err(e) = save_deploy_state(filesystem, config, &deploy_state) {
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "Cannot save deploy state: {e}"
        )));
    }

    OperationResult::Success(OperationSuccess::DotfileTracked {
        name: name.to_string(),
        source_path,
        target_path: target_path.to_string(),
        was_already_tracked: false,
        environment: config.environment().to_string(),
        steps_completed: StepCount::new(1, 1),
    })
}

/// Handle `track_for_package`: load an existing package, copy the target file
/// alongside the YAML, add a dotfiles entry, save, and record deploy state.
fn handle_track_for_package<R, F>(
    package_name: &str,
    target_path: &str,
    repo: &R,
    filesystem: &F,
    config: &SelfieConfig,
) -> OperationResult
where
    R: PackageRepository,
    F: FileSystem,
{
    // Load the existing package
    let mut package_blob = match repo.get_package(package_name) {
        Ok(blob) => blob,
        Err(e) => {
            return OperationResult::Failure(OperationFailure::Generic(format!(
                "Cannot load package '{package_name}': {e}"
            )));
        }
    };

    // Check if this target is already tracked in the package
    let expanded_target = expand_target_path(filesystem, target_path);
    if package_blob
        .package()
        .dotfiles()
        .iter()
        .any(|entry| expand_target_path(filesystem, entry.target()) == expanded_target)
    {
        return OperationResult::Success(OperationSuccess::DotfileTracked {
            name: package_name.to_string(),
            source_path: expanded_target,
            target_path: target_path.to_string(),
            was_already_tracked: true,
            environment: config.environment().to_string(),
            steps_completed: StepCount::new(1, 1),
        });
    }

    // Validate the target path
    if !filesystem.path_exists(&expanded_target) {
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "Target file does not exist: {}",
            expanded_target.display()
        )));
    }

    // Read the target file content
    let content = match filesystem.read_file(&expanded_target) {
        Ok(c) => c,
        Err(e) => {
            return OperationResult::Failure(OperationFailure::Generic(format!(
                "Cannot read target file: {e}"
            )));
        }
    };

    // Determine where to copy the file — alongside the package YAML
    let package_dir = package_blob
        .file_path()
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let filename = expanded_target
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let source_dir = package_dir.join(package_name);
    let source_path = source_dir.join(&filename);
    let relative_source = format!("{package_name}/{filename}");

    // Prevent silent overwrite of existing source files
    if filesystem.path_exists(&source_path) {
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "Source file already exists at {}. Remove it first or choose a different file.",
            source_path.display()
        )));
    }

    // Copy the file
    if let Err(e) = filesystem.write_file(&source_path, content.as_bytes()) {
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "Cannot write source file: {e}"
        )));
    }

    // Add dotfiles entry and save — source is relative to the YAML's parent dir
    package_blob
        .package_mut()
        .add_dotfile(DotfileEntry::new(&relative_source, target_path));

    let file_path = package_blob.file_path().to_path_buf();
    if let Err(e) = repo.save_package(package_blob.package(), &file_path) {
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "Cannot save updated package: {e}"
        )));
    }

    // Record initial deploy state
    let checksum = compute_checksum(content.as_bytes());
    let mut deploy_state = load_deploy_state(filesystem, config);
    deploy_state.record_deployment(&relative_source, target_path, &checksum);
    if let Err(e) = save_deploy_state(filesystem, config, &deploy_state) {
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "Cannot save deploy state: {e}"
        )));
    }

    OperationResult::Success(OperationSuccess::DotfileTracked {
        name: package_name.to_string(),
        source_path,
        target_path: target_path.to_string(),
        was_already_tracked: false,
        environment: config.environment().to_string(),
        steps_completed: StepCount::new(1, 1),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::RealFileSystem;

    #[test]
    fn test_expand_target_path_absolute() {
        let fs = RealFileSystem;
        let result = expand_target_path(&fs, "/tmp/some/file");
        // Should be an absolute path starting with /tmp
        assert!(result.is_absolute());
        assert!(result.starts_with("/tmp"));
    }

    #[test]
    fn test_expand_target_path_tilde() {
        let fs = RealFileSystem;
        let result = expand_target_path(&fs, "~/test-file");
        // Should start with the actual home directory, not literal "~"
        assert!(result.is_absolute());
        assert!(
            !result.starts_with("~"),
            "Tilde should be expanded to actual home directory"
        );
        assert!(
            result.to_string_lossy().contains("test-file"),
            "Should preserve the filename after tilde expansion"
        );
    }
}
