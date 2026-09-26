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
        deploy::compute_checksum,
        state::{DeployState, DriftType},
    },
    fs::filesystem::FileSystem,
    package::{
        Package,
        event::{EventSender, OperationResult, OperationSuccess, StepCount},
    },
};

use super::classify::{Classified, Purpose, RepoRead, classify_entry, read_repo_file};
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

    // One refusal for an unlistable dotfiles directory, as apply counts it. A
    // drift report missing every standalone dotfile must not read as all clear.
    let mut tally = DriftTally {
        refused: usize::from(unreadable_repository),
        ..DriftTally::default()
    };

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
            tally.refused += 1;
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

            // Refused for the same reasons apply refuses it, in the same order. A
            // drift check that described an undeployable entry differently would
            // send the user looking for a different problem from the one apply
            // reports, and every refusal counts: a green result over an entry drift
            // never examined is a false success.
            let repo = match classify_entry(filesystem, &base_dir, entry, Purpose::Check) {
                Ok(Classified::RepoFile(repo)) => repo,
                // Secret-bearing entries hold no deploy state, so there is nothing
                // to compare against, and resolving them here would run the user's
                // commands: leaking content into a read-only operation and
                // prompting for authentication. Classified first all the same, so
                // an entry apply would refuse is reported as refused rather than as
                // merely unverifiable.
                //
                // Reported as unverifiable rather than counted as drift or as a
                // refusal. Either would leave `dotfiles drift` permanently dirty on
                // any machine with one provider-sourced dotfile (ADR-0003).
                Ok(Classified::SecretBearing(secret)) => {
                    sender
                        .send_dotfile_skipped(
                            &secret.origin,
                            secret.path.display(),
                            "provider-sourced (not verifiable without resolving)",
                        )
                        .await;
                    tally.unverified += 1;
                    continue;
                }
                Err(refused) => {
                    refused.send(sender).await;
                    tally.refused += 1;
                    continue;
                }
            };
            let RepoRead {
                source_content,
                current,
            } = match read_repo_file(filesystem, &repo) {
                Ok(read) => read,
                Err(refused) => {
                    refused.send(sender).await;
                    tally.refused += 1;
                    continue;
                }
            };
            let source_checksum = compute_checksum(source_content.as_bytes());
            let target_checksum = current.as_deref().map(compute_checksum).unwrap_or_default();

            tally.compared += 1;
            let drift = deploy_state.detect_drift(
                &repo.target.display().to_string(),
                &source_checksum,
                &target_checksum,
            );
            if drift != DriftType::None {
                sender
                    .send_dotfile_drift_detected(repo.target.display(), &drift)
                    .await;
                tally.drifted += 1;
            }
        }
    }

    Some(OperationResult::Success(
        tally.into_success(config.environment(), unloaded_specs),
    ))
}

/// What a drift check found for each entry, counted as it goes.
#[derive(Default)]
struct DriftTally {
    drifted: usize,
    /// Entries compared against their source, and nothing else. `sync status`
    /// renders this as the entries in place, so an entry refused or reported
    /// unverified never reaches it.
    compared: usize,
    refused: usize,
    unverified: usize,
}

impl DriftTally {
    fn into_success(self, environment: &str, unloaded_specs: usize) -> OperationSuccess {
        OperationSuccess::DotfileDriftChecked {
            drift_count: self.drifted,
            total_count: self.compared,
            refused_count: self.refused,
            unloaded_specs,
            unverified_count: self.unverified,
            environment: environment.to_string(),
            steps_completed: StepCount::new(self.compared, self.compared),
        }
    }
}
