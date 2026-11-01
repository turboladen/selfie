//! A fixture for every operation's success, for tests of how a summary is
//! worded and shown.

use std::collections::HashSet;

use selfie::package::event::{
    AuditResult, EnvironmentStatus, OperationSuccess, RefusalKind, UnknownStatus,
};
use selfie::package::port::PackageRepoError;

/// One success of every [`OperationSuccess`] variant, each naming
/// `environment`, `package` and `path` wherever its variant names one, with
/// gaps for the variants that carry them. `PackageInstalled` appears twice:
/// once already installed at `path`, once installed with gaps.
///
/// # Panics
///
/// Panics when a variant has no success here, so a variant the library adds
/// fails every test that asks for these until it has one.
#[must_use]
pub fn every_success(environment: &str, package: &str, path: &str) -> Vec<OperationSuccess> {
    use selfie::package::event::{CheckVerdict, Outcome, StepCount};

    // One gap the run could not answer for and one selfie failed on, so
    // both clauses name the package.
    let gaps = || {
        [
            UnknownStatus::Refused {
                kind: RefusalKind::AmbiguousName,
                reason: "two files claim it".to_string(),
            },
            UnknownStatus::Error(PackageRepoError::IoError(std::sync::Arc::new(
                std::io::Error::other("disk"),
            ))),
        ]
        .into_iter()
        .map(|reason| {
            EnvironmentStatus::Unknown(reason)
                .gap(package)
                .expect("a refusal and an error are gaps")
        })
        .collect::<Vec<_>>()
    };
    let steps = || StepCount::new(1, 1);
    let all = vec![
        OperationSuccess::PackageChecked {
            package_name: package.to_string(),
            environment: environment.to_string(),
            verdict: CheckVerdict::Installed,
            steps_completed: steps(),
        },
        OperationSuccess::PackageAudited {
            package_name: package.to_string(),
            environment: environment.to_string(),
            audit_result: AuditResult::Clean {
                sources: Vec::new(),
            },
            steps_completed: steps(),
        },
        OperationSuccess::PackagesAudited {
            audited_count: 3,
            conflict_count: 1,
            not_installed_count: 1,
            error_count: 0,
            refused_count: 1,
            environment: environment.to_string(),
            steps_completed: steps(),
        },
        OperationSuccess::PackageInstalled {
            package_name: package.to_string(),
            environment: environment.to_string(),
            was_already_installed: true,
            executable_path: Some(path.to_string()),
            gaps: Vec::new(),
            steps_completed: steps(),
        },
        OperationSuccess::PackageInstalled {
            package_name: package.to_string(),
            environment: environment.to_string(),
            was_already_installed: false,
            executable_path: None,
            gaps: gaps(),
            steps_completed: steps(),
        },
        OperationSuccess::PackageValidated {
            package_name: package.to_string(),
            environment: environment.to_string(),
            outcome: Outcome::Failed,
            error_count: 2,
            warning_count: Some(1),
            steps_completed: steps(),
        },
        OperationSuccess::SpecInfoRetrieved {
            package_name: package.to_string(),
            environment: environment.to_string(),
            steps_completed: steps(),
        },
        OperationSuccess::PackageStatusChecked {
            package_name: package.to_string(),
            environment: environment.to_string(),
            gaps: gaps(),
            steps_completed: steps(),
        },
        OperationSuccess::PackageListGenerated {
            valid_count: 2,
            invalid_count: 1,
            refused_specs: 1,
            gaps: gaps(),
            environment: environment.to_string(),
            steps_completed: steps(),
        },
        OperationSuccess::DotfilesListed {
            package_count: 2,
            gaps: gaps(),
        },
        OperationSuccess::PackageCreated {
            package_name: package.to_string(),
            file_path: path.into(),
            environment: environment.to_string(),
            steps_completed: steps(),
        },
        OperationSuccess::PackageUpdated {
            package_name: package.to_string(),
            environment: environment.to_string(),
            steps_completed: steps(),
        },
        OperationSuccess::PackageRemoved {
            package_name: package.to_string(),
            file_path: path.into(),
            environment: environment.to_string(),
            dependent_packages: vec![package.to_string()],
            steps_completed: steps(),
        },
        OperationSuccess::SpecListGenerated {
            valid_count: 2,
            invalid_count: 0,
            refused_specs: 1,
            environment: environment.to_string(),
            steps_completed: steps(),
        },
        OperationSuccess::SpecsValidated {
            validated_count: 3,
            error_count: 1,
            unvalidated: Vec::new(),
            warning_count: 1,
            other_warning_count: 1,
            environment: environment.to_string(),
            steps_completed: steps(),
        },
        OperationSuccess::DotfilesApplied {
            deployed_count: 1,
            skipped_count: 1,
            conflict_count: 0,
            refused_count: 1,
            failed_count: 1,
            orphan_count: 1,
            environment: environment.to_string(),
            steps_completed: steps(),
        },
        OperationSuccess::DotfileDriftChecked {
            drift_count: 1,
            total_count: 2,
            refused_count: 0,
            unverified_count: 1,
            orphan_count: 1,
            unjudged_count: 1,
            environment: environment.to_string(),
            steps_completed: steps(),
        },
        OperationSuccess::DotfileTracked {
            name: package.to_string(),
            source_path: path.into(),
            target_path: path.to_string(),
            was_already_tracked: false,
            environment: environment.to_string(),
            steps_completed: steps(),
        },
        OperationSuccess::SyncPushComplete {
            commits_pushed: 2,
            steps_completed: steps(),
        },
        OperationSuccess::SyncPullComplete {
            commits_pulled: 1,
            packages_updated: vec![package.to_string(), package.to_string()],
            packages_added: Vec::new(),
            packages_removed: vec![package.to_string()],
            steps_completed: steps(),
        },
        OperationSuccess::SyncPullUpToDate {
            steps_completed: steps(),
        },
        OperationSuccess::SyncNothingToPush {
            steps_completed: steps(),
        },
        OperationSuccess::Generic("done".to_string()),
    ];
    let covered: HashSet<usize> = all.iter().map(variant).collect();
    assert_eq!(
        covered,
        (0..VARIANTS).collect(),
        "every OperationSuccess variant needs a success in every_success"
    );
    all
}

// The number of `OperationSuccess` variants, one for each arm of `variant`.
const VARIANTS: usize = 22;

// Which variant `success` is, as a number below `VARIANTS`. No arm is a
// wildcard, so a variant the library adds fails to compile here until it is
// given the next number, and `VARIANTS` with it.
fn variant(success: &OperationSuccess) -> usize {
    match success {
        OperationSuccess::PackageChecked { .. } => 0,
        OperationSuccess::PackageAudited { .. } => 1,
        OperationSuccess::PackagesAudited { .. } => 2,
        OperationSuccess::PackageInstalled { .. } => 3,
        OperationSuccess::PackageValidated { .. } => 4,
        OperationSuccess::SpecInfoRetrieved { .. } => 5,
        OperationSuccess::PackageStatusChecked { .. } => 6,
        OperationSuccess::PackageListGenerated { .. } => 7,
        OperationSuccess::DotfilesListed { .. } => 8,
        OperationSuccess::PackageCreated { .. } => 9,
        OperationSuccess::PackageUpdated { .. } => 10,
        OperationSuccess::PackageRemoved { .. } => 11,
        OperationSuccess::SpecListGenerated { .. } => 12,
        OperationSuccess::SpecsValidated { .. } => 13,
        OperationSuccess::DotfilesApplied { .. } => 14,
        OperationSuccess::DotfileDriftChecked { .. } => 15,
        OperationSuccess::DotfileTracked { .. } => 16,
        OperationSuccess::SyncPushComplete { .. } => 17,
        OperationSuccess::SyncPullComplete { .. } => 18,
        OperationSuccess::SyncPullUpToDate { .. } => 19,
        OperationSuccess::SyncNothingToPush { .. } => 20,
        OperationSuccess::Generic(_) => 21,
    }
}
