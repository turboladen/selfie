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
        event::{EventSender, OperationResult, OperationSuccess, StepCount},
        refusal::refuse_up_front,
    },
};

use super::classify::{
    Classified, PackageCollisions, Purpose, RepoRead, ResolvedHome, classify_entry, read_repo_file,
};
use super::orphan::{self, Catalog};
use super::state_file::{StateLoad, load_deploy_state, read_only_state_warning};

/// Core logic for checking drift, or `None` if the run was cancelled part way.
///
/// `None` says the run was left early and the counts are partial. Only the
/// caller can report that, and it must not infer it from the token: a run that
/// finished its orphan check is a whole answer even if the token was cancelled
/// after it, and a collection failure is not a cancellation whatever the token
/// says.
///
/// `collection_refusals` counts what collecting the catalog's packages refused,
/// such as a dotfiles directory it could not list or a name several spec files
/// claim.
pub(super) async fn handle_check_drift<F>(
    catalog: Catalog<'_>,
    filesystem: &F,
    config: &SelfieConfig,
    sender: &EventSender,
    token: &CancellationToken,
    collection_refusals: usize,
) -> Option<OperationResult>
where
    F: FileSystem,
{
    // Drift only reads, so an unusable state file is reported and the check runs
    // against an empty one: every entry then shows as untracked, which is the
    // honest answer while the file cannot be read.
    let deploy_state = match load_deploy_state(
        filesystem,
        config.state_directory().map(std::path::PathBuf::as_path),
    ) {
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

    // One refusal for each thing collection refused, as apply counts them: a drift
    // report missing every standalone dotfile, or a package it never examined,
    // must not read as all clear.
    let mut tally = DriftTally {
        refused: collection_refusals,
        ..DriftTally::default()
    };

    // A guard at each place a cancel can land, because a `for` body's first
    // statement never runs over an empty set. This one stops a cancel that arrived
    // before or during the state load above before any package is read, so such a
    // run never reports zero counts and exit 0 for a check that examined nothing.
    if token.is_cancelled() {
        return None;
    }

    // Asked once, so every package compares targets against the same home.
    let home = ResolvedHome::of(filesystem);
    // The same question apply asks, so the two commands cannot answer
    // differently about one file. Drift reporting a package clean while apply
    // refuses it is worse than either answer alone: it sends a reader to run the
    // command that will not run. A package refused whole has no entry examined
    // at all, so its reason is reported against the package.
    let (packages, refused) = refuse_up_front(catalog.packages, config.environment(), sender).await;
    tally.refused += refused.len();

    for package in packages {
        // Between packages, for a run whose entries are few or absent.
        if token.is_cancelled() {
            return None;
        }

        // Source paths resolve relative to the YAML file's parent directory
        let base_dir = package
            .path()
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let source_base = super::classify::source_base(config, package);

        // The same collisions apply refuses, so drift refuses the same entries.
        let collisions = PackageCollisions::of(package, &home, config.environment());

        for scoped in &package.effective_dotfiles(Some(config.environment())) {
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
            let repo = match classify_entry(
                filesystem,
                &base_dir,
                source_base.as_ref(),
                *scoped,
                &collisions,
                Purpose::Check,
            ) {
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
                            &secret.source,
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
                &repo.target.state_key(),
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

    // The orphan check is part of the answer, so a cancel that arrives before it
    // leaves the answer partial.
    if token.is_cancelled() {
        return None;
    }
    // Drift reports and writes nothing, so a gone orphan's record is left for an
    // apply to drop.
    let findings = orphan::check(filesystem, catalog, config, &deploy_state, None, sender).await;
    tally.orphaned = findings.reported;
    tally.unjudged = findings.unjudged;
    // The token is asked again after the orphans are reported: a cancel that
    // arrived while they were being checked leaves the answer partial.
    if token.is_cancelled() {
        return None;
    }

    Some(OperationResult::Success(
        tally.into_success(config.environment()),
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
    /// Orphaned targets whose files are still there. Neither drift nor a refusal.
    orphaned: usize,
    /// Recorded targets the orphan check could not judge.
    unjudged: usize,
}

impl DriftTally {
    fn into_success(self, environment: &str) -> OperationSuccess {
        OperationSuccess::DotfileDriftChecked {
            drift_count: self.drifted,
            total_count: self.compared,
            refused_count: self.refused,
            unverified_count: self.unverified,
            orphan_count: self.orphaned,
            unjudged_count: self.unjudged,
            environment: environment.to_string(),
            steps_completed: StepCount::new(self.compared, self.compared),
        }
    }
}
