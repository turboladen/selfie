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
        deploy::{DeployDecision, compute_checksum, deploy_decision},
        diff::unified_diff,
        port::{ConflictDetail, ConflictResolution},
        state::{DeployState, DriftType},
    },
    fs::filesystem::FileSystem,
    package::{
        Package,
        event::{EventSender, OperationFailure, OperationResult, OperationSuccess, StepCount},
    },
};

use super::classify::{
    Classified, PackageCollisions, Purpose, RepoFile, RepoRead, ResolvedHome, classify_entry,
    read_repo_file,
};
use super::deploy_entry::{
    Decided, DeployOutcome, DeployUnit, Ledger, Recorded, deploy_and_record, record_and_save,
};
use super::orphan::{self, Catalog};
use super::port::ApplyOptions;
use super::secret::{SecretApply, SecretOutcome, programs_of};
use super::state_file::{
    LoadedState, StateLoad, load_deploy_state, read_only_state_warning, save_deploy_state,
};
use super::warning::CollectionRefusal;

/// What an apply covers.
pub(super) enum Scope {
    /// Every package. Carries what collecting them refused, each counted as one
    /// refusal before any package is looked at.
    All(Vec<CollectionRefusal>),
    /// Packages asked for by name, with case folded. Collection's refusals are no
    /// part of it, and a package with nothing to apply here is worth saying so.
    Named(String),
}

/// The reason a cancelled apply gives.
pub(super) const APPLY_CANCELLED: &str = "Apply cancelled";

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
    /// Collecting the packages refused something while `stop_on_error` is on.
    Collection(CollectionRefusal),
    /// The deploy state could not record a write. Carries the reason, already
    /// worded. Stops the run whatever `stop_on_error` says.
    Unrecorded(String),
}

impl std::fmt::Display for Stop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str(APPLY_CANCELLED),
            Self::Entry(target) => write!(
                f,
                "Stopped after failing to apply dotfile '{target}' (stop_on_error is enabled)"
            ),
            Self::Package(name) => write!(
                f,
                "Stopped after refusing package '{name}' (stop_on_error is enabled)"
            ),
            Self::Collection(CollectionRefusal::UnreadableDotfilesDirectory) => f.write_str(
                "Stopped before applying anything: the standalone dotfiles directory could not \
                 be read (stop_on_error is enabled)",
            ),
            Self::Collection(CollectionRefusal::AmbiguousName { name, .. }) => write!(
                f,
                "Stopped before applying anything: several spec files claim the name '{name}' \
                 (stop_on_error is enabled)"
            ),
            Self::Collection(CollectionRefusal::UnloadableSpec(path)) => write!(
                f,
                "Stopped before applying anything: spec '{}' could not be loaded (stop_on_error \
                 is enabled)",
                path.display()
            ),
            Self::Unrecorded(reason) => f.write_str(reason),
        }
    }
}

/// How one entry ended, for the loop to tally once.
enum EntryOutcome {
    Deployed,
    /// In sync, or only previewed by a dry run.
    Skipped,
    Conflicted,
    /// Refused or failed, and already reported as whichever it was.
    Refused,
    /// Written, and the deploy state could not record it. Carries why the run
    /// stops.
    Unrecorded(String),
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
    /// Orphaned targets whose files are still there. Not an outcome of any entry,
    /// so no part of the step count.
    orphaned: usize,
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
            orphan_count: self.orphaned,
            environment: environment.to_string(),
            steps_completed: StepCount::new(total, total),
        }
    }
}

/// Everything an apply needs that does not vary from package to package.
///
/// Grouped because they travel together: `handle_apply` needs all six, and
/// builds a [`SecretApply`] from them once per package.
// Passed individually, these exceed clippy's too_many_arguments limit.
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
/// Applies every package in `packages`, over the scope `scope` says. `None`
/// when the run was cancelled part way, so the caller reports a cancellation
/// rather than a result.
pub(super) async fn handle_apply<F, CR>(
    packages: &[Package],
    ctx: &ApplyContext<'_, F, CR>,
    scope: Scope,
    catalog: Catalog<'_>,
) -> Option<OperationResult>
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
    let mut ledger =
        match load_deploy_state(filesystem, config.state_directory().map(PathBuf::as_path)) {
            StateLoad::Usable(loaded) => {
                if let Some(warning) = loaded.directory_warning() {
                    sender.send_warning(warning.to_string()).await;
                }
                if options.dry_run {
                    Ledger::Preview(Some(loaded))
                } else {
                    Ledger::Record(loaded)
                }
            }
            StateLoad::Unusable(failure) if options.dry_run => {
                sender.send_warning(read_only_state_warning(&failure)).await;
                Ledger::Preview(None)
            }
            StateLoad::Unusable(failure) => {
                return Some(OperationResult::Failure(OperationFailure::Generic(
                    failure.to_string(),
                )));
            }
        };
    // What a dry run over an unusable state file reads drift against.
    let empty = DeployState::empty();

    // Owned rather than borrowed from `ledger`, which is mutably borrowed inside
    // the loop. Taken from the loaded state rather than resolved again, so the
    // copies land beside the state file this run is updating.
    //
    // `None` only where the state could not be loaded at all, which is a dry run
    // and nothing else. A dry run over a state file selfie *can* read still has a
    // root here; what keeps it from writing a copy is its preview ledger, under
    // which `deploy_and_record` never reaches the write.
    let backups_root: Option<PathBuf> = ledger.loaded().map(LoadedState::backups_root);
    // Targets this run has settled, and where each one's former content went.
    let mut backed_up: HashMap<String, Option<PathBuf>> = HashMap::new();

    // Refusals are counted apart from skips because a caller cannot act on a
    // number that means both "nothing to do" and "selfie declined": that
    // conflation is what let `selfie apply` exit 0 having deployed nothing
    // (selfie-c28).
    let mut tally = ApplyTally::default();
    // Whether a record gained its base, for the one save at the end.
    let mut placed = false;

    // Set when the run stops early. Held rather than returned so every stop
    // reports through the one failure below.
    //
    // Every refusal goes through `ApplyTally::refuse`, which decides whether it
    // stops the run. A failed state record stops the run whatever
    // `stop_on_error` says, because the next entry would fail the same way.
    let mut stopped: Option<Stop> = None;

    // Known before any package is looked at, so under `stop_on_error` the run
    // stops before it deploys anything: no package is walked once `stopped` is set
    // here.
    let refusals = match &scope {
        Scope::All(refusals) => refusals.as_slice(),
        Scope::Named(_) => &[],
    };
    for refusal in refusals {
        stopped = tally.refuse(config, token, Stop::Collection(refusal.clone()));
        if stopped.is_some() {
            break;
        }
    }
    let packages = if stopped.is_some() { &[][..] } else { packages };

    // The programs whose commands have failed in this run. A failure is usually
    // shared by that program's later commands: a locked vault or a dismissed
    // biometric prompt fails every later `op read` the same way, each after its own
    // prompt or `command_timeout`. So a later entry running one of these programs
    // is refused without running anything, while other programs' entries and
    // repository files still deploy. A dry run runs no command, so never adds one.
    let mut failed_programs: BTreeSet<String> = BTreeSet::new();

    // Asked once, so every package compares targets against the same home.
    let home = ResolvedHome::of(filesystem);

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

        let dotfiles = package.effective_dotfiles(Some(config.environment()));
        let package_name = package.spec_name();
        let collisions = PackageCollisions::of(package, &home, config.environment());

        if dotfiles.is_empty() {
            // A named package with nothing for this environment would otherwise
            // complete with every count at zero, which reads as "already up to
            // date". The run says so instead, and does not fail: on a machine
            // where the package declares nothing, nothing to apply is the right
            // answer.
            if matches!(scope, Scope::Named(_)) {
                sender
                    .send_warning(format!(
                        "Package '{}' has no dotfiles for environment '{}'; nothing to apply",
                        package.name(),
                        config.environment()
                    ))
                    .await;
            }
            continue;
        }

        // Source paths resolve relative to the YAML file's parent directory,
        // so packages/fnm.yaml with source "fnm/init.fish" → packages/fnm/init.fish
        let base_dir = package
            .path()
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let source_base = super::classify::source_base(config, package);

        let secret_apply = SecretApply {
            base_dir: &base_dir,
            filesystem,
            runner,
            config,
            sender,
            options,
            token,
        };

        for scoped in &dotfiles {
            let entry = scoped.entry;
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

            // Settled once for the entry, and tallied once below, so every way an
            // entry can end reports, counts and stops through one place.
            let outcome = 'entry: {
                let classified = match classify_entry(
                    filesystem,
                    &base_dir,
                    source_base.as_ref(),
                    *scoped,
                    &collisions,
                    Purpose::Deploy,
                ) {
                    Ok(classified) => classified,
                    Err(refused) => {
                        refused.send(sender).await;
                        break 'entry EntryOutcome::Refused;
                    }
                };

                let repo = match classified {
                    Classified::RepoFile(repo) => repo,
                    // Secret-bearing entries resolve their content by running
                    // commands, compare it in memory, and record nothing.
                    Classified::SecretBearing(secret) => {
                        if let Some(program) = programs_of(entry)
                            .into_iter()
                            .find(|program| failed_programs.contains(program))
                        {
                            sender
                                .send_warning(format!(
                                    "Skipping '{}': an earlier `{program}` command failed; no \
                                     command was run",
                                    entry.target()
                                ))
                                .await;
                            break 'entry EntryOutcome::Refused;
                        }
                        break 'entry match secret_apply.apply(&secret).await {
                            SecretOutcome::Deployed => EntryOutcome::Deployed,
                            SecretOutcome::Skipped => EntryOutcome::Skipped,
                            SecretOutcome::Conflicted => EntryOutcome::Conflicted,
                            SecretOutcome::Failed => EntryOutcome::Refused,
                            SecretOutcome::CommandFailed(program) => {
                                failed_programs.insert(program);
                                EntryOutcome::Refused
                            }
                        };
                    }
                };

                let RepoRead {
                    source_content,
                    current,
                } = match read_repo_file(filesystem, &repo) {
                    Ok(read) => read,
                    Err(refused) => {
                        refused.send(sender).await;
                        break 'entry EntryOutcome::Refused;
                    }
                };
                let RepoFile {
                    source,
                    event_source,
                    target: target_path,
                    ..
                } = repo;
                let source_checksum = compute_checksum(source_content.as_bytes());

                let target_exists = current.is_some();
                let target_checksum = current.as_deref().map(compute_checksum).unwrap_or_default();

                // State is keyed by the expanded target, the one path that has one
                // file and one checksum however many sources name it.
                let target_key = target_path.state_key();
                let unit = DeployUnit {
                    event_source: &event_source,
                    target_path: &target_path,
                    target_key: &target_key,
                    source_content: &source_content,
                    source_checksum: &source_checksum,
                    source,
                    package: package_name.as_deref(),
                    backups: backups_root.as_deref(),
                };

                let drift = ledger
                    .loaded()
                    .map_or(&empty, LoadedState::state)
                    .detect_drift(&target_key, &source_checksum, &target_checksum);
                let decision =
                    deploy_decision(&drift, target_exists, &source_checksum, &target_checksum);

                // Every arm that writes names what the target held when it decided, and
                // the one write below carries it out, so the two ways to reach a write
                // cannot count or record it differently.
                let decided = match decision {
                    DeployDecision::Deploy => Decided::Now(current.as_deref()),
                    DeployDecision::Skip(reason) => {
                        // Record an untracked but in-sync entry so future runs see
                        // `DriftType::None`. A symlinked target never reaches here: the
                        // guard above refused it before the read.
                        if drift == DriftType::NotTracked
                            && let Ledger::Record(loaded) = &mut ledger
                            && let Some(reason) =
                                record_and_save(filesystem, loaded, sender, Recorded::InSync, &unit)
                                    .await
                        {
                            break 'entry EntryOutcome::Unrecorded(reason);
                        }
                        // A tracked record that names no base gains one, so an orphan
                        // it later becomes can be shown against it. Only in memory:
                        // the run saves once at its end, and a failure to save it
                        // costs nothing but the location, so it never stops the run.
                        if let crate::package::event::DotfileSource::File {
                            base: Some(base),
                            path,
                            ..
                        } = &event_source
                            && let Ledger::Record(loaded) = &mut ledger
                            && loaded
                                .state()
                                .get(&target_key)
                                .is_some_and(|e| e.base().is_none())
                        {
                            placed |= loaded.state_mut().place(
                                &target_key,
                                &path.to_string_lossy(),
                                base.kind,
                            );
                        }

                        sender
                            .send_dotfile_skipped(&event_source, target_path.display(), &reason)
                            .await;
                        break 'entry EntryOutcome::Skipped;
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
                                // Relative, as the line above the diff names it.
                                &event_source.relative().to_string(),
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
                            let src = event_source.clone();
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
                            decided
                        } else {
                            sender
                                .send_dotfile_conflict(
                                    &event_source,
                                    target_path.display(),
                                    rendered.get_or_insert_with(&render),
                                )
                                .await;
                            break 'entry EntryOutcome::Conflicted;
                        }
                    }
                };

                match deploy_and_record(
                    filesystem,
                    sender,
                    &mut ledger,
                    &unit,
                    decided,
                    &mut backed_up,
                )
                .await
                {
                    DeployOutcome::Deployed => EntryOutcome::Deployed,
                    DeployOutcome::Previewed => EntryOutcome::Skipped,
                    // A refusal or a write failure, already reported as whichever it
                    // was. Here they are the same thing: asked to deploy, did not.
                    DeployOutcome::Refused => EntryOutcome::Refused,
                    DeployOutcome::Unrecorded(reason) => EntryOutcome::Unrecorded(reason),
                }
            };

            match outcome {
                EntryOutcome::Deployed => tally.deployed += 1,
                EntryOutcome::Skipped => tally.skipped += 1,
                EntryOutcome::Conflicted => tally.conflicts += 1,
                EntryOutcome::Refused => {
                    if let Some(stop) = tally.refuse(config, token, failed()) {
                        stopped = Some(stop);
                        break 'packages;
                    }
                }
                EntryOutcome::Unrecorded(reason) => {
                    stopped = Some(Stop::Unrecorded(reason));
                    break 'packages;
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
        return match stop {
            Stop::Cancelled => None,
            stop => Some(OperationResult::Failure(OperationFailure::Generic(
                stop.to_string(),
            ))),
        };
    }

    // Orphans are judged after the entries, so a record this run just wrote is
    // judged as written. A run that stopped part way neither reports nor drops
    // anything.
    let owner = match &scope {
        Scope::All(_) => None,
        Scope::Named(name) => Some(name.as_str()),
    };
    let findings = orphan::check(
        filesystem,
        catalog,
        config,
        ledger.loaded().map_or(&empty, LoadedState::state),
        owner,
        sender,
    )
    .await;
    // The token is asked again after the orphans are reported: a cancel that
    // arrived while they were being sent must still stop the run before it writes.
    if token.is_cancelled() {
        return None;
    }
    tally.orphaned = findings.reported;
    // A dry run has no state to change, or leaves the one it read alone.
    if let Ledger::Record(loaded) = &mut ledger
        && (findings.settle(loaded.state_mut()) | placed)
        && let Err(e) = save_deploy_state(filesystem, loaded)
    {
        // Every deployment is already recorded; only the housekeeping is lost,
        // and the next run redoes it.
        sender
            .send_warning(format!(
                "Could not tidy the deploy state after the run, so the next apply tries again: {e}"
            ))
            .await;
    }

    Some(OperationResult::Success(
        tally.into_success(config.environment()),
    ))
}
