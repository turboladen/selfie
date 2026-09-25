//! Applying every entry of every selected package.
//!
//! Walks the packages an operation covers and sends each entry down the path its
//! content source calls for. Three things stop the run before the end: the caller
//! cancelling, the deploy state failing to write, and a secret-bearing entry
//! failing to resolve while `stop_on_error` is on.

use std::collections::HashMap;
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

use super::deploy_entry::{DeployUnit, Recorded, perform_deploy, record_and_save};
use super::port::ApplyOptions;
use super::refusal::{guard_refusal, readable_target, refusal_warning, target_refusal};
use super::secret::{SecretApply, SecretOutcome, secret_origin};
use super::state_file::{LoadedState, StateLoad, load_deploy_state, read_only_state_warning};

/// How a cancelled apply is reported.
///
/// One constant so the two sites that stop a run — between entries, and after a
/// provider command was killed mid-flight — cannot describe the same event two
/// different ways.
const APPLY_CANCELLED: &str = "Apply cancelled";

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
    let mut refused_count = usize::from(refused_repository);

    // Set when the run stops early. Held rather than returned so every stop
    // reports through the one failure below.
    //
    // `stop_on_error` governs secret-resolution failures only: a repository-file
    // refusal or write failure is counted and the loop continues. A failed state
    // record stops the run whatever `stop_on_error` says, because the next entry
    // would fail the same way.
    let mut stopped: Option<String> = None;

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
            refused_count += 1;
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
            // write. Reading a fifo blocks until a writer opens it, and a
            // character device would be read from, then written to. A symlink is
            // refused here whatever it points at and whether or not its content
            // already matches: reading it would checksum the destination, a file
            // selfie was never asked to manage, and a repository-file entry never
            // writes through a link, so there is no outcome the read could change.
            if let Some(refusal) = guard_refusal(filesystem, &target_path) {
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

            // Ahead of the decision, like the fifo refusal, so it holds under
            // `auto_accept`, under an interactive resolver, and in a dry run. The
            // bytes read here are also what the conflict diff shows, so the
            // checksum and the diff cannot disagree about the target.
            let current = match readable_target(filesystem, source, &target_path) {
                Ok(current) => current,
                Err(warning) => {
                    sender.send_warning(warning).await;
                    refused_count += 1;
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

            match decision {
                DeployDecision::Deploy => {
                    if perform_deploy(filesystem, sender, &unit, options.dry_run, &mut backed_up)
                        .await
                        .is_ok()
                    {
                        if options.dry_run {
                            skipped_count += 1;
                        } else {
                            deployed_count += 1;
                            if let Some(reason) = record_and_save(
                                filesystem,
                                &mut loaded,
                                sender,
                                Recorded::Deployed,
                                &unit,
                            )
                            .await
                            {
                                stopped = Some(reason);
                                break 'packages;
                            }
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
                        stopped = Some(reason);
                        break 'packages;
                    }

                    sender
                        .send_dotfile_skipped(source_path.display(), target_path.display(), &reason)
                        .await;
                    skipped_count += 1;
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
                    // entry to `perform_deploy`'s dry-run skip and report it as
                    // skipped -- leaving the summary at zero conflicts. The preview
                    // someone runs to see what `--yes` would overwrite is the one
                    // place that count has to be right.
                    let accept = if options.dry_run {
                        false
                    } else if options.auto_accept {
                        true
                    } else if let Some(resolver) = &options.conflict_resolver {
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
                        if perform_deploy(
                            filesystem,
                            sender,
                            &unit,
                            options.dry_run,
                            &mut backed_up,
                        )
                        .await
                        .is_ok()
                        {
                            if options.dry_run {
                                skipped_count += 1;
                            } else {
                                deployed_count += 1;
                                if let Some(reason) = record_and_save(
                                    filesystem,
                                    &mut loaded,
                                    sender,
                                    Recorded::Deployed,
                                    &unit,
                                )
                                .await
                                {
                                    stopped = Some(reason);
                                    break 'packages;
                                }
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
                                rendered.get_or_insert_with(&render),
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
    // so.
    //
    // Does not overwrite an existing reason: `stop_on_error` names the entry that
    // failed, which is more specific than this.
    if stopped.is_none() && token.is_cancelled() {
        stopped = Some(APPLY_CANCELLED.to_string());
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
