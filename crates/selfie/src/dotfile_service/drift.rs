//! Reporting how far every tracked target has drifted from its source.
//!
//! Reads and compares; writes nothing, records nothing and resolves no conflict.
//! A cancelled run stops between entries and says so to its caller, so a partial
//! count is never read as a clean result.

use std::path::Path;

use tokio_util::sync::CancellationToken;

use crate::{
    config::SelfieConfig,
    dotfile_service::{
        deploy::{compute_checksum, resolve_source_path},
        state::{DeployState, DriftType},
    },
    fs::{
        filesystem::{FileSystem, repository_read_refusal},
        target::{deploy_target, expand_target_path, repository_path},
    },
    package::{
        ContentSource, Package,
        event::{EventSender, OperationResult, OperationSuccess, StepCount},
    },
    paths::is_within,
};

use super::refusal::{guard_refusal, readable_target, refusal_warning, target_refusal};
use super::secret::secret_origin;
use super::state_file::{StateLoad, load_deploy_state, read_only_state_warning};

/// Core logic for checking drift, or `None` if the run was cancelled part way.
///
/// `None` says the loops were left early and the counts are partial. Only the
/// caller can report that, and it must not infer it from the token: a run whose
/// last entry completed is a whole answer even if the token was cancelled after it,
/// and a collection failure is not a cancellation whatever the token says.
///
/// `unreadable_repository` says a dotfiles repository could not be listed, so
/// `packages` is missing whatever it holds.
pub(super) async fn handle_check_drift<F>(
    packages: &[Package],
    filesystem: &F,
    config: &SelfieConfig,
    sender: &EventSender,
    token: &CancellationToken,
    unreadable_repository: bool,
    unloaded_specs: usize,
) -> Option<OperationResult>
where
    F: FileSystem,
{
    // Drift only reads, so an unusable state file is reported and the check runs
    // against an empty one: every entry then shows as untracked, which is the
    // honest answer while the file cannot be read.
    let deploy_state = match load_deploy_state(filesystem, config) {
        StateLoad::Usable(loaded) => {
            if let Some(warning) = loaded.directory_warning() {
                sender.send_warning(warning.to_string()).await;
            }
            loaded.into_state()
        }
        StateLoad::Unusable(failure) => {
            sender.send_warning(read_only_state_warning(&failure)).await;
            DeployState::empty()
        }
    };

    let mut drift_count: usize = 0;
    let mut total_count: usize = 0;
    // One for an unlistable dotfiles directory, as apply counts it. A drift
    // report missing every standalone dotfile must not read as all clear.
    let mut refused_count = usize::from(unreadable_repository);

    // Three guards, one per case, because a `for` body's first statement never runs
    // over an empty set: this one covers no packages at all, and a cancel arriving
    // before or during the state load above. Without it such a run reported
    // `DotfileDriftChecked` with zero counts and exit 0 — a clean bill of health for
    // a check that examined nothing (found by Copilot on PR #185).
    if token.is_cancelled() {
        return None;
    }

    for package in packages {
        // Between packages, for a run whose entries are few or absent.
        if token.is_cancelled() {
            return None;
        }

        // The same question apply asks, in the same place, so the two commands
        // cannot answer differently about one file. Drift reporting a package
        // clean while apply refuses it is worse than either answer alone: it
        // sends a reader to run the command that will not run.
        //
        // The entries are not examined at all. A package refused whole is
        // refused before there is an entry to attach a reason to, which is the
        // same reason apply asks here rather than per entry.
        if let Some(refusal) = package.spec_refusal(config.environment()) {
            sender
                .send_warning(format!("Skipping package '{}': {refusal}", package.name()))
                .await;
            refused_count += 1;
            continue;
        }

        // Source paths resolve relative to the YAML file's parent directory
        let base_dir = package
            .path()
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();

        for entry in &package.dotfiles_for_environment(config.environment()) {
            // Between entries. Drift reads and checksums every tracked file, so
            // over a large repository an unchecked run goes on long after the
            // receiver is gone.
            if token.is_cancelled() {
                return None;
            }

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

            // Same lexical guard as handle_apply — see `crate::paths::is_within`.
            if !is_within(&source_path, &base_dir) {
                sender
                    .send_warning(format!(
                        "Skipping '{source}': source path escapes YAML base directory"
                    ))
                    .await;
                continue;
            }

            // The guard apply asks, after the containment check as apply asks it, so
            // an escaping source is refused the same way whatever is at the target,
            // and worded identically through `refusal_warning`. Drift reads the target
            // to checksum it, so it hangs on a fifo exactly as apply does, and reading
            // through a link would checksum a file selfie does not manage.
            //
            // Refused and not examined, like an unreadable target below: a green
            // result over a target drift never looked at is a false success.
            if let Some(refusal) = guard_refusal(filesystem, &target_path) {
                sender.send_warning(refusal_warning(source, &refusal)).await;
                refused_count += 1;
                total_count -= 1;
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

            let current = match readable_target(filesystem, source, &target_path) {
                Ok(current) => current,
                Err(warning) => {
                    sender.send_warning(warning).await;
                    // Refused and not examined, unlike the per-entry refusals
                    // above it: a green "0 drifted" over a target drift could
                    // not read is a false success, which outranks parity with
                    // its neighbors, and `sync status` renders the total as
                    // "N deployed".
                    refused_count += 1;
                    total_count -= 1;
                    continue;
                }
            };
            let target_checksum = current.as_deref().map(compute_checksum).unwrap_or_default();

            let drift = deploy_state.detect_drift(
                &target_path.display().to_string(),
                &source_checksum,
                &target_checksum,
            );
            if drift != DriftType::None {
                sender
                    .send_dotfile_drift_detected(target_path.display(), &drift)
                    .await;
                drift_count += 1;
            }
        }
    }

    Some(OperationResult::Success(
        OperationSuccess::DotfileDriftChecked {
            drift_count,
            total_count,
            refused_count,
            unloaded_specs,
            environment: config.environment().to_string(),
            steps_completed: StepCount::new(total_count, total_count),
        },
    ))
}
