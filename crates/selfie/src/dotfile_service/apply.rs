//! Applying every entry of every selected package.
//!
//! Walks the packages an operation covers and sends each entry down the path its
//! content source calls for. Three things stop the run before the end: the caller
//! cancelling, the deploy state failing to write, and any refusal or failure while
//! `stop_on_error` is on.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::{
    commands::CommandRunner,
    config::SelfieConfig,
    dotfile_service::{
        deploy::{DeployDecision, compute_checksum, deploy_decision, resolve_source_path},
        diff::unified_diff,
        port::{ConflictDetail, ConflictResolution},
        state::{DeployState, DriftType},
    },
    fs::{
        filesystem::{FileSystem, repository_read_refusal},
        target::{deploy_target, repository_path},
    },
    package::{
        ContentSource, Package,
        event::{EventSender, OperationFailure, OperationResult, OperationSuccess, StepCount},
    },
    paths::is_within,
};

use super::deploy_entry::{
    Decided, DeployOutcome, DeployUnit, Recorded, deploy_and_record, record_and_save,
};
use super::port::ApplyOptions;
use super::refusal::{guard_refusal, readable_target, refusal_warning, target_refusal};
use super::secret::{SecretApply, SecretOutcome, programs_of, secret_origin};
use super::state_file::{LoadedState, StateLoad, load_deploy_state, read_only_state_warning};

/// Why an apply stopped before its last entry.
///
/// Each cause is worded once, in `Display`, so no two sites that stop a run can
/// describe the same stop differently.
enum Stop {
    /// The caller cancelled the run.
    Cancelled,
    /// An entry was refused or failed while `stop_on_error` is on. Carries the
    /// entry's target as the package file spells it.
    Entry(String),
    /// A package was refused whole while `stop_on_error` is on. Carries its name.
    Package(String),
    /// The standalone dotfiles directory could not be read while `stop_on_error`
    /// is on.
    UnreadableDotfilesDirectory,
    /// The deploy state could not record a write. Carries the reason, already
    /// worded. Stops the run whatever `stop_on_error` says.
    Unrecorded(String),
}

impl std::fmt::Display for Stop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("Apply cancelled"),
            Self::Entry(target) => write!(
                f,
                "Stopped after failing to apply dotfile '{target}' (stop_on_error is enabled)"
            ),
            Self::Package(name) => write!(
                f,
                "Stopped after refusing package '{name}' (stop_on_error is enabled)"
            ),
            Self::UnreadableDotfilesDirectory => f.write_str(
                "Stopped before applying anything: the standalone dotfiles directory could not \
                 be read (stop_on_error is enabled)",
            ),
            Self::Unrecorded(reason) => f.write_str(reason),
        }
    }
}

/// What an apply did with each entry, counted as it goes.
#[derive(Default)]
struct ApplyTally {
    deployed: usize,
    /// Entries there was correctly nothing to do for, or that a dry run only
    /// previewed.
    skipped: usize,
    conflicts: usize,
    /// Entries, packages and directories this run was asked to deploy and did
    /// not.
    refused: usize,
}

impl ApplyTally {
    /// Count one refusal, and return the stop it calls for, if any: a cancelled
    /// run stops, and otherwise `stop_on_error` decides.
    // Cancellation is asked first. Ctrl+C kills a provider command, which then
    // fails, and blaming `stop_on_error` for that sends the user looking for a
    // problem in the package file that is not there.
    fn refuse(
        &mut self,
        config: &SelfieConfig,
        token: &CancellationToken,
        cause: Stop,
    ) -> Option<Stop> {
        self.refused += 1;
        if token.is_cancelled() {
            Some(Stop::Cancelled)
        } else if config.stop_on_error() {
            Some(cause)
        } else {
            None
        }
    }

    fn into_success(self, environment: &str) -> OperationSuccess {
        // `refused` belongs in the total: leaving it out would shrink the step
        // count by exactly the number of refusals, so a run that refused two of
        // three entries would report (1/1) and the two refusals would vanish from
        // the summary as well as from the counters.
        //
        // That makes this "outcomes recorded" rather than "entries seen": a package
        // refused whole for a top-level unknown key contributes one outcome and no
        // entries.
        let total = self.deployed + self.skipped + self.conflicts + self.refused;
        OperationSuccess::DotfilesApplied {
            deployed_count: self.deployed,
            skipped_count: self.skipped,
            conflict_count: self.conflicts,
            refused_count: self.refused,
            environment: environment.to_string(),
            steps_completed: StepCount::new(total, total),
        }
    }
}

/// Everything an apply needs that does not vary from package to package.
///
/// Grouped because they travel together: `handle_apply` needs all six, and
/// builds a [`SecretApply`] from them once per package. Passing them
/// individually put the argument count over clippy's limit once the cancellation
/// token joined them.
#[derive(Clone, Copy)]
pub(super) struct ApplyContext<'a, F, CR> {
    pub(super) filesystem: &'a F,
    pub(super) runner: &'a CR,
    pub(super) config: &'a SelfieConfig,
    pub(super) sender: &'a EventSender,
    pub(super) options: &'a ApplyOptions,
    /// The caller's live token. See [`SecretApply::token`].
    pub(super) token: &'a CancellationToken,
}

/// Core logic for applying config files
///
/// Applies every package in `packages`. `refused_repository` counts one refusal
/// for a dotfiles repository whose dotfiles were asked for and could not be
/// listed.
pub(super) async fn handle_apply<F, CR>(
    packages: &[Package],
    ctx: &ApplyContext<'_, F, CR>,
    refused_repository: bool,
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

    // A run that writes refuses to start over a state file it could not read:
    // proceeding would deploy files it can never record, and the next run would
    // re-evaluate every one of them as untracked. A dry run writes nothing, so it
    // warns instead and previews against an empty state.
    let mut loaded = match load_deploy_state(filesystem, config) {
        StateLoad::Usable(loaded) => {
            if let Some(warning) = loaded.directory_warning() {
                sender.send_warning(warning.to_string()).await;
            }
            Some(loaded)
        }
        StateLoad::Unusable(failure) if options.dry_run => {
            sender.send_warning(read_only_state_warning(&failure)).await;
            None
        }
        StateLoad::Unusable(failure) => {
            return OperationResult::Failure(OperationFailure::Generic(failure.to_string()));
        }
    };
    // What a dry run over an unusable state file reads drift against. Only a dry
    // run leaves `loaded` as `None`, and a dry run records nothing.
    let empty = DeployState::empty();

    // Owned rather than borrowed from `loaded`, which is mutably borrowed inside
    // the loop. Taken from the loaded state rather than resolved again, so the
    // copies land beside the state file this run is updating.
    //
    // `None` only where the state could not be loaded at all, which is a dry run
    // and nothing else. A dry run over a state file selfie *can* read still has a
    // root here; what keeps it from writing a copy is `perform_deploy` returning
    // on `dry_run` before it reaches one.
    let backups_root: Option<PathBuf> = loaded.as_ref().map(LoadedState::backups_root);
    // Targets this run has settled, and where each one's former content went.
    let mut backed_up: HashMap<String, Option<PathBuf>> = HashMap::new();

    // Refusals are counted apart from skips because a caller cannot act on a
    // number that means both "nothing to do" and "selfie declined": that
    // conflation is what let `selfie apply` exit 0 having deployed nothing
    // (selfie-c28).
    let mut tally = ApplyTally::default();

    // Set when the run stops early. Held rather than returned so every stop
    // reports through the one failure below.
    //
    // Every refusal goes through `ApplyTally::refuse`, which decides whether it
    // stops the run. A failed state record stops the run whatever
    // `stop_on_error` says, because the next entry would fail the same way.
    let mut stopped: Option<Stop> = None;

    // The directory is known unreadable before any package is looked at, so under
    // `stop_on_error` the run stops before it deploys anything: no package is
    // walked once `stopped` is set here.
    if refused_repository {
        stopped = tally.refuse(config, token, Stop::UnreadableDotfilesDirectory);
    }
    let packages = if stopped.is_some() { &[][..] } else { packages };

    // The programs whose commands have failed in this run. A failure is usually
    // shared by that program's later commands: a locked vault or a dismissed
    // biometric prompt fails every later `op read` the same way, each after its own
    // prompt or `command_timeout`. So a later entry running one of these programs
    // is refused without running anything, while other programs' entries and
    // repository files still deploy. A dry run runs no command, so never adds one.
    let mut failed_programs: BTreeSet<String> = BTreeSet::new();

    'packages: for package in packages {
        // Refuse the whole package before asking what dotfiles it has, through
        // the one function that answers whether apply refuses a package at all.
        // A `configs:` or a `_dotfiles:` anchor leaves the list selfie read empty
        // or short, so the `is_empty` check below would pass over the package in
        // silence (selfie-g199, selfie-jt6m).
        //
        // The reason arrives already worded for the level it came from, so this
        // adds only the package it belongs to.
        if let Some(reason) = package.spec_refusal(config.environment()) {
            sender
                .send_warning(format!("Skipping package '{}': {reason}", package.name()))
                .await;
            if let Some(stop) =
                tally.refuse(config, token, Stop::Package(package.name().to_string()))
            {
                stopped = Some(stop);
                break 'packages;
            }
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
            token,
        };

        for entry in &dotfiles {
            // Between entries: refuse to start another entry's commands once the
            // user has asked to stop. The *mid-command* case cannot be caught
            // here: a killed command fails, and `ApplyTally::refuse` reports the
            // cancellation when that failure is counted.
            if token.is_cancelled() {
                stopped = Some(Stop::Cancelled);
                break 'packages;
            }
            // The stop any refusal of this entry calls for, when `stop_on_error`
            // is on.
            let failed = || Stop::Entry(entry.target().to_string());

            let source = match entry.content_source() {
                Ok(ContentSource::RepoFile(source)) => source,

                // Secret-bearing entries resolve their content by running
                // commands, compare it in memory, and record nothing.
                Ok(content @ (ContentSource::Template { .. } | ContentSource::Provider(_))) => {
                    if let Some(program) = programs_of(entry)
                        .into_iter()
                        .find(|program| failed_programs.contains(*program))
                    {
                        sender
                            .send_warning(format!(
                                "Skipping '{}': an earlier `{program}` command failed; no \
                                 command was run",
                                entry.target()
                            ))
                            .await;
                        if let Some(stop) = tally.refuse(config, token, failed()) {
                            stopped = Some(stop);
                            break 'packages;
                        }
                        continue;
                    }
                    match secret_apply.apply(entry, secret_origin(&content)).await {
                        SecretOutcome::Deployed => tally.deployed += 1,
                        SecretOutcome::Skipped => tally.skipped += 1,
                        SecretOutcome::Conflicted => tally.conflicts += 1,
                        outcome @ (SecretOutcome::Failed | SecretOutcome::CommandFailed(_)) => {
                            if let SecretOutcome::CommandFailed(program) = outcome {
                                failed_programs.insert(program);
                            }
                            if let Some(stop) = tally.refuse(config, token, failed()) {
                                stopped = Some(stop);
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
                    if let Some(stop) = tally.refuse(config, token, failed()) {
                        stopped = Some(stop);
                        break 'packages;
                    }
                    continue;
                }
            };

            let source_path = resolve_source_path(&base_dir, source);

            // The one target rule. A relative target would write relative to CWD,
            // which is surprising and potentially dangerous; a `~user/…` one names
            // a home directory selfie does not resolve.
            //
            let target_path = match deploy_target(filesystem, entry.target()) {
                Ok(path) => path,
                Err(rejection) => {
                    sender
                        .send_warning(target_refusal(entry.target(), rejection))
                        .await;
                    if let Some(stop) = tally.refuse(config, token, failed()) {
                        stopped = Some(stop);
                        break 'packages;
                    }
                    continue;
                }
            };

            // After the target rule, as drift and the secret-bearing path ask it,
            // so an entry failing both gets the same first reason from every
            // command. Lexical: catches a written `..`, not a planted symlink. See
            // `crate::paths::is_within`.
            if !is_within(&source_path, &base_dir) {
                sender
                    .send_warning(format!(
                        "Skipping '{source}': source path escapes YAML base directory"
                    ))
                    .await;
                if let Some(stop) = tally.refuse(config, token, failed()) {
                    stopped = Some(stop);
                    break 'packages;
                }
                continue;
            }

            // Ahead of every read of the target below, not merely ahead of the
            // write. Reading a fifo blocks until a writer opens it, and a
            // character device would be read from, then written to. A symlink is
            // refused here whatever it points at and whether or not its content
            // already matches: reading it would checksum the destination, a file
            // selfie was never asked to manage, and a repository-file entry never
            // writes through a link, so there is no outcome the read could change.
            if let Some(refusal) = guard_refusal(filesystem, &target_path) {
                sender.send_warning(refusal_warning(source, &refusal)).await;
                if let Some(stop) = tally.refuse(config, token, failed()) {
                    stopped = Some(stop);
                    break 'packages;
                }
                continue;
            }

            // Immediately ahead of the read, which is what this guards: a fifo
            // source blocks `read_file` until a writer arrives and hangs apply.
            if let Some(refusal) =
                filesystem.irregular_target_refusal(&repository_path(&source_path))
            {
                sender
                    .send_warning(format!(
                        "Skipping '{source}': {}. Replace it with a regular file.",
                        repository_read_refusal(&refusal)
                    ))
                    .await;
                if let Some(stop) = tally.refuse(config, token, failed()) {
                    stopped = Some(stop);
                    break 'packages;
                }
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
                    if let Some(stop) = tally.refuse(config, token, failed()) {
                        stopped = Some(stop);
                        break 'packages;
                    }
                    continue;
                }
            };

            let source_checksum = compute_checksum(source_content.as_bytes());

            // Ahead of the decision, like the fifo refusal, so it holds under
            // `auto_accept`, under an interactive resolver, and in a dry run. The
            // bytes read here are also what the conflict diff shows, so the
            // checksum and the diff cannot disagree about the target.
            let current = match readable_target(filesystem, source, &target_path) {
                Ok(current) => current,
                Err(warning) => {
                    sender.send_warning(warning).await;
                    if let Some(stop) = tally.refuse(config, token, failed()) {
                        stopped = Some(stop);
                        break 'packages;
                    }
                    continue;
                }
            };
            let target_exists = current.is_some();
            let target_checksum = current.as_deref().map(compute_checksum).unwrap_or_default();

            // State is keyed by the expanded target, the one path that has one
            // file and one checksum however many sources name it.
            let target_key = target_path.display().to_string();
            let unit = DeployUnit {
                source_path: &source_path,
                target_path: &target_path,
                target_key: &target_key,
                source_content: &source_content,
                source_checksum: &source_checksum,
                source,
                backups: backups_root.as_deref(),
            };

            let drift = loaded
                .as_ref()
                .map_or(&empty, LoadedState::state)
                .detect_drift(&target_key, &source_checksum, &target_checksum);
            let decision =
                deploy_decision(&drift, target_exists, &source_checksum, &target_checksum);

            // Every arm that writes names what the target held when it decided, and
            // the one write below carries it out, so the two ways to reach a write
            // cannot count or record it differently.
            let write = match decision {
                DeployDecision::Deploy => Some(Decided::Now(current.as_deref())),
                DeployDecision::Skip(reason) => {
                    // Record an untracked but in-sync entry so future runs see
                    // `DriftType::None`. A symlinked target never reaches here: the
                    // guard above refused it before the read.
                    if drift == DriftType::NotTracked
                        && !options.dry_run
                        && let Some(reason) = record_and_save(
                            filesystem,
                            &mut loaded,
                            sender,
                            Recorded::InSync,
                            &unit,
                        )
                        .await
                    {
                        stopped = Some(Stop::Unrecorded(reason));
                        break 'packages;
                    }

                    sender
                        .send_dotfile_skipped(source_path.display(), target_path.display(), &reason)
                        .await;
                    tally.skipped += 1;
                    None
                }
                DeployDecision::Conflict => {
                    // Built only where it is read. It renders two whole files, and
                    // a run that accepts without asking reads it nowhere.
                    //
                    // An absent target decides `Deploy`, so a conflict always has
                    // bytes; the default is never reached. Lossy only for
                    // display: the checksum above compared the raw bytes.
                    let render = || {
                        let target_content =
                            String::from_utf8_lossy(current.as_deref().unwrap_or_default());
                        unified_diff(
                            &target_content,
                            &source_content,
                            &target_path.display().to_string(),
                            &source_path.to_string_lossy(),
                        )
                    };
                    // Rendered at most once. A declined conflict reaches the
                    // resolver branch and then the reported-conflict branch, and
                    // both read the diff.
                    let mut rendered: Option<String> = None;

                    // Determine whether to accept: --yes flag, interactive
                    // resolver, or neither (report the conflict).
                    //
                    // A dry run accepts nothing, whatever else was asked for,
                    // and is asked first for that reason. It writes nothing, so
                    // there is no answer to honor, and an accept would carry the
                    // entry to the write's dry-run preview and report it as
                    // skipped -- leaving the summary at zero conflicts. The preview
                    // someone runs to see what `--yes` would overwrite is the one
                    // place that count has to be right.
                    let mut decided = Decided::Now(current.as_deref());
                    let accept = if options.dry_run {
                        false
                    } else if options.auto_accept {
                        true
                    } else if let Some(resolver) = &options.conflict_resolver {
                        // The prompt waits on the user, so what the decision read
                        // may no longer be at the target when the write comes.
                        decided = Decided::BeforePrompt;
                        let src = source_path.display().to_string();
                        let tgt = target_path.display().to_string();
                        let d = rendered.get_or_insert_with(&render).clone();
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
                        Some(decided)
                    } else {
                        sender
                            .send_dotfile_conflict(
                                source_path.display(),
                                target_path.display(),
                                rendered.get_or_insert_with(&render),
                            )
                            .await;
                        tally.conflicts += 1;
                        None
                    }
                }
            };

            if let Some(decided) = write {
                match deploy_and_record(
                    filesystem,
                    sender,
                    &mut loaded,
                    &unit,
                    decided,
                    options.dry_run,
                    &mut backed_up,
                )
                .await
                {
                    DeployOutcome::Deployed => tally.deployed += 1,
                    DeployOutcome::Previewed => tally.skipped += 1,
                    // A refusal or a write failure, already reported as whichever it
                    // was. Here they are the same thing: asked to deploy, did not.
                    DeployOutcome::Refused => {
                        if let Some(stop) = tally.refuse(config, token, failed()) {
                            stopped = Some(stop);
                            break 'packages;
                        }
                    }
                    DeployOutcome::Unrecorded(reason) => {
                        stopped = Some(Stop::Unrecorded(reason));
                        break 'packages;
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
    // so.
    //
    // Does not overwrite an existing reason: `stop_on_error` names the entry that
    // failed, which is more specific than this.
    if stopped.is_none() && token.is_cancelled() {
        stopped = Some(Stop::Cancelled);
    }

    if let Some(stop) = stopped {
        return OperationResult::Failure(OperationFailure::Generic(stop.to_string()));
    }

    OperationResult::Success(tally.into_success(config.environment()))
}
