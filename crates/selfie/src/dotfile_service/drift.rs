//! Reporting how far every tracked target has drifted from its source.
//!
//! Reads and compares; writes nothing, records nothing and resolves no conflict.

use std::path::Path;

use crate::{
    config::SelfieConfig,
    dotfile_service::{
        deploy::{DeployDecision, compute_checksum, deploy_decision, resolve_source_path},
        state::{DeployState, DriftType},
    },
    fs::{
        filesystem::FileSystem,
        target::{deploy_target, expand_target_path, repository_path},
    },
    package::{
        ContentSource, Package,
        event::{EventSender, OperationResult, OperationSuccess, StepCount},
    },
    paths::is_within,
};

use super::refusal::{
    readable_target, refusal_warning, repository_read_refusal, target_refusal,
    unmanaged_symlink_reason,
};
use super::secret::secret_origin;
use super::state_file::{StateLoad, load_deploy_state, read_only_state_warning};

/// Core logic for checking drift
///
/// `unreadable_repository` says a dotfiles repository could not be listed, so
/// `packages` is missing whatever it holds.
pub(super) async fn handle_check_drift<F>(
    packages: &[Package],
    filesystem: &F,
    config: &SelfieConfig,
    sender: &EventSender,
    unreadable_repository: bool,
    unloaded_specs: usize,
) -> OperationResult
where
    F: FileSystem,
{
    // Drift only reads, so an unusable state file is reported and the check runs
    // against an empty one: every entry then shows as untracked, which is the
    // honest answer while the file cannot be read.
    let deploy_state = match load_deploy_state(filesystem, config) {
        StateLoad::Usable(loaded) => loaded.into_state(),
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

    for package in packages {
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

            // `read_file_bytes` follows a final-component symlink, so a symlinked
            // target is checksummed by its destination. Following the link is
            // deliberate: not following would change the drift type, and with it
            // the counts `sync status` reads.
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
            let target_exists = current.is_some();
            let target_checksum = current.as_deref().map(compute_checksum).unwrap_or_default();

            let drift = deploy_state.detect_drift(
                &target_path.display().to_string(),
                &source_checksum,
                &target_checksum,
            );
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
        refused_count,
        unloaded_specs,
        environment: config.environment().to_string(),
        steps_completed: StepCount::new(total_count, total_count),
    })
}
