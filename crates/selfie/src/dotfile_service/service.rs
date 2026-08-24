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
        state::{DeployState, DriftType, ParseFailure},
    },
    fs::{
        filesystem::{FileSystem, FileSystemError},
        target::{
            TargetPath, TargetRejection, deploy_target, expand_target_path, portable_target,
            repository_path, state_file_path,
        },
    },
    package::{
        ContentSource, DotfileEntry, EnvironmentField, Package, TopLevelKeys,
        describe_unknown_key_in,
        event::{
            EventSender, EventStream, OperationContext, OperationFailure, OperationResult,
            OperationSuccess, PackageEvent, StepCount, metadata::OperationType,
        },
        port::PackageRepository,
    },
    paths::is_within,
    privilege::{Privilege, SudoPolicy, SudoRefusal, WriteScope},
};

use super::port::{ApplyOptions, DotfileService};

/// Default deploy state filename
const DEPLOY_STATE_FILENAME: &str = "deploy-state.yml";

/// How a cancelled apply is reported.
///
/// One constant so the two sites that stop a run — between entries, and after a
/// provider command was killed mid-flight — cannot describe the same event two
/// different ways.
const APPLY_CANCELLED: &str = "Apply cancelled";

/// Concrete implementation of the [`DotfileService`] trait
///
/// Coordinates between the package repository, file system, and application
/// configuration to deploy dotfiles and check for drift.
///
/// Supports an optional second repository for standalone dotfiles (the `dotfiles/`
/// directory). When present, both repositories are scanned during apply and drift
/// operations.
#[derive(Debug, Clone)]
pub struct DotfileServiceImpl<R, F, CR, P> {
    package_repository: R,
    dotfiles_repository: Option<R>,
    filesystem: F,
    /// Runs the commands that produce secret-bearing dotfile content.
    runner: CR,
    config: SelfieConfig,
    /// Token used to signal graceful cancellation of in-flight operations.
    cancellation_token: CancellationToken,
    /// Whether this process reached root through `sudo`, and whether that was
    /// asked for.
    sudo_policy: SudoPolicy<P>,
}

impl<R, F, CR, P> DotfileServiceImpl<R, F, CR, P>
where
    R: PackageRepository + Clone + Send + Sync + 'static,
    F: FileSystem + Clone + Send + Sync + 'static,
    CR: CommandRunner + Clone + Send + Sync + 'static,
    P: Privilege,
{
    /// Create a new dotfile service instance
    ///
    /// `cancellation_token` is required rather than defaulted, mirroring
    /// [`PackageServiceImpl::new`](crate::package::service::PackageServiceImpl::new).
    /// Apply runs the user's provider commands, so an adapter that cannot supply
    /// a live token has to say so at its own boundary — where it is visible —
    /// instead of a fresh token being conjured deep in the resolve path, which is
    /// what made Ctrl+C a no-op here for as long as this path could run commands.
    ///
    /// `sudo_policy` is required for the same reason. Sniffing the environment
    /// inline would leave the MCP server — a second driving adapter that needs
    /// the same refusal — to repeat the rule, and would leave no way to test
    /// "running under sudo" short of running the suite as root.
    pub fn new(
        package_repository: R,
        filesystem: F,
        runner: CR,
        config: SelfieConfig,
        cancellation_token: CancellationToken,
        sudo_policy: SudoPolicy<P>,
    ) -> Self {
        Self {
            package_repository,
            dotfiles_repository: None,
            filesystem,
            runner,
            config,
            cancellation_token,
            sudo_policy,
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

    /// The refusal this run must report instead of writing anything, if any.
    ///
    /// Every caller evaluates this *before* building the event stream, so `P`
    /// itself never enters the spawned task — only the plain [`SudoRefusal`]
    /// does. `Send + Sync` are still required, because the policy is a field of a
    /// service the trait declares `Send + Sync`; what evaluating early buys is
    /// that the [`DotfileService`] impl needs no `'static`, `Clone` or `Debug`
    /// bound on `P`, which the other three ports all carry.
    ///
    /// That is a claim about the trait impl and not about the type. `Clone` is
    /// derived, so cloning the service still requires `P: Clone` — which is why
    /// the MCP server's `RealPrivilege` has it. A `P` with none of the three can
    /// drive every method here; it just cannot be cloned along with the service.
    fn sudo_refusal(&self) -> Option<SudoRefusal> {
        self.sudo_policy.refusal(WriteScope::Dotfiles)
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
                warnings.push(crate::package::service::skipped_spec_warning(invalid));
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

fn deploy_state_path<F: FileSystem>(
    filesystem: &F,
    config: &SelfieConfig,
) -> Result<TargetPath, FileSystemError> {
    state_file_path(
        filesystem,
        config.state_directory().map(PathBuf::as_path),
        DEPLOY_STATE_FILENAME,
    )
}

/// Load the deploy state, or an empty one if it cannot be used.
///
/// The second return is a warning naming the file and what went wrong. An absent
/// file is the ordinary first run and warns nothing; a file that cannot be located,
/// read, or parsed each warn differently, because the fixes differ.
///
/// Never fails: callers always proceed with whatever state came back. An empty one
/// costs a round of conflict prompts, where refusing to run would block every apply
/// until someone repairs the file by hand.
fn load_deploy_state<F: FileSystem>(
    filesystem: &F,
    config: &SelfieConfig,
) -> (DeployState, Option<String>) {
    /// What each message says happens next, so the part that differs between them
    /// is the condition and the path rather than the consequence.
    const IGNORED: &str = "continuing as though nothing had been deployed";

    let path = match deploy_state_path(filesystem, config) {
        Ok(p) => p,
        Err(e) => {
            return (
                DeployState::empty(),
                Some(format!(
                    "Cannot locate the deploy state file: {e}; {IGNORED}"
                )),
            );
        }
    };
    if !filesystem.path_exists(path.path()) {
        return (DeployState::empty(), None);
    }
    let content = match filesystem.read_file(path.path()) {
        Ok(content) => content,
        Err(e) => {
            return (
                DeployState::empty(),
                Some(format!(
                    "Cannot read deploy state '{}': {e}; {IGNORED}",
                    path.display()
                )),
            );
        }
    };

    // This message reaches the MCP server's JSON. No credential can be in it --
    // secret-bearing entries record nothing (ADR-0003) -- but the file names every
    // repository-file dotfile on the machine, each with a checksum, which is why
    // `save_deploy_state` writes it owner-only.
    //
    // `ParseFailure` is what keeps the file's text out: serde-saphyr's `Display`
    // interpolates parsed content into several messages, and the duplicate-key one
    // quotes the key, which here is a dotfile source path. It still reports a line
    // and column; the reasoning for accepting that is on the type. Suppressing the
    // snippet keeps the cropped source windows out of the value.
    let options = serde_saphyr::options! { with_snippet: false };
    match serde_saphyr::from_str_with_options(&content, options) {
        Ok(state) => (state, None),
        Err(e) => (
            DeployState::empty(),
            Some(format!(
                "Cannot parse deploy state '{}': {}; {IGNORED}",
                path.display(),
                ParseFailure::of(&e)
            )),
        ),
    }
}

/// Write the deploy state, owner-only where the platform allows it.
///
/// Owner-only because the file names every repository-file dotfile selfie manages
/// here, with checksums — no credentials, but a reconnaissance aid on a shared
/// host. Secret-bearing entries record nothing, so this is not a complete list.
///
/// # Errors
///
/// [`FileSystemError`] if the state path cannot be resolved, the state cannot be
/// serialized, or the write fails.
fn save_deploy_state<F: FileSystem>(
    filesystem: &F,
    config: &SelfieConfig,
    state: &DeployState,
) -> Result<(), FileSystemError> {
    // Deliberately less durable than `write_file_no_follow`: that syncs the file's
    // data before renaming, and the directory fsync missing here is the safe
    // direction. Losing this file costs nothing — the next run re-derives it.
    //
    // Do not "fix" that by syncing harder. A record only lies when it outlives the
    // write it describes, so making the state survive a crash the target write did
    // not would widen that window. The ordering is established at the other end:
    // `write_file_no_follow` is durable before `record_deployment` runs (selfie-aub).
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

impl<R, F, CR, P> DotfileService for DotfileServiceImpl<R, F, CR, P>
where
    R: PackageRepository + Clone + std::fmt::Debug + Send + Sync + 'static,
    F: FileSystem + Clone + std::fmt::Debug + Send + Sync + 'static,
    CR: CommandRunner + Clone + std::fmt::Debug + Send + Sync + 'static,
    P: Privilege + Send + Sync,
{
    async fn apply_all(&self, options: ApplyOptions) -> EventStream {
        let refusal = self.sudo_refusal();
        let collected =
            Self::collect_all_packages(&self.package_repository, self.dotfiles_repository.as_ref());
        let fs = self.filesystem.clone();
        let runner = self.runner.clone();
        let config = self.config.clone();
        let token = self.cancellation_token.clone();

        Self::create_event_stream(move |tx| async move {
            let sender = EventSender::new_with_context(
                tx,
                OperationType::DotfileApply,
                String::new(),
                config.environment().to_string(),
                OperationContext::default(),
            );

            sender.send_started().await;

            let result = match (refusal, collected) {
                (Some(refusal), _) => {
                    OperationResult::Failure(OperationFailure::Privilege(refusal))
                }
                (None, Ok((packages, warnings))) => {
                    for warning in warnings {
                        sender.send_warning(&warning).await;
                    }
                    let ctx = ApplyContext {
                        filesystem: &fs,
                        runner: &runner,
                        config: &config,
                        sender: &sender,
                        options: &options,
                        token: &token,
                    };
                    handle_apply(&packages, &ctx, None).await
                }
                (None, Err(e)) => {
                    OperationResult::Failure(crate::package::event::OperationFailure::Generic(e))
                }
            };

            sender.send_completed(result).await;
        })
    }

    async fn apply(&self, name: &str, options: ApplyOptions) -> EventStream {
        let refusal = self.sudo_refusal();
        let collected =
            Self::collect_all_packages(&self.package_repository, self.dotfiles_repository.as_ref());
        let fs = self.filesystem.clone();
        let runner = self.runner.clone();
        let config = self.config.clone();
        let token = self.cancellation_token.clone();
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

            let result = match (refusal, collected) {
                (Some(refusal), _) => {
                    OperationResult::Failure(OperationFailure::Privilege(refusal))
                }
                (None, Ok((packages, warnings))) => {
                    for warning in warnings {
                        sender.send_warning(&warning).await;
                    }
                    let ctx = ApplyContext {
                        filesystem: &fs,
                        runner: &runner,
                        config: &config,
                        sender: &sender,
                        options: &options,
                        token: &token,
                    };
                    handle_apply(&packages, &ctx, Some(&name)).await
                }
                (None, Err(e)) => {
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
        let refusal = self.sudo_refusal();
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

            let result = match refusal {
                Some(refusal) => OperationResult::Failure(OperationFailure::Privilege(refusal)),
                None => {
                    handle_track_standalone(
                        &name,
                        &target_path,
                        dotfiles_repo.as_ref(),
                        &fs,
                        &config,
                        &sender,
                    )
                    .await
                }
            };

            sender.send_completed(result).await;
        })
    }

    async fn track_for_package(&self, package_name: &str, target_path: &str) -> EventStream {
        let refusal = self.sudo_refusal();
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

            let result = match refusal {
                Some(refusal) => OperationResult::Failure(OperationFailure::Privilege(refusal)),
                None => {
                    handle_track_for_package(
                        &package_name,
                        &target_path,
                        &repo,
                        &fs,
                        &config,
                        &sender,
                    )
                    .await
                }
            };

            sender.send_completed(result).await;
        })
    }
}

/// Identify a secret-bearing entry by what produces it, never by its content.
///
/// Commands and var names come from the package file and are references, not
/// credentials, so they are safe to surface. Used as the `source` of the events
/// this path emits; the wording lives on [`ContentSource`] so apply, `dotfiles
/// list` and the MCP server cannot describe the same entry differently.
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
    // Absolute, checked below. Unresolved is the type's job, not a caller's.
    path: TargetPath,
}

/// Deploying the secret-bearing entries of one package.
///
/// Resolved content stays in memory: compared against the target directly, written
/// owner-only, never recorded in deploy state, never put in an event.
// Exists so the phases below can be separate methods. Each wants most of this
// context, and as free functions they carried six or seven parameters apiece.
struct SecretApply<'a, F, CR> {
    /// The package file's directory. Repository sources resolve against it and
    /// provider commands run in it.
    base_dir: &'a Path,
    filesystem: &'a F,
    runner: &'a CR,
    config: &'a SelfieConfig,
    sender: &'a EventSender,
    options: &'a ApplyOptions,
    /// The caller's live cancellation token, so Ctrl+C reaches a provider command
    /// that is blocked on a biometric or password prompt. Never a fresh token:
    /// `command_timeout` would then be the only way out of an interactive prompt.
    token: &'a CancellationToken,
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

        // Same guard the repository-file path applies, in the same position:
        // before anything reads the target. `read_target` below opens it, and a
        // fifo blocks that open indefinitely.
        //
        // `Failed` rather than `Skipped`, for the reason given above: this is
        // decided from the target alone before anything runs, and a refused entry
        // is not a skipped one.
        if let Some(refusal) = self.filesystem.irregular_target_refusal(&path) {
            self.sender
                .send_warning(refusal_warning(entry.target(), &refusal))
                .await;
            return Err(SecretOutcome::Failed);
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

    /// What is at the target: absent, readable, or present but unreadable.
    ///
    /// Conflating any two of those loses a credential. Read as raw bytes because
    /// neither side is guaranteed to be UTF-8, and a lossy decode would report
    /// two different files as identical.
    fn read_target(&self, target: &SecretTarget<'_>) -> TargetState {
        if !self.filesystem.path_exists(target.path.path()) {
            return TargetState::Absent;
        }

        match self.filesystem.read_file_bytes(target.path.path()) {
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

/// Why an in-sync entry will never settle, when that is the case.
///
/// `Some` for an untracked target whose contents already match but which is a
/// symlink: apply skips it and records nothing, so drift reports it on every run
/// forever. Call it from both apply and drift so their wording cannot diverge.
// Scoped to `NotTracked` deliberately. A *tracked* entry whose target later became
// a symlink produces no drift line at all — a different bug — and answering for it
// here would half-fix that one from the wrong place (selfie-v7py).
fn unmanaged_symlink_reason<F: FileSystem>(
    filesystem: &F,
    drift: &DriftType,
    decision: &DeployDecision,
    target: &TargetPath,
) -> Option<&'static str> {
    (*drift == DriftType::NotTracked
        && matches!(decision, DeployDecision::Skip(_))
        && filesystem.symlink_refusal(target).is_some())
    .then_some(
        "the target is a symlink, so selfie will not manage it \
         and records no deployment for it",
    )
}

// Every site that words a refused deploy shares this, so apply, drift and the
// writer cannot describe the same refusal differently. Format it here rather than
// at a call site — no test pins this wrapper at the write site, so a copy there
// could drift unnoticed.
//
// Named as a property rather than counted. The count was "three", and was correct
// until the same change that wrote it added three more call sites — a number in a
// comment is a claim that goes stale on the next edit, in a file whose whole
// subject is claims going stale.
fn refusal_warning(source: &str, refusal: &FileSystemError) -> String {
    format!("Skipping '{source}': {refusal}")
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

// Why selfie will not read a file out of its own repository.
//
// Reading a fifo blocks until a writer arrives, so one committed into the
// dotfiles directory hangs `selfie apply` and `dotfiles drift` with no timeout --
// `command_timeout` governs provider commands, not filesystem calls (selfie-lwv5).
//
// Returns the reason only; the three read sites frame it differently.
//
// Worded for a *source*. `IrregularTarget`'s own `Display` describes a deploy
// target, and here the problem is a file in the repository the user syncs.
pub(crate) fn repository_read_refusal(refusal: &FileSystemError) -> String {
    match refusal {
        FileSystemError::IrregularTarget { kind, .. } => {
            format!("the repository file is a {kind} and selfie will not read it")
        }
        // Fails **closed**, and deliberately not a `_ => {}` that would skip the
        // guard. `irregular_target_refusal` returns only `IrregularTarget` today,
        // so nothing reaches this arm; a wildcard would silently let a future
        // variant through and un-guard the read, which is the failure this whole
        // guard exists to prevent. Refuse on anything it reports.
        other => format!("selfie will not read the repository file: {other}"),
    }
}

// The three deploy-side sites that refuse a target by the rule: apply's
// secret-bearing path, apply's repository-file path, and drift. `TargetRejection`
// supplies the words so all three say the same thing; this supplies the frame.
fn target_refusal(target: &str, rejection: TargetRejection) -> String {
    format!("Skipping '{target}': {}", rejection.message())
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

/// Describes a single config file deployment operation
struct DeployUnit<'a> {
    source_path: &'a Path,
    target_path: &'a TargetPath,
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
    // whoever planted it.
    if let Err(e) =
        filesystem.write_file_no_follow(unit.target_path, unit.source_content.as_bytes())
    {
        // A refusal is not a failure. "Failed to write" would read as something
        // going wrong rather than as selfie declining, and the error already names
        // both the target and where the link points.
        //
        // Reaching the refusal arm here means the link appeared between the check
        // in `handle_apply` and this write. It is exercised by
        // `the_writer_refuses_even_when_the_check_is_blinded`, which asserts only
        // that the message names a symlink — not the `Skipping '{source}': `
        // wrapper. Share `refusal_warning` rather than repeating the wording, or
        // that unpinned half can drift.
        let message = match &e {
            FileSystemError::SymlinkedTarget { .. } => refusal_warning(unit.source_key, &e),
            _ => format!("Failed to write '{}': {e}", unit.target_path.display()),
        };
        sender.send_warning(message).await;
        // `Err` has the caller count this as refused and leaves the deploy state
        // untouched, so nothing is recorded as deployed that was not. An entry
        // already in the state keeps its previous checksums and is stale rather than
        // untracked, which is the honest record: for a refusal nothing was written,
        // and for an IO failure the target may have been truncated or partly written
        // first, since this writer truncates in place unlike `write_file_private`.
        // Recording either as a fresh deployment is what would make a later drift
        // check call the damage clean.
        return Err(());
    }
    deploy_state.record_deployment(unit.source_key, unit.source_checksum);

    sender
        .send_dotfile_deployed(unit.source_path.display(), unit.target_path.display())
        .await;
    Ok(())
}

/// Everything an apply needs that does not vary from package to package.
///
/// Grouped because they travel together: `handle_apply` needs all six, and
/// builds a [`SecretApply`] from them once per package. Passing them
/// individually put the argument count over clippy's limit once the cancellation
/// token joined them.
#[derive(Clone, Copy)]
struct ApplyContext<'a, F, CR> {
    filesystem: &'a F,
    runner: &'a CR,
    config: &'a SelfieConfig,
    sender: &'a EventSender,
    options: &'a ApplyOptions,
    /// The caller's live token. See [`SecretApply::token`].
    token: &'a CancellationToken,
}

/// Core logic for applying config files
async fn handle_apply<F, CR>(
    packages: &[Package],
    ctx: &ApplyContext<'_, F, CR>,
    filter_name: Option<&str>,
) -> OperationResult
where
    F: FileSystem,
    CR: CommandRunner,
{
    let ApplyContext {
        filesystem,
        runner,
        config,
        sender,
        options,
        token,
    } = *ctx;

    let (mut deploy_state, state_warning) = load_deploy_state(filesystem, config);
    if let Some(warning) = state_warning {
        sender.send_warning(&warning).await;
    }

    let mut deployed_count: usize = 0;
    let mut skipped_count: usize = 0;
    let mut conflict_count: usize = 0;
    // Entries this run was asked to deploy and did not. Kept apart from
    // `skipped_count` because a caller cannot act on a number that means both
    // "nothing to do" and "selfie declined": that conflation is what let `selfie
    // apply` exit 0 having deployed nothing (selfie-c28).
    //
    // The split is the one the secret-bearing path already draws between
    // `SecretOutcome::Failed` and `SecretOutcome::Skipped` — see
    // `SecretApply::usable_target`, whose "a refused entry is not a skipped one"
    // never reached the repository-file path until now.
    let mut refused_count: usize = 0;

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

        // Refuse the whole package before asking what dotfiles it has. A
        // `configs:` or a `_dotfiles:` anchor leaves the list selfie read empty
        // or short, so the `is_empty` check below would skip the package in
        // silence (selfie-g199, selfie-jt6m). The set is the one `spec validate`
        // errors on, so the two commands answer alike.
        //
        // Whole-package rather than per-entry: the problem is in the file's top
        // level, so there is no entry to attach it to. The keys arrive worded for
        // that level, so this cannot explain them against another. A file that
        // could not be read back is decided below instead.
        if let TopLevelKeys::Checked(keys) = package.top_level_keys()
            && !keys.is_empty()
        {
            let described: Vec<&str> = keys.iter().map(|key| key.message.as_str()).collect();
            sender
                .send_warning(format!(
                    "Skipping package '{}': {}",
                    package.name(),
                    described.join("; ")
                ))
                .await;
            refused_count += 1;
            continue;
        }

        // The same refusal for the environment about to be applied. An unknown
        // key here is not merely ignored: `_dotfiles:` leaves this environment's
        // list empty, so `dotfiles_for_environment` falls back to the shared
        // entry and deploys a file this machine was meant to override.
        //
        // Scoped to the active environment, because a typo in an environment
        // this run does not touch cannot affect what it deploys.
        if let Some(env) = package.environments().get(config.environment()) {
            let unknown = env.unknown_keys();
            if !unknown.is_empty() {
                let described: Vec<String> = unknown
                    .iter()
                    .map(|key| describe_unknown_key_in::<EnvironmentField>(key))
                    .collect();
                sender
                    .send_warning(format!(
                        "Skipping package '{}': in environment '{}': {}",
                        package.name(),
                        config.environment(),
                        described.join("; ")
                    ))
                    .await;
                refused_count += 1;
                continue;
            }
        }

        let dotfiles = package.dotfiles_for_environment(config.environment());

        // Waits for `dotfiles` because what an unread top level costs depends on
        // what is left to deploy.
        //
        // With nothing to deploy, a package that has none by design and one whose
        // entries are hidden behind an unchecked key are indistinguishable, and
        // the `continue` below reports both as nothing to do (selfie-c28).
        //
        // With entries, what deploys is what selfie's own parse produced -- not
        // necessarily the right content: a hidden `environments:` key costs the
        // mapping, so a shared entry can land on its override's target (selfie-flsi).
        if let TopLevelKeys::Unchecked(error) = package.top_level_keys() {
            if dotfiles.is_empty() {
                sender
                    .send_warning(format!(
                        "Skipping package '{}': it has no dotfiles to deploy, and its top-level \
                         keys could not be checked, so a shadowed 'dotfiles:' key cannot be ruled \
                         out. The re-read failed with: {error}",
                        package.name()
                    ))
                    .await;
                refused_count += 1;
                continue;
            }

            // The parse failure ends both messages because it is several lines of
            // source snippet, and anything after it reads as part of it.
            sender
                .send_warning(format!(
                    "Package '{}': could not re-read the package file to check its top-level \
                     keys, so an unrecognized one -- a misspelling, or an anchor named after a \
                     real field -- would not have been caught. Applying it anyway. The re-read \
                     failed with: {error}",
                    package.name()
                ))
                .await;
        }

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
            token,
        };

        for entry in &dotfiles {
            // Between entries: refuse to start another entry's commands once the
            // user has asked to stop. The *mid-command* case cannot be caught
            // here — it surfaces as a resolve failure and is handled in the
            // `Failed` arm below.
            if token.is_cancelled() {
                stopped = Some(APPLY_CANCELLED.to_string());
                break 'packages;
            }

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
                            refused_count += 1;
                            // Cancellation is decided before `stop_on_error` gets
                            // to explain the failure, and outside its branch,
                            // because a cancelled run stops either way.
                            //
                            // Ctrl+C kills the provider command, which fails, and
                            // `stop_on_error` defaults to true — so without this
                            // the run blames the package file for the user's own
                            // interrupt ("Stopped after failing to apply dotfile
                            // 'X' (stop_on_error is enabled)"). That reads as a
                            // spec bug and sends the user looking for one.
                            if token.is_cancelled() {
                                stopped = Some(APPLY_CANCELLED.to_string());
                                break 'packages;
                            }
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
                    refused_count += 1;
                    continue;
                }
            };

            let source_path = resolve_source_path(&base_dir, source);

            // Lexical: catches a written `..`, not a planted symlink. See
            // `crate::paths::is_within`.
            if !is_within(&source_path, &base_dir) {
                sender
                    .send_warning(format!(
                        "Skipping '{source}': source path escapes YAML base directory"
                    ))
                    .await;
                refused_count += 1;
                continue;
            }

            // The one target rule. A relative target would write relative to CWD,
            // which is surprising and potentially dangerous; a `~user/…` one names
            // a home directory selfie does not resolve.
            //
            // Still a skip rather than a failure, unlike the secret-bearing path
            // above: every repository-file refusal in this loop continues, and
            // `stop_on_error` governs secret-resolution failures only (see the
            // comment on `stopped`). Changing that is a behavior change beyond
            // this rule.
            let target_path = match deploy_target(filesystem, entry.target()) {
                Ok(path) => path,
                Err(rejection) => {
                    sender
                        .send_warning(target_refusal(entry.target(), rejection))
                        .await;
                    refused_count += 1;
                    continue;
                }
            };

            // Ahead of every read of the target below, not merely ahead of the
            // write. Reading a fifo blocks until a writer opens it, exactly as
            // writing one blocks until a reader does, so the checksum read further
            // down hangs `selfie apply` before the write is ever reached — and a
            // character device would be read from, then written to. Placing this
            // beside the symlink check instead would leave the hang in place.
            if let Some(refusal) = filesystem.irregular_target_refusal(&target_path) {
                sender.send_warning(refusal_warning(source, &refusal)).await;
                refused_count += 1;
                continue;
            }

            // Immediately ahead of the read, which is what this guards: a fifo
            // source blocks `read_file` until a writer arrives and hangs apply.
            // Anchored to the read rather than to the containment check above,
            // because drift runs those two in the opposite order (selfie-tl1w)
            // and anchoring to `is_within` would put this guard on a different
            // side of the target rule in the two commands.
            if let Some(refusal) =
                filesystem.irregular_target_refusal(&repository_path(&source_path))
            {
                sender
                    .send_warning(format!(
                        "Skipping '{source}': {}. Replace it with a regular file.",
                        repository_read_refusal(&refusal)
                    ))
                    .await;
                refused_count += 1;
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
                    refused_count += 1;
                    continue;
                }
            };

            let source_checksum = compute_checksum(source_content.as_bytes());
            let target_exists = filesystem.path_exists(target_path.path());

            // Read target if exists
            let target_checksum = if target_exists {
                match filesystem.read_file(target_path.path()) {
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
            // dry run previews what a real apply would do, an interactive resolver
            // is never asked a question whose answer cannot be honored, and the
            // link destination is never rendered in a diff.
            //
            // `Skip` is excluded: an in-sync target is not written to. Recording one
            // as deployed would let `detect_drift` answer `None` forever for a path
            // selfie will never write (selfie-phnh), so the suppression below covers
            // only the symlinked case. `write_file_no_follow` holds the TOCTOU half.
            if !matches!(decision, DeployDecision::Skip(_))
                && let Some(refusal) = filesystem.symlink_refusal(&target_path)
            {
                sender.send_warning(refusal_warning(source, &refusal)).await;
                refused_count += 1;
                continue;
            }

            // Computed before the match, which consumes `decision`. `None` for
            // every branch but `Skip`, so only that one reads it.
            let unmanaged = unmanaged_symlink_reason(filesystem, &drift, &decision, &target_path);

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
                        // A refusal or a write failure. `perform_deploy` has
                        // already said which in a warning; here they are the same
                        // thing — asked to deploy, did not.
                        refused_count += 1;
                    }
                }
                DeployDecision::Skip(reason) => {
                    // Record an untracked but in-sync entry so future runs see
                    // `DriftType::None`, unless the target is a symlink.
                    //
                    // Gating this on `symlink_refusal` is safe despite its
                    // advisory-and-racy documentation, because nothing is
                    // written here. Where this path does write, `perform_deploy`
                    // relies on `write_file_no_follow`'s kernel refusal.
                    //
                    // A stale answer omits an entry the next run re-evaluates.
                    // The window that could manufacture one is small, not absent.
                    if drift == DriftType::NotTracked && !options.dry_run && unmanaged.is_none() {
                        deploy_state.record_deployment(source, &source_checksum);
                    }

                    // Say why it will not settle, on the line the user is already
                    // reading. Not a warning: nothing was written and nothing was
                    // refused, and raising one here would break the control whose
                    // whole value is that an in-sync symlinked target is left in
                    // silence.
                    let reason = match unmanaged {
                        Some(why) => format!("{reason}; {why}"),
                        None => reason,
                    };
                    sender
                        .send_dotfile_skipped(source_path.display(), target_path.display(), &reason)
                        .await;
                    skipped_count += 1;
                }
                DeployDecision::Conflict => {
                    // Build the diff for display/resolution (needed by both
                    // the resolver and the fallback conflict event).
                    let target_content =
                        filesystem.read_file(target_path.path()).unwrap_or_default();
                    let diff = unified_diff(
                        &target_content,
                        &source_content,
                        &target_path.display().to_string(),
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
                            // The second of `perform_deploy`'s two failure sites,
                            // easy to miss because the first one looks the same.
                            // A conflict the user accepted and selfie then could
                            // not write is a refusal exactly like the plain one.
                            refused_count += 1;
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

    // Cancellation arriving once the *last* entry has started is seen by nothing
    // above: the loop's guard sits at the top of each entry, so when there is no
    // next entry it never runs again, and a command that finishes despite the
    // cancellation leaves `stopped` as `None`. The run would then report success
    // for a run the user interrupted — and for a provider entry that means a
    // credential written to disk after Ctrl+C, with nothing in the stream saying
    // so. Deploy state is still saved below, because the writes really happened.
    //
    // Does not overwrite an existing reason: `stop_on_error` names the entry that
    // failed, which is more specific than this.
    if stopped.is_none() && token.is_cancelled() {
        stopped = Some(APPLY_CANCELLED.to_string());
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

    // `refused_count` belongs in the total: leaving it out would shrink the step
    // count by exactly the number of refusals, so a run that refused two of three
    // entries would report (1/1) and the two refusals would vanish from the
    // summary as well as from the counters.
    //
    // That makes this "outcomes recorded" rather than "entries seen": a package
    // refused whole for a top-level unknown key contributes one outcome and no
    // entries.
    let total = deployed_count + skipped_count + conflict_count + refused_count;
    OperationResult::Success(OperationSuccess::DotfilesApplied {
        deployed_count,
        skipped_count,
        conflict_count,
        refused_count,
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
    let (deploy_state, state_warning) = load_deploy_state(filesystem, config);
    if let Some(warning) = state_warning {
        sender.send_warning(&warning).await;
    }

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

                // Secret-bearing entries hold no deploy state, so there is
                // nothing to compare against, and resolving them here would run
                // the user's commands: leaking content into a read-only
                // operation and prompting for authentication.
                //
                // Reported as unverifiable rather than counted as drift.
                // Counting them would leave `dotfiles drift` permanently dirty
                // on any machine with one provider-sourced dotfile (ADR-0003).
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

            // The same rule apply applies, worded the same way through
            // `target_refusal` -- a drift check that described an undeployable
            // entry differently would send the user looking for a different
            // problem from the one apply reports.
            let target_path = match deploy_target(filesystem, entry.target()) {
                Ok(path) => path,
                Err(rejection) => {
                    sender
                        .send_warning(target_refusal(entry.target(), rejection))
                        .await;
                    continue;
                }
            };

            // Drift reads the target to checksum it, so it hangs on a fifo exactly
            // as apply does. Same guard, same position — ahead of the read — and
            // worded identically through `refusal_warning`.
            if let Some(refusal) = filesystem.irregular_target_refusal(&target_path) {
                sender.send_warning(refusal_warning(source, &refusal)).await;
                continue;
            }

            // Same lexical guard as handle_apply — see `crate::paths::is_within`.
            if !is_within(&source_path, &base_dir) {
                sender
                    .send_warning(format!(
                        "Skipping '{source}': source path escapes YAML base directory"
                    ))
                    .await;
                continue;
            }

            // Same guard apply applies, in the same position -- immediately ahead
            // of the source read -- and worded identically. Drift reads the
            // source to checksum it, so it hangs on a fifo there exactly as apply
            // does.
            if let Some(refusal) =
                filesystem.irregular_target_refusal(&repository_path(&source_path))
            {
                sender
                    .send_warning(format!(
                        "Skipping '{source}': {}. Replace it with a regular file.",
                        repository_read_refusal(&refusal)
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

            // `read_file` follows a final-component symlink, so a symlinked target
            // is checksummed by its destination. Following the link is deliberate:
            // not following would change the drift type, and with it the counts
            // `sync status` reads.
            let target_exists = filesystem.path_exists(target_path.path());
            let target_checksum = if target_exists {
                match filesystem.read_file(target_path.path()) {
                    Ok(content) => compute_checksum(content.as_bytes()),
                    Err(_) => String::new(),
                }
            } else {
                String::new()
            };

            let drift = deploy_state.detect_drift(source, &source_checksum, &target_checksum);
            let decision =
                deploy_decision(&drift, target_exists, &source_checksum, &target_checksum);

            if drift != DriftType::None {
                // The reason rides on the drift line rather than on a warning,
                // because this is the line the user is already looking at and the
                // one that keeps coming back. Apply says the same sentence on its
                // skip line; both read `unmanaged_symlink_reason`.
                sender
                    .send_dotfile_drift_detected(
                        target_path.display(),
                        &drift,
                        unmanaged_symlink_reason(filesystem, &drift, &decision, &target_path),
                    )
                    .await;
                drift_count += 1;
            }

            // Gate on `deploy_decision`, the function apply calls — not on
            // `drift != None`. An untracked target whose contents already match is
            // `NotTracked`, so the block above reports it as drift, but it is `Skip`
            // and apply is silent for it; gating on the drift type would warn
            // exactly where apply says nothing. The drift event keeps its own gate
            // on purpose: only the refusal follows apply's decision.
            if !matches!(decision, DeployDecision::Skip(_))
                && let Some(refusal) = filesystem.symlink_refusal(&target_path)
            {
                sender.send_warning(refusal_warning(source, &refusal)).await;
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
async fn handle_track_standalone<R, F>(
    name: &str,
    target_path: &str,
    dotfiles_repo: Option<&R>,
    filesystem: &F,
    config: &SelfieConfig,
    sender: &EventSender,
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

    // Expand the target, or refuse it if selfie could never deploy to it.
    //
    // First of the three refusals, and ahead of `symlink_refusal` for a reason of
    // its own: this one touches no filesystem at all, while `symlink_refusal` and
    // `path_exists` both stat a relative path against the *process working
    // directory* -- which is what made track record entries every later apply
    // refuses (selfie-q9t3). It therefore also sits ahead of all three writes.
    let expanded_target = match deploy_target(filesystem, target_path) {
        Ok(path) => path,
        Err(rejection) => {
            return OperationResult::Failure(OperationFailure::Generic(track_target_refusal(
                target_path,
                rejection,
            )));
        }
    };

    // Position is load-bearing at both ends. Before the writes: tracking reads
    // *through* a link, so accepting one copies the destination into the dotfiles
    // directory — where `sync push` commits it — and records a deployment that never
    // happened. Before the existence check: `path_exists` follows the link, so a
    // dangling one would be reported as a missing file.
    if let Some(refusal) = filesystem.symlink_refusal(&expanded_target) {
        return OperationResult::Failure(OperationFailure::Generic(track_refusal(&refusal)));
    }

    // Also ahead of the read: tracking copies the target into the dotfiles
    // repository, and reading a fifo blocks until a writer arrives. There is
    // nothing to track in a fifo or a device node in any case.
    //
    // Deliberately not `track_refusal`, which the symlink case above uses: that
    // one appends "replace the symlink with a regular file, or track the path it
    // points to", and neither half applies here -- a fifo points at nothing, and
    // "replace it with a regular file" describes deleting the user's pipe. The
    // remedy that does apply is naming a different target, so this says that.
    if let Some(refusal) = filesystem.irregular_target_refusal(&expanded_target) {
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "{refusal}. Point the entry at a regular file instead."
        )));
    }

    if !filesystem.path_exists(expanded_target.path()) {
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "Target file does not exist: {}",
            expanded_target.display()
        )));
    }

    // Read the target file content
    let content = match filesystem.read_file(expanded_target.path()) {
        Ok(c) => c,
        Err(e) => {
            return OperationResult::Failure(OperationFailure::Generic(format!(
                "Cannot read target file: {e}"
            )));
        }
    };

    // Determine source filename (just the basename of the target)
    let filename = expanded_target
        .path()
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

    if let Err(e) =
        filesystem.write_file_no_follow(&repository_path(&source_path), content.as_bytes())
    {
        return OperationResult::Failure(OperationFailure::Generic(repository_write_refusal(
            &source_path,
            &e,
        )));
    }

    // Create the YAML spec (spec_path already computed above for overwrite check)
    let recorded_target = portable_target(filesystem, target_path);
    let package = crate::package::PackageBuilder::default()
        .name(name)
        .dotfiles(vec![DotfileEntry::new(
            format!("{name}/{filename}"),
            &recorded_target,
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
    let (mut deploy_state, state_warning) = load_deploy_state(filesystem, config);
    if let Some(warning) = state_warning {
        sender.send_warning(&warning).await;
    }
    let source_key = format!("{name}/{filename}");
    deploy_state.record_deployment(&source_key, &checksum);
    if let Err(e) = save_deploy_state(filesystem, config, &deploy_state) {
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "Cannot save deploy state: {e}"
        )));
    }

    // The recorded form, not the argument: an adapter that echoed the caller's
    // path would name a target the spec does not contain.
    OperationResult::Success(OperationSuccess::DotfileTracked {
        name: name.to_string(),
        source_path,
        target_path: recorded_target,
        was_already_tracked: false,
        environment: config.environment().to_string(),
        steps_completed: StepCount::new(1, 1),
    })
}

/// Handle `track_for_package`: load an existing package, copy the target file
/// alongside the YAML, add a dotfiles entry, save, and record deploy state.
async fn handle_track_for_package<R, F>(
    package_name: &str,
    target_path: &str,
    repo: &R,
    filesystem: &F,
    config: &SelfieConfig,
    sender: &EventSender,
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

    // Same rule and same wording as `handle_track_standalone`, and ahead of the
    // already-tracked lookup below rather than after it: an entry recording a
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

    // Check if this target is already tracked in the package. Each entry's own
    // target goes through `expand_target_path`, not the rule: this compares a
    // recorded entry rather than writing to it, and a spec may hold one the rule
    // refuses.
    let already_tracked = package_blob
        .package()
        .dotfiles()
        .iter()
        .find(|entry| expand_target_path(filesystem, entry.target()) == expanded_target);

    if let Some(entry) = already_tracked {
        // The entry's own target, not the argument: "already tracking X" should
        // name what the spec says, which is what a later apply will use.
        return OperationResult::Success(OperationSuccess::DotfileTracked {
            name: package_name.to_string(),
            source_path: expanded_target.path().to_path_buf(),
            target_path: entry.target().to_string(),
            was_already_tracked: true,
            environment: config.environment().to_string(),
            steps_completed: StepCount::new(1, 1),
        });
    }

    // Same ordering constraint as `handle_track_standalone`, and it applies to
    // this guard only. The target-rule check above deliberately sits *before* the
    // already-tracked short-circuit, because an entry recording a target that can
    // never deploy must not be reported as tracked. This one sits after it,
    // because refusing an idempotent no-op helps nobody.
    if let Some(refusal) = filesystem.symlink_refusal(&expanded_target) {
        return OperationResult::Failure(OperationFailure::Generic(track_refusal(&refusal)));
    }

    // Also ahead of the read: tracking copies the target into the dotfiles
    // repository, and reading a fifo blocks until a writer arrives. There is
    // nothing to track in a fifo or a device node in any case.
    //
    // Deliberately not `track_refusal`, which the symlink case above uses: that
    // one appends "replace the symlink with a regular file, or track the path it
    // points to", and neither half applies here -- a fifo points at nothing, and
    // "replace it with a regular file" describes deleting the user's pipe. The
    // remedy that does apply is naming a different target, so this says that.
    if let Some(refusal) = filesystem.irregular_target_refusal(&expanded_target) {
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "{refusal}. Point the entry at a regular file instead."
        )));
    }

    // Validate the target path
    if !filesystem.path_exists(expanded_target.path()) {
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "Target file does not exist: {}",
            expanded_target.display()
        )));
    }

    // Read the target file content
    let content = match filesystem.read_file(expanded_target.path()) {
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
        .path()
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
    if let Err(e) =
        filesystem.write_file_no_follow(&repository_path(&source_path), content.as_bytes())
    {
        return OperationResult::Failure(OperationFailure::Generic(repository_write_refusal(
            &source_path,
            &e,
        )));
    }

    // Add dotfiles entry and save — source is relative to the YAML's parent dir
    let recorded_target = portable_target(filesystem, target_path);
    package_blob
        .package_mut()
        .add_dotfile(DotfileEntry::new(&relative_source, &recorded_target));

    let file_path = package_blob.file_path().to_path_buf();
    if let Err(e) = repo.save_package(package_blob.package(), &file_path) {
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "Cannot save updated package: {e}"
        )));
    }

    // Record initial deploy state
    let checksum = compute_checksum(content.as_bytes());
    let (mut deploy_state, state_warning) = load_deploy_state(filesystem, config);
    if let Some(warning) = state_warning {
        sender.send_warning(&warning).await;
    }
    deploy_state.record_deployment(&relative_source, &checksum);
    if let Err(e) = save_deploy_state(filesystem, config, &deploy_state) {
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "Cannot save deploy state: {e}"
        )));
    }

    OperationResult::Success(OperationSuccess::DotfileTracked {
        name: package_name.to_string(),
        source_path,
        target_path: recorded_target,
        was_already_tracked: false,
        environment: config.environment().to_string(),
        steps_completed: StepCount::new(1, 1),
    })
}

/// What [`load_deploy_state`] reports, and what it stays quiet about.
///
/// At this layer rather than through a service, because the path-resolution branch
/// is unreachable from an integration test: those always configure a
/// `state_directory`, and only an unset one with no determinable home reaches it.
/// The wiring — that these messages actually leave the library as events — is
/// covered per command in `tests/dotfile_service_tests.rs`.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SelfieConfigBuilder;
    use crate::fs::MockFileSystem;

    const STATE_DIR: &str = "/state";
    const STATE_FILE: &str = "/state/deploy-state.yml";

    fn config_with_state_dir() -> SelfieConfig {
        SelfieConfigBuilder::default()
            .environment("test")
            .package_directory("/packages")
            .state_directory(PathBuf::from(STATE_DIR))
            .build()
    }

    // A state file that exists and reads back as `content`.
    fn filesystem_holding(content: &str) -> MockFileSystem {
        let mut fs = MockFileSystem::default();
        fs.mock_path_exists(PathBuf::from(STATE_FILE), true);
        fs.mock_read_file(PathBuf::from(STATE_FILE), content);
        fs
    }

    // Text that must never reach a message. `KEY` is shaped like the dotfile
    // source path a real state file keys on; `VALUE` like a checksum. Both are
    // distinctive enough that a `contains` cannot match selfie's own wording.
    const KEY: &str = "zzz-recon-marker/id_rsa.conf";
    const VALUE: &str = "zzz-value-marker-9c1f";

    // The malformed shapes, built once. Each test below picks the shapes whose
    // error class it is about, so a fixture written slightly differently in two
    // places cannot make two tests disagree about what they cover.
    fn duplicate_key(key: &str) -> String {
        format!(
            "deployed:\n  {key}:\n    source_checksum: a\n    deployed_checksum: a\n    \
             deployed_at: b\n  {key}:\n    source_checksum: c\n    deployed_checksum: c\n    \
             deployed_at: d\n"
        )
    }

    fn entry_is_a_scalar(key: &str, value: &str) -> String {
        format!("deployed:\n  {key}: {value}\n")
    }

    fn entry_is_missing_a_field(key: &str, value: &str) -> String {
        format!("deployed:\n  {key}:\n    source_checksum: {value}\n")
    }

    fn unclosed_bracket(key: &str, value: &str) -> String {
        format!("{key}: [unclosed {value}\n")
    }

    const VALID_STATE_YAML: &str = "deployed:\n  myapp/config.toml:\n    source_checksum: abc\n    \
         deployed_checksum: abc\n    deployed_at: \"2026-01-01T00:00:00+00:00\"\n";

    // The positive control for every test below.
    //
    // Without it they would all pass against a `load_deploy_state` that returned an
    // empty state and a message unconditionally.
    #[test]
    fn a_valid_state_file_loads_its_entries_with_no_message() {
        let fs = filesystem_holding(VALID_STATE_YAML);

        let (state, warning) = load_deploy_state(&fs, &config_with_state_dir());

        assert!(state.get("myapp/config.toml").is_some());
        assert_eq!(warning, None);
    }

    // The first-run case, and the one branch that must stay silent.
    //
    // A warning here would fire on every fresh machine, for the ordinary condition
    // of never having deployed anything.
    #[test]
    fn an_absent_state_file_loads_empty_and_says_nothing() {
        let mut fs = MockFileSystem::default();
        fs.mock_path_exists(PathBuf::from(STATE_FILE), false);

        let (state, warning) = load_deploy_state(&fs, &config_with_state_dir());

        assert!(state.entries().is_empty());
        assert_eq!(warning, None);
    }

    #[test]
    fn an_unparsable_state_file_is_named_in_the_message() {
        let fs = filesystem_holding("{{{{not valid yaml!!! garbage $$$");

        let (state, warning) = load_deploy_state(&fs, &config_with_state_dir());

        assert!(state.entries().is_empty());
        let warning = warning.expect("a corrupt state file must be reported");
        assert!(
            warning.contains(STATE_FILE),
            "the message must name the file: {warning}"
        );
    }

    // The two conditions are distinguished, not merely both reported.
    //
    // Repairing malformed YAML and fixing permissions are different jobs, so one
    // message for both sends the reader to the wrong one. Asserted in a single test
    // because the property is that the two *differ*, which neither alone can see.
    #[test]
    fn an_unreadable_state_file_is_named_differently_from_an_unparsable_one() {
        let mut unreadable = MockFileSystem::default();
        unreadable.mock_path_exists(PathBuf::from(STATE_FILE), true);
        unreadable.expect_read_file().returning(|_| {
            Err(FileSystemError::IoError(std::sync::Arc::new(
                std::io::Error::other("permission denied"),
            )))
        });

        let (state, unreadable_warning) = load_deploy_state(&unreadable, &config_with_state_dir());
        assert!(state.entries().is_empty());
        let unreadable_warning = unreadable_warning.expect("an unreadable state file is reported");

        let corrupt = filesystem_holding("{{{{not valid yaml!!! garbage $$$");
        let (_, corrupt_warning) = load_deploy_state(&corrupt, &config_with_state_dir());
        let corrupt_warning = corrupt_warning.expect("a corrupt state file is reported");

        assert!(
            unreadable_warning.contains("Cannot read"),
            "an I/O failure must say so: {unreadable_warning}"
        );
        assert!(
            corrupt_warning.contains("Cannot parse"),
            "a malformed file must say so: {corrupt_warning}"
        );
        assert_ne!(
            unreadable_warning, corrupt_warning,
            "two different conditions reported identically"
        );
    }

    // Reachable only with no configured `state_directory` and no determinable home.
    //
    // `home()` is not on `FileSystem` — it comes from the blanket `HomeDir` impl,
    // whose body is `expand_path("~")` — so this stubs the method that one calls.
    // `MockFileSystem` has no `expect_home` to stub.
    #[test]
    fn a_state_file_whose_location_cannot_be_resolved_is_reported() {
        let mut fs = MockFileSystem::default();
        fs.expect_expand_path().returning(|_| {
            Err(FileSystemError::IoError(std::sync::Arc::new(
                std::io::Error::other("no home"),
            )))
        });
        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory("/packages")
            .build();

        let (state, warning) = load_deploy_state(&fs, &config);

        assert!(state.entries().is_empty());
        let warning = warning.expect("an unresolvable state path must be reported");
        assert!(
            warning.contains("Cannot locate"),
            "the message must say the file could not be located: {warning}"
        );
    }

    // A duplicated key must not reach the message.
    //
    // serde-saphyr interpolates a duplicated key into that error's own text, and
    // no snippet option governs it, so the classifier has to keep it out.
    //
    // The controls below are part of the assertion: an inverted `contains` passes
    // just as well against an empty message or an unreached branch.
    #[test]
    fn a_duplicate_key_does_not_reach_the_message() {
        let fs = filesystem_holding(&duplicate_key(KEY));

        let (state, warning) = load_deploy_state(&fs, &config_with_state_dir());

        assert!(state.entries().is_empty());
        let warning = warning.expect("a duplicated key must still be reported");
        assert!(
            !warning.contains(KEY),
            "the duplicated key was quoted into the message: {warning}"
        );
        // `contains("line")` would pass on "at line , column " and on "line 0,
        // column 0" -- the sentinel the classifier's own guard exists to drop --
        // so the coordinates are asserted exactly.
        assert!(
            warning.contains(STATE_FILE)
                && warning.contains("Cannot parse")
                && warning.contains("a key is listed twice")
                && warning.contains("at line 6, column 3"),
            "the message must still identify the file, the condition and where: {warning}"
        );
    }

    // The malformed shapes a deploy state file can take, scanned for their own
    // content.
    //
    // The uncovered arms of `ParseFailure::of` are worth naming, since a partial
    // list reads as a complete one: `MergeKeyNotAllowed`, the unbalanced-container
    // group, the two alias groups and `Eof` have no row, and `InvalidScalar` and
    // `IndentationError` cannot fire while every field here is a `String`.
    //
    // Every marker sits on the line its parse fails at, so a returning snippet
    // would quote it and fail the scan below.
    #[test]
    fn no_malformed_state_file_shape_quotes_its_contents() {
        const DUPLICATE: &str = "a key is listed twice";
        const WRONG_SHAPE: &str = "the file has the wrong shape";
        const UNPARSABLE: &str = "the file is not valid YAML";

        let entry = "{source_checksum: x, deployed_checksum: y, deployed_at: z}";
        // YAML escapes, so the key this builds is ordinary UTF-8 text holding
        // U+00FF and U+00FE -- non-ASCII, not raw bytes.
        let non_ascii_key = format!("\"{KEY}\\xff\\xfe\"");

        let shapes: Vec<(&str, String, &str)> = vec![
            ("duplicate key", duplicate_key(KEY), DUPLICATE),
            (
                "duplicate key inside an anchor",
                format!("anchor: &a\n  {KEY}: 1\n  {KEY}: 2\ndeployed: *a\n"),
                DUPLICATE,
            ),
            (
                "duplicate key through a merge key",
                format!("anchor: &a\n  {KEY}: 1\n  {KEY}: 2\ndeployed:\n  <<: *a\n"),
                DUPLICATE,
            ),
            (
                "duplicate key that is itself an alias",
                format!("anchor: &a {KEY}\ndeployed:\n  *a : {entry}\n  *a : {entry}\n"),
                DUPLICATE,
            ),
            (
                "duplicate key holding non-ASCII characters",
                duplicate_key(&non_ascii_key),
                DUPLICATE,
            ),
            (
                "entry is a scalar",
                entry_is_a_scalar(KEY, VALUE),
                WRONG_SHAPE,
            ),
            (
                "field is a sequence",
                format!(
                    "deployed:\n  {KEY}:\n    source_checksum:\n      - {VALUE}\n    \
                     deployed_checksum: a\n    deployed_at: b\n"
                ),
                WRONG_SHAPE,
            ),
            (
                "field is null",
                format!(
                    "deployed:\n  {KEY}:\n    source_checksum: ~\n    deployed_checksum: a\n    \
                     deployed_at: b\n"
                ),
                "a value is empty where text is required",
            ),
            (
                "entry is missing a field",
                entry_is_missing_a_field(KEY, VALUE),
                "an entry is missing the field",
            ),
            (
                "deployed is a scalar",
                format!("deployed: {VALUE}\n"),
                WRONG_SHAPE,
            ),
            ("top level is a scalar", format!("{VALUE}\n"), WRONG_SHAPE),
            ("unclosed bracket", unclosed_bracket(KEY, VALUE), UNPARSABLE),
            (
                "tab indentation",
                format!("deployed:\n\t{KEY}: {VALUE}\n"),
                UNPARSABLE,
            ),
            (
                "unknown anchor",
                format!("deployed: *{KEY}\n"),
                "the file refers to an anchor it never defines",
            ),
            (
                "merge key against a scalar",
                format!("deployed:\n  {KEY}:\n    <<: {VALUE}\n"),
                "a merge key does not refer to a mapping or a list of mappings",
            ),
            (
                "!!binary that is not base64",
                format!(
                    "deployed:\n  {KEY}:\n    source_checksum: !!binary \"@@@@\"\n    \
                     deployed_checksum: a\n    deployed_at: b\n"
                ),
                "a !!binary value is not valid base64",
            ),
            (
                "!!binary that is not text",
                format!(
                    "deployed:\n  {KEY}:\n    source_checksum: !!binary \"//8=\"\n    \
                     deployed_checksum: a\n    deployed_at: b\n"
                ),
                "a !!binary value is not text",
            ),
            (
                "more than one document",
                format!("deployed: {{}}\n---\ndeployed: {VALUE}\n"),
                "the file holds more than one YAML document",
            ),
        ];

        for (name, yaml, condition) in shapes {
            let fs = filesystem_holding(&yaml);

            let (state, warning) = load_deploy_state(&fs, &config_with_state_dir());

            assert!(
                state.entries().is_empty(),
                "{name}: state was not discarded"
            );
            let warning = warning.unwrap_or_else(|| {
                panic!("{name}: stopped being an error, so this row no longer tests anything")
            });
            assert!(
                !warning.contains(KEY) && !warning.contains(VALUE),
                "{name}: the file's contents were quoted into the message: {warning}"
            );
            assert!(
                warning.contains("Cannot parse") && warning.contains(STATE_FILE),
                "{name}: a malformed file must say so, and name itself: {warning}"
            );
            assert!(
                warning.contains(condition),
                "{name}: this row now reports a different condition, so it no \
                 longer covers the class it was added for: {warning}"
            );
        }
    }

    // The classification survives, so the message is worth reading.
    //
    // Three failures a user fixes differently must not render alike. Compared
    // with the location cut off, because the three fixtures fail at three
    // different places: comparing whole messages, every kind could collapse to
    // one string and the differing line numbers would still tell them apart.
    #[test]
    fn a_parse_failure_names_its_condition() {
        let condition = |yaml: &str| {
            let fs = filesystem_holding(yaml);
            let warning = load_deploy_state(&fs, &config_with_state_dir())
                .1
                .expect("a corrupt state file must be reported");
            let at = warning
                .find(" at line ")
                .unwrap_or_else(|| panic!("a parse failure must say where it happened: {warning}"));
            warning[..at].to_string()
        };

        let duplicate = condition(&duplicate_key("a/b.conf"));
        let wrong_shape = condition(&entry_is_a_scalar("a/b.conf", "scalar"));
        let unparsable = condition(&unclosed_bracket("a/b.conf", "x"));

        assert_ne!(duplicate, wrong_shape);
        assert_ne!(wrong_shape, unparsable);
        assert_ne!(duplicate, unparsable);
    }

    // The two classes that forward the library's own `&'static str`.
    //
    // Those strings are the deserializer's vocabulary, not the file's -- the field
    // name comes from this crate's own derive, and "mapping start" names a YAML
    // event. Checked here rather than taken on the type's word, which is the same
    // trust the rest of this work withholds.
    #[test]
    fn no_passed_through_text_carries_input() {
        for (yaml, expected) in [
            (entry_is_a_scalar(KEY, VALUE), "expected mapping start"),
            (entry_is_missing_a_field(KEY, VALUE), "deployed_checksum"),
        ] {
            let fs = filesystem_holding(&yaml);

            let warning = load_deploy_state(&fs, &config_with_state_dir())
                .1
                .expect("a corrupt state file must be reported");

            assert!(
                warning.contains(expected),
                "the library's own text stopped being forwarded, so this test no \
                 longer proves anything about it: {warning}"
            );
            assert!(
                !warning.contains(KEY) && !warning.contains(VALUE),
                "forwarded library text carried the file's content: {warning}"
            );
        }
    }

    // A key whose length selfie does not control must not grow the message.
    //
    // An explicit key (`? <key>`) is not subject to YAML's 1024-byte simple-key
    // limit, so it can be arbitrarily long. Nothing is forwarded, so nothing
    // needs bounding.
    #[test]
    fn a_huge_duplicate_key_does_not_grow_the_message() {
        let key = "k".repeat(2500);
        let fs = filesystem_holding(&format!("? {key}\n: 1\n? {key}\n: 2\n"));

        let (_, warning) = load_deploy_state(&fs, &config_with_state_dir());

        let warning = warning.expect("a duplicated key must be reported");
        // Control: if this fixture stops producing a duplicate-key error, the
        // huge-key path goes untested and everything below still passes.
        assert!(
            warning.contains("a key is listed twice"),
            "this fixture no longer exercises the huge-key path: {warning}"
        );
        // Neither assertion below is redundant, and the numbers are why. The
        // invariant message measures 142 bytes against the 300-byte bound, so a
        // leak of up to ~158 bytes of the key would satisfy the length check
        // alone; the scan is what catches those. The scan in turn only fires on
        // 32 consecutive key bytes, so the length check is what catches a long
        // leak that somehow broke the run up. A fragment shorter than 32 bytes
        // slips both -- the exact residual, and the reason to keep the pair.
        assert!(
            !warning.contains(&"k".repeat(32)),
            "the key reached the message: {warning}"
        );
        assert!(
            warning.len() < 300,
            "the message grew with the file's content: {} bytes",
            warning.len()
        );
    }

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

    // The `other` arm fails closed. Nothing returns a non-`IrregularTarget`
    // variant from `irregular_target_refusal` today, so this is the only thing
    // holding the arm: hand it one directly and the read must still be refused
    // with something a user can read. A `_ => {}` that skipped the guard would
    // return an empty string here.
    #[test]
    fn a_read_refusal_that_is_not_an_irregular_file_still_refuses() {
        let message = repository_read_refusal(&FileSystemError::SymlinkedTarget {
            path: PathBuf::from("/pkgs/myapp/config.toml"),
            points_to: None,
        });
        assert!(!message.is_empty(), "the guard fell through silently");
        assert!(message.contains("repository file"), "got: {message}");
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
