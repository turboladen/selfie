//! DotfileService implementation
//!
//! This module provides the concrete implementation of the [`DotfileService`] trait.
//! It coordinates between the package repository (for loading package dotfiles),
//! the file system (for reading/writing dotfiles), and the application config
//! to perform dotfile deployment operations.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    commands::CommandRunner,
    config::SelfieConfig,
    dotfile_service::{
        backup,
        deploy::{DeployDecision, compute_checksum, deploy_decision, resolve_source_path},
        diff::unified_diff,
        port::{ConflictDetail, ConflictResolution},
        resolve::{ResolvedContent, check_resolvable, resolve_content},
        state::{DeployState, DriftType},
    },
    fs::{
        filesystem::{FileSystem, FileSystemError},
        target::{
            TargetPath, TargetRejection, deploy_target, expand_target_path, portable_target,
            repository_path,
        },
    },
    package::{
        ContentSource, DotfileEntry, Package,
        event::{
            EventSender, EventStream, OperationContext, OperationFailure, OperationResult,
            OperationSuccess, PackageEvent, StepCount, metadata::OperationType,
        },
        port::{PackageRepoError, PackageRepository},
    },
    paths::is_within,
    privilege::{Privilege, SudoPolicy, SudoRefusal, WriteScope},
};

use super::port::{ApplyOptions, DotfileService};
use super::state_file::{
    LoadedState, StateLoad, StateSaveError, load_deploy_state, read_only_state_warning,
    save_deploy_state,
};

/// How a cancelled apply is reported.
///
/// One constant so the two sites that stop a run — between entries, and after a
/// provider command was killed mid-flight — cannot describe the same event two
/// different ways.
const APPLY_CANCELLED: &str = "Apply cancelled";

/// A non-fatal warning raised while collecting packages, before any event stream
/// exists to send it on.
///
/// Two kinds travel together because they are produced in one pass, and they are
/// kept apart because they leave as different events: a skipped spec is reported
/// whole so each adapter can render it, and everything else is already prose.
enum ApplyWarning {
    /// A package file that could not be parsed.
    SkippedSpec(crate::package::port::PackageParseError),
    /// A repository that exists and could not be listed, so the collection is
    /// missing whatever it holds.
    UnreadableRepository(crate::package::port::PackageListError),
    /// A dotfiles directory the user configured that does not exist.
    MissingDotfilesDirectory(PathBuf),
    /// Anything else worth saying, already worded.
    Other(String),
}

/// What a package name appearing in both directories means to the caller.
// A parameter rather than two collectors: the reading of both repositories, the
// unparsable-spec reporting and the unlistable-directory handling are identical,
// and only this one question differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NameCollision {
    /// The `packages/` copy wins and the other is dropped with a warning.
    PackagesWin,
    /// Both are kept, because both files are there.
    KeepBoth,
}

impl ApplyWarning {
    /// Whether any of `warnings` is an [`UnreadableRepository`](Self::UnreadableRepository).
    fn any_unreadable_repository(warnings: &[Self]) -> bool {
        warnings
            .iter()
            .any(|warning| matches!(warning, Self::UnreadableRepository(_)))
    }

    /// Emit this warning on the event stream it belongs to.
    ///
    /// The two kinds leave differently on purpose: a skipped spec travels typed so
    /// each adapter renders it, and everything else is already a sentence. Written
    /// once here rather than at each drain, so three call sites cannot disagree.
    async fn send(self, sender: &crate::package::event::EventSender) {
        match self {
            Self::SkippedSpec(error) => sender.send_spec_skipped(error).await,
            Self::UnreadableRepository(e) => {
                sender
                    .send_warning(format!("Failed to load standalone dotfiles: {e}"))
                    .await;
            }
            Self::MissingDotfilesDirectory(path) => {
                sender
                    .send_warning(super::directory::missing_warning(&path))
                    .await;
            }
            Self::Other(message) => sender.send_warning(message).await,
        }
    }
}

/// Whether `package` is the one `folded_name`, already lowercased, names.
///
/// A package is named by its spec file, with case folded, as package lookup
/// resolves a name. The YAML `name:` field does not decide it.
fn is_named(package: &Package, folded_name: &str) -> bool {
    package
        .spec_name()
        .is_some_and(|spec_name| spec_name == folded_name)
}

/// The failure for a named apply that no collected package answers.
fn no_such_package(name: &str, warnings: &[ApplyWarning]) -> OperationFailure {
    use crate::package::event::NoSuchPackageReason;

    // The unloadable check runs before either not-found answer. A spec that
    // failed to parse is not among the collected packages, and "no package
    // named" would send the user looking for a file they may be looking at.
    let requested = name.to_lowercase();
    let unloadable = warnings.iter().any(|warning| {
        matches!(warning, ApplyWarning::SkippedSpec(error)
            if crate::package::spec_name_of(error.package_path())
                .is_some_and(|spec_name| spec_name == requested))
    });

    let reason = if unloadable {
        NoSuchPackageReason::NotLoaded
    } else if ApplyWarning::any_unreadable_repository(warnings) {
        NoSuchPackageReason::MaybeInUnlistableDirectory
    } else {
        NoSuchPackageReason::NotFound
    };
    OperationFailure::NoSuchPackage {
        name: name.to_string(),
        reason,
    }
}

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
    /// `list`, `apply`, `apply_all` and `check_drift` read it alongside the
    /// main package repository. `track_standalone` writes a new spec into it
    /// instead. Attach it whether or not its directory exists: each of those
    /// operations decides for itself what a missing or unlistable directory
    /// means.
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
    /// Warnings are returned rather than emitted because collection happens before
    /// the event channel exists. Each caller sends them once its stream is up, and
    /// [`ApplyWarning`] is what tells it which event each one is.
    fn collect_all_packages(
        package_repo: &R,
        dotfiles_repo: Option<&R>,
        dotfiles_directory_configured: bool,
    ) -> Result<(Vec<Package>, Vec<ApplyWarning>), crate::package::port::PackageListError> {
        Self::collect_packages(
            package_repo,
            dotfiles_repo,
            NameCollision::PackagesWin,
            dotfiles_directory_configured,
        )
    }

    /// Collect from both repositories, deciding what a name in both means.
    ///
    /// Deploying has to choose one, because two packages cannot both own a name.
    /// Listing must not: both files exist, and a caller asking what is on disk is
    /// asking about the files rather than about what would win.
    ///
    /// `dotfiles_directory_configured` says whether the user set
    /// `dotfiles_directory`, which decides whether a dotfiles directory that is
    /// not there is worth a warning.
    fn collect_packages(
        package_repo: &R,
        dotfiles_repo: Option<&R>,
        collision: NameCollision,
        dotfiles_directory_configured: bool,
    ) -> Result<(Vec<Package>, Vec<ApplyWarning>), crate::package::port::PackageListError> {
        let mut warnings = Vec::new();

        // A package file that does not parse is dropped by `valid_packages`, and
        // silence there is dangerous for this command specifically: apply is what
        // people run, and a dotfile that quietly stops deploying surfaces much
        // later as an authentication failure nobody traces back to a typo. The
        // run would otherwise report success having done nothing at all.
        let note_unparsable = |output: &crate::package::port::ListPackagesOutput,
                               warnings: &mut Vec<ApplyWarning>| {
            for invalid in output.invalid_packages() {
                warnings.push(ApplyWarning::SkippedSpec(invalid.clone()));
            }
        };

        // The failure travels typed. Rendering it here hands the caller a bare
        // sentence, leaving it able to say only that loading failed -- not which of
        // the three fixes for a missing package directory applies.
        let output = package_repo.list_packages()?;
        note_unparsable(&output, &mut warnings);
        let unloadable_package_names: std::collections::HashSet<String> = output
            .invalid_packages()
            .filter_map(|error| crate::package::spec_name_of(error.package_path()))
            .collect();
        let mut packages = output.valid_packages().cloned().collect::<Vec<_>>();

        let packages_count = packages.len();

        if let Some(dotfiles) = dotfiles_repo {
            match dotfiles.list_packages() {
                Ok(output) => {
                    note_unparsable(&output, &mut warnings);
                    packages.extend(output.valid_packages().cloned());
                }
                Err(error) => match super::directory::UnlistedDotfilesDirectory::from_list_error(
                    error,
                    dotfiles_directory_configured,
                ) {
                    super::directory::UnlistedDotfilesDirectory::UnsetAndMissing => {}
                    super::directory::UnlistedDotfilesDirectory::ConfiguredAndMissing(path) => {
                        warnings.push(ApplyWarning::MissingDotfilesDirectory(path));
                    }
                    // Only a directory that exists and cannot be listed may be
                    // hiding dotfiles, so only this one refuses the run or fails
                    // the listing.
                    super::directory::UnlistedDotfilesDirectory::Unlistable(error) => {
                        warnings.push(ApplyWarning::UnreadableRepository(error));
                    }
                },
            }
        }

        // A packages/ spec claims its name over a dotfiles/ spec of the same
        // name. Names are spec file names with case folded, as package lookup
        // resolves them, so `bat.yml` and `Bat.yml` are one name. A packages/
        // spec that failed to parse still claims its name: deploying the
        // dotfiles/ copy in its place would apply a file the user did not mean.
        if collision == NameCollision::PackagesWin && packages.len() > packages_count {
            let claimed_by_packages: std::collections::HashSet<String> = packages[..packages_count]
                .iter()
                .filter_map(Package::spec_name)
                .collect();
            let mut seen_in_dotfiles = std::collections::HashSet::new();

            // A loaded packages/ spec is asked about first, so the warning names
            // the copy that is used. A dotfiles/ name repeated within dotfiles/
            // is its own case and does not blame packages/.
            let mut deduped_dotfiles = Vec::new();
            for pkg in packages.drain(packages_count..) {
                let Some(name) = pkg.spec_name() else {
                    deduped_dotfiles.push(pkg);
                    continue;
                };
                if claimed_by_packages.contains(&name) {
                    warnings.push(ApplyWarning::Other(format!(
                        "Duplicate name '{name}' found in both packages/ and dotfiles/ — using \
                         the packages/ version"
                    )));
                } else if unloadable_package_names.contains(&name) {
                    warnings.push(ApplyWarning::Other(format!(
                        "Not using '{name}' from dotfiles/: packages/ has a spec by that name \
                         that could not be loaded"
                    )));
                } else if !seen_in_dotfiles.insert(name.clone()) {
                    warnings.push(ApplyWarning::Other(format!(
                        "Duplicate name '{name}' found twice in dotfiles/ — using the first"
                    )));
                } else {
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

impl<R, F, CR, P> DotfileServiceImpl<R, F, CR, P>
where
    R: PackageRepository + Clone + std::fmt::Debug + Send + Sync + 'static,
    F: FileSystem + Clone + std::fmt::Debug + Send + Sync + 'static,
    CR: CommandRunner + Clone + std::fmt::Debug + Send + Sync + 'static,
    P: Privilege + Send + Sync,
{
    /// Apply every package, or only the one named by `filter`.
    fn apply_matching(&self, filter: Option<String>, options: ApplyOptions) -> EventStream {
        // Refused before collecting, so a run that will write nothing does not
        // read and parse every spec in both repositories first.
        let prepared = match self.sudo_refusal() {
            Some(refusal) => Err(OperationFailure::Privilege(refusal)),
            None => Self::collect_all_packages(
                &self.package_repository,
                self.dotfiles_repository.as_ref(),
                self.config.configured_dotfiles_directory().is_some(),
            )
            .map_err(OperationFailure::PackageList),
        };
        let fs = self.filesystem.clone();
        let runner = self.runner.clone();
        let config = self.config.clone();
        let token = self.cancellation_token.clone();

        Self::create_event_stream(move |tx| async move {
            let sender = EventSender::new_with_context(
                tx,
                OperationType::DotfileApply,
                filter.clone().unwrap_or_default(),
                config.environment().to_string(),
                OperationContext::default(),
            );

            sender.send_started().await;

            let result = match prepared {
                Ok((packages, warnings)) => {
                    // Applying everything carries on with the package dotfiles
                    // and counts an unlistable dotfiles directory as a refusal.
                    // A named apply that finds its package lost nothing to the
                    // directory, so it counts none.
                    let refused_repository =
                        filter.is_none() && ApplyWarning::any_unreadable_repository(&warnings);
                    let selected: Vec<Package> = match filter.as_deref() {
                        Some(name) => {
                            let folded_name = name.to_lowercase();
                            packages
                                .into_iter()
                                .filter(|package| is_named(package, &folded_name))
                                .collect()
                        }
                        None => packages,
                    };
                    // A name matching nothing has nothing to deploy. Completing
                    // as a success with every count at zero would read as
                    // "already up to date" to a user who mistyped the name.
                    let unmatched = filter
                        .as_deref()
                        .filter(|_| selected.is_empty())
                        .map(|name| no_such_package(name, &warnings));
                    // Drained first, so a skipped spec's own reason precedes the
                    // failure it explains.
                    for warning in warnings {
                        warning.send(&sender).await;
                    }
                    if let Some(failure) = unmatched {
                        return sender
                            .send_completed(OperationResult::Failure(failure))
                            .await;
                    }
                    let ctx = ApplyContext {
                        filesystem: &fs,
                        runner: &runner,
                        config: &config,
                        sender: &sender,
                        options: &options,
                        token: &token,
                    };
                    handle_apply(&selected, &ctx, refused_repository).await
                }
                Err(failure) => OperationResult::Failure(failure),
            };

            sender.send_completed(result).await;
        })
    }
}

impl<R, F, CR, P> DotfileService for DotfileServiceImpl<R, F, CR, P>
where
    R: PackageRepository + Clone + std::fmt::Debug + Send + Sync + 'static,
    F: FileSystem + Clone + std::fmt::Debug + Send + Sync + 'static,
    CR: CommandRunner + Clone + std::fmt::Debug + Send + Sync + 'static,
    P: Privilege + Send + Sync,
{
    async fn apply_all(&self, options: ApplyOptions) -> EventStream {
        self.apply_matching(None, options)
    }

    async fn apply(&self, name: &str, options: ApplyOptions) -> EventStream {
        self.apply_matching(Some(name.to_string()), options)
    }

    async fn check_drift(&self) -> EventStream {
        let collected = Self::collect_all_packages(
            &self.package_repository,
            self.dotfiles_repository.as_ref(),
            self.config.configured_dotfiles_directory().is_some(),
        );
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
                    // Carries on with the package dotfiles, and
                    // `handle_check_drift` counts the unlistable directory as a
                    // refusal.
                    let unreadable_repository = ApplyWarning::any_unreadable_repository(&warnings);
                    // Counted here rather than from the relayed events: this is
                    // where the collection reports what it could not load, so
                    // the count and the warnings cannot disagree.
                    let unloaded_specs = warnings
                        .iter()
                        .filter(|warning| matches!(warning, ApplyWarning::SkippedSpec(_)))
                        .count();
                    for warning in warnings {
                        warning.send(&sender).await;
                    }
                    handle_check_drift(
                        &packages,
                        &fs,
                        &config,
                        &sender,
                        unreadable_repository,
                        unloaded_specs,
                    )
                    .await
                }
                Err(e) => OperationResult::Failure(
                    crate::package::event::OperationFailure::PackageList(e),
                ),
            };

            sender.send_completed(result).await;
        })
    }

    async fn list(&self) -> EventStream {
        // `KeepBoth`: a name in both directories is two files on disk, and a
        // listing that showed one of them would be answering the deploy question
        // instead of the one the user asked.
        let collected = Self::collect_packages(
            &self.package_repository,
            self.dotfiles_repository.as_ref(),
            NameCollision::KeepBoth,
            self.config.configured_dotfiles_directory().is_some(),
        );
        let config = self.config.clone();

        Self::create_event_stream(move |tx| async move {
            let sender = EventSender::new_with_context(
                tx,
                OperationType::DotfileList,
                String::new(),
                config.environment().to_string(),
                OperationContext::default(),
            );

            sender.send_started().await;

            let result = match collected {
                Ok((packages, warnings)) => {
                    // A directory selfie found and could not list is fatal HERE.
                    // Apply and drift still have the package dotfiles to act on,
                    // so they count it as a refusal and carry on. A listing has
                    // nothing to carry on to except a table missing every
                    // standalone dotfile.
                    let unreadable = ApplyWarning::any_unreadable_repository(&warnings);

                    // Drained before the listing is sent, so a consumer reading
                    // events in order has the caveats in hand before the answer
                    // they qualify.
                    for warning in warnings {
                        warning.send(&sender).await;
                    }

                    if unreadable {
                        return sender
                            .send_completed(OperationResult::Failure(
                                crate::package::event::OperationFailure::Generic(
                                    "Could not list every dotfile directory, so this listing \
                                     would be missing entries"
                                        .to_string(),
                                ),
                            ))
                            .await;
                    }

                    // `listing_refusal` rather than `spec_refusal`: this
                    // listing spans every environment, so a reason keyed to one
                    // named environment would refuse a package for a question
                    // the table never asks -- while a shadowing key in ANY
                    // environment empties a list this table would otherwise show
                    // as simply absent.
                    let mut refused = Vec::new();
                    let mut listable = Vec::new();
                    for package in packages {
                        if let Some(reason) = package.listing_refusal() {
                            refused.push(crate::package::event::RefusedSpec {
                                package_name: package.name().to_string(),
                                path: package.path().display().to_string(),
                                reason: reason.to_string(),
                            });
                        } else if !package.dotfiles_with_scope().is_empty() {
                            listable.push(package);
                        }
                    }

                    let packages = listable;
                    let count = packages.len();

                    sender
                        .send_dotfile_list(crate::package::event::DotfileListData {
                            packages,
                            refused,
                            package_directory: config.package_directory().display().to_string(),
                            dotfiles_directory: config.dotfiles_directory().display().to_string(),
                        })
                        .await;

                    OperationResult::Success(OperationSuccess::Generic(format!(
                        "Listed dotfiles across {count} package(s)"
                    )))
                }
                Err(e) => OperationResult::Failure(
                    crate::package::event::OperationFailure::PackageList(e),
                ),
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
                        &sender,
                        &config,
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
                        &sender,
                        &config,
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

    // Says that nothing is kept, because every other overwrite selfie performs
    // does keep a copy. A user who has seen that line elsewhere would otherwise
    // assume this overwrite is recoverable too, and accepting is the only way
    // past a secret conflict.
    format!(
        "  {}\n  target exists and differs from resolved output\n\n  \
         resolved output : {} lines\n  current target  : {current_side}\n  (content hidden)\n  \
         no copy of the current target is kept",
        origin,
        lines(incoming),
    )
}

/// What is at an entry's target when apply or drift reaches it.
///
/// Kept distinct from `Option<Vec<u8>>` because "absent" and "present but
/// unreadable" call for opposite handling: the first is safe to write, the second
/// must never be written over as though nothing were there.
enum TargetState {
    Absent,
    Readable(Vec<u8>),
    Unreadable(FileSystemError),
}

/// What is at `target`: absent, readable, or present but unreadable.
///
/// Read as raw bytes, so two different files are never reported identical after
/// a lossy decode.
// An unreadable file is still a file, and it may be the very thing an overwrite
// would destroy. No caller treats it as absent, which would write over it
// with no prompt: the secret-bearing path reports a conflict and lets an
// interactive resolver choose, since replacing a file needs only write
// permission on its directory; the repository-file path and drift refuse the
// entry outright, because there is no content to show a diff against.
fn read_target_state<F: FileSystem>(filesystem: &F, target: &TargetPath) -> TargetState {
    if !filesystem.path_exists(target.path()) {
        return TargetState::Absent;
    }

    match filesystem.read_file_bytes(target.path()) {
        Ok(bytes) => TargetState::Readable(bytes),
        Err(e) => TargetState::Unreadable(e),
    }
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
    /// Conflating any two of those loses a credential.
    fn read_target(&self, target: &SecretTarget<'_>) -> TargetState {
        read_target_state(self.filesystem, &target.path)
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
            // The error already names the target; naming it here too would print
            // the path twice.
            self.sender
                .send_warning(format!("Failed to tighten permissions: {e}"))
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
            // The error already names the target; naming it here too would print
            // the path twice.
            self.sender
                .send_warning(format!("Failed to write: {e}"))
                .await;
            return SecretOutcome::Failed;
        }

        self.sender
            // Never a copy. ADR-0003 keeps nothing derived from a credential on
            // disk, and the former content of a secret target is the credential
            // itself -- worse to persist than the checksum that ADR already
            // refuses. Owner-only permissions do not change that.
            .send_dotfile_deployed(&target.origin, target.path.display(), None)
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

// Why an entry whose target exists but could not be read is refused, worded the
// same by apply and drift. A symlink whose destination cannot be read is a
// symlinked target first, which is the refusal every command already shares;
// only a plain file gets the read failure.
fn unreadable_target_refusal<F: FileSystem>(
    filesystem: &F,
    source: &str,
    target: &TargetPath,
    error: &FileSystemError,
) -> String {
    match filesystem.symlink_refusal(target) {
        Some(refusal) => refusal_warning(source, &refusal),
        None => format!(
            "Skipping '{source}': target '{}' exists but could not be read: {error}",
            target.display()
        ),
    }
}

// The target's bytes if the entry can go on to a decision: `None` for an absent
// target, `Err(warning)` for one that exists and could not be read. Apply and
// drift both classify through here, so they cannot answer differently about one
// file.
fn readable_target<F: FileSystem>(
    filesystem: &F,
    source: &str,
    target: &TargetPath,
) -> Result<Option<Vec<u8>>, String> {
    match read_target_state(filesystem, target) {
        TargetState::Absent => Ok(None),
        TargetState::Readable(bytes) => Ok(Some(bytes)),
        TargetState::Unreadable(e) => {
            Err(unreadable_target_refusal(filesystem, source, target, &e))
        }
    }
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

/// How every command reports an already-tracked target it cannot write to.
// A target already in the spec that is not a regular file.
//
// Not `track_refusal`, whose remedy is about creating an entry: "track the path
// it points to" describes something the user cannot do once the entry exists.
//
// Claims nothing about what a later apply does, because that differs by entry
// kind: a repository-file entry is refused, while a secret-bearing one is written
// by `write_file_private`, which replaces a symlink at the final component. This
// breaks the silence and leaves the verdict to the command that has one.
pub fn already_tracked_refusal_warning(refusal: &FileSystemError) -> String {
    format!("{refusal}. The entry stays as it is, and this command wrote nothing.")
}

// Where an entry's repository file sits, for the already-tracked answer.
//
// `source` is relative to the spec's own directory, which is the same rule the
// copy is composed under. A provider entry has no file in the repository, so
// there is nothing to resolve and the spec itself is the closest true answer.
fn tracked_copy_path(spec_path: &Path, entry: &DotfileEntry) -> PathBuf {
    let spec_dir = spec_path.parent().unwrap_or_else(|| Path::new("."));
    match entry.source() {
        Some(source) => spec_dir.join(source),
        None => spec_path.to_path_buf(),
    }
}

// Why a track added nothing although it found no entry for the target: the spec
// already carries one whose recorded target matches, by a comparison that
// disagreed with the one made before the copy.
//
// Reported rather than swallowed. Selfie cannot say which of the two comparisons
// is right, and the spec is the user's file, so it declines and names what it
// found instead of rewriting either.
fn unadded_entry_failure(recorded_target: &str, copy: &Path, removal: &CopyRemoval) -> String {
    format!(
        "The spec already has an entry for '{recorded_target}', so nothing was added. {}",
        copy_fate(copy, removal)
    )
}

/// What became of a copy a track had written, once a later step failed.
enum CopyRemoval {
    /// Gone. The path still held what this call wrote.
    Removed,
    /// Left alone: the path could not be confirmed to hold what this call wrote,
    /// so removing it might have deleted a file selfie did not create.
    Unconfirmed,
    /// Removal was attempted on selfie's own copy and failed.
    Failed(FileSystemError),
}

// Remove a copy this call wrote, and only that.
//
// The guard before the copy is advisory: it answers about the path at the moment
// it is asked, and the write and this removal are two later moments. Between them
// something else can occupy the path -- concurrent selfie runs are unsupported,
// but "unsupported" is not "cannot happen", and the cost of being wrong here is
// deleting a file that belongs to someone else. So ownership is established by
// content rather than assumed from the earlier guard.

// Reading back is itself one moment before the removal, so this narrows the
// window rather than closing it. What it buys is that the ordinary case is
// provably selfie's own file and every other case is reported instead of acted
// on, which is the safe direction for a delete.
fn remove_own_copy<F: FileSystem>(filesystem: &F, path: &Path, written: &str) -> CopyRemoval {
    match filesystem.read_file(path) {
        Ok(found) if found == written => match filesystem.remove_file(path) {
            Ok(()) => CopyRemoval::Removed,
            Err(e) => CopyRemoval::Failed(e),
        },
        // Both arms leave the file: content that differs is not selfie's to
        // delete, and content it could not read back is content it cannot claim.
        Ok(_) | Err(_) => CopyRemoval::Unconfirmed,
    }
}

// What became of the copy, worded once so the two failures that compensate cannot
// describe the same outcome differently.
fn copy_fate(copy: &Path, removal: &CopyRemoval) -> String {
    let copy = copy.display();
    match removal {
        CopyRemoval::Removed => format!("The copy at '{copy}' was removed."),
        CopyRemoval::Unconfirmed => format!(
            "The copy at '{copy}' was left alone: it no longer holds what selfie wrote, so \
             removing it could have deleted another file. Check it, and remove it yourself if \
             it is not wanted."
        ),
        CopyRemoval::Failed(e) => format!(
            "The copy at '{copy}' could not be removed either: {e}. Remove it before retrying."
        ),
    }
}

// Why a track could not save the spec, and what became of the copy it had
// already written.
//
// Does not name the spec: every variant reaching here names it already -- the
// rewrite refusals by construction, and a write failure through the writer's own
// error -- and a message repeating a path the error carries reads twice as long
// as it is. `a_failed_spec_save_names_the_spec_exactly_once` holds that by
// counting rather than by checking presence, so a variant that stops naming the
// spec fails a test instead of going quiet.
fn spec_save_failure(error: &PackageRepoError, copy: &Path, removal: &CopyRemoval) -> String {
    format!(
        "Cannot save the spec: {error}. {}",
        copy_fate(copy, removal)
    )
}

// Why a track's copy and spec are in place with no deployment recorded, and how
// to finish it.
//
// Nothing is rolled back for this: both writes are correct and only the record is
// missing, so undoing them would throw away work to tidy a record.
//
// Sends the user to `selfie apply`, which finishes the job: the entry is
// untracked and its target already matches the copy, so apply's in-sync skip arm
// records it without asking.

// Says nothing about re-running track, which does different things at the two
// entry points and neither of them useful: with the entry saved, a second
// `track_for_package` answers "already tracking" and exits 0 having recorded
// nothing, while a second standalone track is refused by the spec-collision
// guard. Naming either would be wrong at the other call site.
fn unrecorded_track_failure(
    error: &StateSaveError,
    name: &str,
    recorded_target: &str,
    spec_path: &Path,
    copy: &Path,
) -> String {
    format!(
        "Tracked '{recorded_target}': the copy at '{}' and the entry in '{}' are written. \
         The deployment was not recorded: {error}. Run `selfie apply {name}` once the state \
         file can be written, and it records the deployment.",
        copy.display(),
        spec_path.display()
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
    /// `target_path` as the deploy state keys it.
    target_key: &'a str,
    source_content: &'a str,
    source_checksum: &'a str,
    /// The entry's `source` as the spec names it, recorded beside the checksum.
    source: &'a str,
    /// Where copies of overwritten targets go, or `None` if there is nowhere to
    /// put one. `Some` does not mean a copy will be made.
    // A dry run has a root here and writes nothing: `perform_deploy` returns on
    // `dry_run` before it reaches one.
    backups: Option<&'a Path>,
}

/// Deploy a single config file to its target path and emit events. Records
/// nothing: the caller records and saves once the write is known to have landed.
///
/// `backed_up` carries what this run has already copied aside, keyed by target,
/// so a target two entries deploy to is copied once.
async fn perform_deploy<F: FileSystem>(
    filesystem: &F,
    sender: &EventSender,
    unit: &DeployUnit<'_>,
    dry_run: bool,
    backed_up: &mut HashMap<String, Option<PathBuf>>,
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

    // `kept` is `Some` only for the entry that actually made the copy, so only it
    // prunes, and only once the write below has landed.
    let (backup, kept) = match backed_up.get(unit.target_key) {
        // This run has already settled this target. Report the copy it made --
        // which holds what the target held before the run touched it -- and make
        // no second one. `None` means the run found nothing there to keep.
        //
        // Without this, two entries naming one target destroy the very thing the
        // copy exists for: the first copies the user's file, the second finds the
        // first entry's output, copies that, and the prune deletes the user's.
        // Two entries can name one target -- an apply covers every package, and
        // the only same-target check anywhere looks inside a single package.
        Some(existing) => (existing.clone(), None),
        None => match keep_current(filesystem, unit) {
            Ok(kept) => {
                let path = kept.as_ref().map(|kept| kept.path().to_path_buf());
                // Recorded before the write, not after: a copy that was made and a
                // target write that then failed still holds what the target held,
                // so a later entry for this target must report it.
                backed_up.insert(unit.target_key.to_string(), path.clone());
                (path, kept)
            }
            Err(warning) => {
                sender.send_warning(warning).await;
                // Left out of `backed_up`, so a refused entry does not mark the
                // target as settled for a later one.
                return Err(());
            }
        },
    };

    // Refuses a symlinked target rather than writing through it: the content would
    // otherwise land wherever the link points, which may be a path chosen by
    // whoever planted it.
    if let Err(e) =
        filesystem.write_file_no_follow(unit.target_path, unit.source_content.as_bytes())
    {
        // A refusal is not a failure. "Failed to write" would read as something
        // going wrong rather than as selfie declining. The error names the target
        // in both arms, so neither repeats it.
        //
        // Reaching the refusal arm here means the link or fifo appeared between
        // the checks in `handle_apply` and this write. It is exercised by
        // `the_writer_refuses_even_when_the_check_is_blinded`, which asserts only
        // that the message names a symlink — not the `Skipping '{source}': `
        // wrapper. Share `refusal_warning` rather than repeating the wording, or
        // that unpinned half can drift.
        let message = match &e {
            FileSystemError::SymlinkedTarget { .. } | FileSystemError::IrregularTarget { .. } => {
                refusal_warning(unit.source, &e)
            }
            _ => format!("Failed to write: {e}"),
        };
        sender.send_warning(message).await;
        // `Err` has the caller count this as refused and record nothing, so
        // nothing is recorded as deployed that was not. An entry already in the
        // state keeps its previous checksum and is stale rather than untracked,
        // which is the honest record: a refusal writes nothing, and a failed write
        // leaves the target as it was, so the previous checksum still describes it.
        return Err(());
    }

    // Only now that the overwrite has landed is an earlier copy redundant. Before
    // this point it may be the only record of content the target no longer holds,
    // while this run's copy holds what is still at the target -- so pruning on the
    // way to a write that then fails trades the irreplaceable for a duplicate.
    // Both returns above therefore leave two copies, and the next successful
    // overwrite reduces them to one. Do not delete either on the way out: a delete
    // path fails too, and losing a copy is worse than keeping a redundant one.
    if let Some(kept) = kept
        && let Some(stale) = kept.prune_earlier(filesystem)
    {
        sender.send_warning(stale).await;
    }

    sender
        .send_dotfile_deployed(
            unit.source_path.display(),
            unit.target_path.display(),
            backup.as_deref(),
        )
        .await;
    Ok(())
}

/// Copy the target's content aside, if this overwrite would destroy any.
///
/// `Ok(None)` when there is nothing to keep: nowhere to put a copy, no target,
/// or the target already holds what is about to be written.
///
/// # Errors
///
/// The warning to report, when the target cannot be read or the copy cannot be
/// written. Nothing has been written to the target in either case.
fn keep_current<F: FileSystem>(
    filesystem: &F,
    unit: &DeployUnit<'_>,
) -> Result<Option<backup::Kept>, String> {
    let Some(root) = unit.backups else {
        return Ok(None);
    };

    // Read again here rather than reuse the bytes the deploy decision was made
    // from. An interactive resolver sits at a prompt for as long as the user
    // takes, and the target can change while it waits -- so the earlier read is
    // what the user was shown, and this one is what the write is about to
    // destroy. Copying the first would keep bytes that are still reachable and
    // lose the ones that are not.
    let current = match read_target_state(filesystem, unit.target_path) {
        // Gone since the decision. Nothing to keep, and the write will recreate it.
        TargetState::Absent => return Ok(None),
        TargetState::Readable(bytes) => bytes,
        // Readable when the decision was made and not now. Refusing leaves the
        // target alone, which is the same answer apply gives a target it could
        // not read in the first place, in the same words.
        TargetState::Unreadable(e) => {
            return Err(unreadable_target_refusal(
                filesystem,
                unit.source,
                unit.target_path,
                &e,
            ));
        }
    };

    // Decided from the bytes rather than from the deploy decision. A refresh whose
    // repository file changed is `Deploy` and overwrites differing content with no
    // prompt at all, so gating on an accepted conflict would leave the commonest
    // overwrite uncovered.
    if current == unit.source_content.as_bytes() {
        return Ok(None);
    }

    backup::keep(filesystem, root, unit.target_key, &current)
        .map(Some)
        .map_err(|e| backup::refusal(unit.source, unit.target_path.path(), &e))
}

/// What an apply just recorded about a target.
#[derive(Clone, Copy)]
enum Recorded {
    /// The target was written.
    Deployed,
    /// The target already matched its source and was left alone.
    InSync,
}

impl std::fmt::Display for Recorded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Recorded::Deployed => write!(f, "Deployed"),
            Recorded::InSync => write!(f, "Found in sync"),
        }
    }
}

/// Record `unit` in the loaded state and write the state back. On failure,
/// warns with what happened to the target and returns the reason the run stops.
// A dry run has no loaded state and records nothing. The state is written
// after every record, so a run that cannot write it has recorded everything
// before the failing entry. Stopping on the first failure keeps the
// unrecorded set to one entry: a state directory that refused this write
// refuses the next one too. An unrecorded target is re-evaluated by the next
// run as untracked: one whose content still matches its source is recorded
// silently through the in-sync skip arm, and only one whose source has
// changed since is asked about.
async fn record_and_save<F: FileSystem>(
    filesystem: &F,
    loaded: &mut Option<LoadedState>,
    sender: &EventSender,
    recorded: Recorded,
    unit: &DeployUnit<'_>,
) -> Option<String> {
    let loaded = loaded.as_mut()?;
    loaded
        .state_mut()
        .record_deployment(unit.target_key, unit.source, unit.source_checksum);
    let Err(e) = save_deploy_state(filesystem, loaded) else {
        return None;
    };
    // `e` already names the state file, so the message does not repeat it.
    sender
        .send_warning(format!(
            "{recorded} '{}' but cannot record it: {e}",
            unit.target_path.display()
        ))
        .await;
    Some(format!(
        "Stopped after failing to record '{}' in the deploy state; the next run re-evaluates \
         it once the state can be written",
        unit.target_path.display()
    ))
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
///
/// Applies every package in `packages`. `refused_repository` counts one refusal
/// for a dotfiles repository whose dotfiles were asked for and could not be
/// listed.
async fn handle_apply<F, CR>(
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
        StateLoad::Usable(loaded) => Some(loaded),
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

            // Refuse a symlinked target before anything acts on the decision, so a
            // dry run previews what a real apply would do, an interactive resolver
            // is never asked a question whose answer cannot be honored, and the
            // link destination is never rendered in a diff.
            //
            // `Skip` is excluded: an in-sync target is not written to. Recording one
            // as deployed would let `detect_drift` answer `None` forever for a path
            // selfie will never write (selfie-phnh), so the suppression below covers
            // only the symlinked case. `write_file_no_follow` checks again itself.
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
                    // `DriftType::None`, unless the target is a symlink.
                    //
                    // Gating this on `symlink_refusal` is safe despite its
                    // advisory-and-racy documentation, because nothing is
                    // written here. Where this path does write, `perform_deploy`
                    // relies on `write_file_no_follow`'s own check.
                    //
                    // A stale answer omits an entry the next run re-evaluates.
                    // The window that could manufacture one is small, not absent.
                    if drift == DriftType::NotTracked
                        && !options.dry_run
                        && unmanaged.is_none()
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
                    //
                    // An absent target decides `Deploy`, so a conflict always has
                    // bytes; the default is never reached. Lossy only for
                    // display: the checksum above compared the raw bytes.
                    let target_content =
                        String::from_utf8_lossy(current.as_deref().unwrap_or_default());
                    let diff = unified_diff(
                        &target_content,
                        &source_content,
                        &target_path.display().to_string(),
                        &source_path.to_string_lossy(),
                    );

                    // Determine whether to accept: --yes flag, interactive
                    // resolver, or neither (skip with conflict event).
                    //
                    // A dry run never asks: nothing will be written, so the
                    // question has no answer to honor. It reports the conflict
                    // with the diff a real run would prompt on. With `--yes` the
                    // accept still lands in `perform_deploy`'s dry-run skip.
                    let accept = if options.auto_accept {
                        true
                    } else if !options.dry_run
                        && let Some(resolver) = &options.conflict_resolver
                    {
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

/// Core logic for checking drift
///
/// `unreadable_repository` says a dotfiles repository could not be listed, so
/// `packages` is missing whatever it holds.
async fn handle_check_drift<F>(
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

/// Which of the two specs a track writes into.
///
/// The only difference between the two entry points once their setup is done.
enum SpecKind {
    /// A spec this track creates. A file already at its path refuses the track.
    New,
    /// A spec that already exists and was loaded, whose entries this track's
    /// target may already be one of.
    Existing,
}

/// The spec a track writes, before this track's own entry is added to it.
struct TrackSpec {
    /// Names the spec, and the directory inside the spec's own that the copy
    /// goes in.
    name: String,
    spec_path: PathBuf,
    package: Package,
    kind: SpecKind,
}

/// Handle `track_standalone`: copy the target file into the dotfiles directory,
/// create a new YAML spec, and record initial deploy state.
async fn handle_track_standalone<R, F>(
    name: &str,
    target_path: &str,
    dotfiles_repo: Option<&R>,
    filesystem: &F,
    sender: &EventSender,
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

    // `write_file_no_follow` creates missing parent directories, so this
    // check precedes every target check and every write: tracking into a
    // directory that is not there would otherwise create it, turning a
    // mistyped `dotfiles_directory` into a new directory holding one spec.
    //
    // Asks the repository rather than `filesystem.path_exists`, which reads
    // false for a symlink loop exactly as it does for a missing path and
    // would send this refusal down the "does not exist" branch with a
    // `mkdir -p` hint that cannot work. Listing for `name` is the cheapest
    // repository call that still classifies the directory.
    if let Err(error) = dotfiles_repo.find_package_files(name) {
        return OperationResult::Failure(OperationFailure::Generic(
            super::directory::track_listing_refusal(&dotfiles_dir, error),
        ));
    }

    let spec_path = dotfiles_dir.join(format!("{name}.yml"));

    // Deliberately not `set_source`. `top_level_refusal` answers only for a
    // package carrying stored source text, and a spec built here has none, so
    // `save_package`'s unknown-key guards stay quiet -- which is what lets a
    // freshly built spec save at all. Making the two variants symmetric by
    // storing source here would start refusing every standalone track.
    let package = crate::package::PackageBuilder::default()
        .name(name)
        .path(spec_path.clone())
        .build();

    handle_track(
        TrackSpec {
            name: name.to_string(),
            spec_path,
            package,
            kind: SpecKind::New,
        },
        target_path,
        dotfiles_repo,
        filesystem,
        sender,
        config,
    )
    .await
}

/// Handle `track_for_package`: load an existing package, copy the target file
/// alongside the YAML, add a dotfiles entry, save, and record deploy state.
async fn handle_track_for_package<R, F>(
    package_name: &str,
    target_path: &str,
    repo: &R,
    filesystem: &F,
    sender: &EventSender,
    config: &SelfieConfig,
) -> OperationResult
where
    R: PackageRepository,
    F: FileSystem,
{
    // Carried with its type rather than stringified, as every other
    // single-package path carries it, so an adapter keyed on the typed error
    // reaches this command too. Needs no frame of its own: `PackageError` names
    // the package and the directory it searched.
    let package_blob = match repo.get_package(package_name) {
        Ok(blob) => blob,
        Err(e) => return OperationResult::Failure(e.into()),
    };

    let spec_path = package_blob.file_path().to_path_buf();

    handle_track(
        TrackSpec {
            name: package_name.to_string(),
            spec_path,
            package: package_blob.into_package(),
            kind: SpecKind::Existing,
        },
        target_path,
        repo,
        filesystem,
        sender,
        config,
    )
    .await
}

/// Track `target_path` into `spec`: refuse what cannot be tracked, copy the file
/// into the repository beside the spec, add the entry, save the spec, and record
/// the deployment.
///
/// One body for both entry points, because every check and every write they
/// perform is the same one. [`SpecKind`] carries the single difference.
async fn handle_track<R, F>(
    mut spec: TrackSpec,
    target_path: &str,
    repo: &R,
    filesystem: &F,
    sender: &EventSender,
    config: &SelfieConfig,
) -> OperationResult
where
    R: PackageRepository,
    F: FileSystem,
{
    // Expand the target, or refuse it if selfie could never deploy to it.
    //
    // First of the three refusals, and ahead of `symlink_refusal` for a reason of
    // its own: this one touches no filesystem at all, while `symlink_refusal` and
    // `path_exists` both stat a relative path against the *process working
    // directory* -- which is what made track record entries every later apply
    // refuses (selfie-q9t3). It therefore also sits ahead of all three writes.
    //
    // Ahead of the already-tracked answer below as well: an entry recording a
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

    // An entry for this target already in the spec. Each entry's own target goes
    // through `expand_target_path`, not the rule: this compares a recorded entry
    // rather than writing to it, and a spec may hold one the rule refuses.
    //
    // A `SpecKind::New` spec has no entries, so this answers `None` for one
    // without a branch of its own.
    let already_tracked = spec
        .package
        .dotfiles()
        .iter()
        .find(|entry| expand_target_path(filesystem, entry.target()) == expanded_target);

    if let Some(entry) = already_tracked {
        // Nothing is written here, so a target selfie cannot write to is reported
        // rather than refused -- and it has to be reported here, because this is
        // the one track answer that reaches neither the refusals below nor a
        // deploy. With matching content drift answers `None` and has no line to
        // carry a reason either, so both commands were silent about it.
        // One answer, not both: a symlink to a socket satisfies each check and
        // would otherwise warn twice with the same sentence. `or_else` also skips
        // the second stat when the first already answered.
        if let Some(refusal) = filesystem
            .symlink_refusal(&expanded_target)
            .or_else(|| filesystem.irregular_target_refusal(&expanded_target))
        {
            sender
                .send_warning(already_tracked_refusal_warning(&refusal))
                .await;
        }

        // The entry's own paths, not the argument and not the target: "already
        // tracking X" should name what the spec says, which is what a later apply
        // will use, and `source_path` means the file in the repository in every
        // other arm of this event.
        return OperationResult::Success(OperationSuccess::DotfileTracked {
            name: spec.name,
            source_path: tracked_copy_path(&spec.spec_path, entry),
            target_path: entry.target().to_string(),
            was_already_tracked: true,
            environment: config.environment().to_string(),
            steps_completed: StepCount::new(1, 1),
        });
    }

    // Position is load-bearing at both ends. Before the writes: tracking reads
    // *through* a link, so accepting one copies the destination into the dotfiles
    // directory — where `sync push` commits it — and records a deployment that never
    // happened. Before the existence check: `path_exists` follows the link, so a
    // dangling one would be reported as a missing file.
    //
    // After the already-tracked answer above, because refusing an idempotent
    // no-op helps nobody.
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

    let content = match filesystem.read_file(expanded_target.path()) {
        Ok(c) => c,
        Err(e) => {
            return OperationResult::Failure(OperationFailure::Generic(format!(
                "Cannot read target file: {e}"
            )));
        }
    };

    // Ahead of every write below. Track ends by recording the deployment, and a
    // state file it could not load is one it must not write over, so the copy
    // and the spec are not created for a record that cannot be kept.
    let mut loaded = match load_deploy_state(filesystem, config) {
        StateLoad::Usable(loaded) => loaded,
        StateLoad::Unusable(failure) => {
            return OperationResult::Failure(OperationFailure::Generic(failure.to_string()));
        }
    };

    let filename = expanded_target
        .path()
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    // Only for a spec this track would create. The other kind was loaded from
    // this path, so something being there is what was expected.
    if matches!(spec.kind, SpecKind::New) && filesystem.path_exists(&spec.spec_path) {
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "A dotfile spec already exists at {}. Remove it first or choose a different name.",
            spec.spec_path.display()
        )));
    }

    // The copy goes in a directory named for the spec, beside the spec itself:
    // `dotfiles/bat/config` for `dotfiles/bat.yml`, `packages/bat/config` for
    // `packages/bat.yml`. One formula, because the two entry points compose the
    // same shape from different roots.
    let source_dir = spec
        .spec_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(&spec.name);
    let source_path = source_dir.join(&filename);
    let relative_source = format!("{}/{filename}", spec.name);

    if filesystem.path_exists(&source_path) {
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "Source file already exists at {}. Remove it first, or track a different file.",
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

    let recorded_target = portable_target(filesystem, &expanded_target);
    spec.package
        .add_dotfile(DotfileEntry::new(&relative_source, &recorded_target));

    // `add_dotfile` drops the entry when an existing one carries the same target
    // *string*, while the answer above compares expanded paths. The two agree as
    // long as the recorded form derives from the same expansion, and a
    // disagreement would otherwise save a spec that never names the copy, leave
    // the copy behind, and record a deployment for a source the spec does not
    // contain -- while reporting success. Checked rather than assumed, and
    // compensated exactly as a failed save is.
    if !spec
        .package
        .dotfiles()
        .iter()
        .any(|entry| entry.source() == Some(relative_source.as_str()))
    {
        let removal = remove_own_copy(filesystem, &source_path, &content);
        return OperationResult::Failure(OperationFailure::Generic(unadded_entry_failure(
            &recorded_target,
            &source_path,
            &removal,
        )));
    }

    if let Err(e) = repo.save_package(&spec.package, &spec.spec_path) {
        // Only the file, and only selfie's own. `remove_own_copy` establishes the
        // second by content; the directory is the part selfie leaves, because it
        // cannot tell one it created from one that was already there and an empty
        // directory refuses nothing on a retry.
        let removal = remove_own_copy(filesystem, &source_path, &content);
        return OperationResult::Failure(OperationFailure::Generic(spec_save_failure(
            &e,
            &source_path,
            &removal,
        )));
    }

    let checksum = compute_checksum(content.as_bytes());
    loaded.state_mut().record_deployment(
        &expanded_target.display().to_string(),
        &relative_source,
        &checksum,
    );
    // Last, and nothing is rolled back for it: the copy and the entry are both
    // correct and only the record is missing, so the failure names what exists and
    // what recovers it rather than undoing two good writes.
    if let Err(e) = save_deploy_state(filesystem, &loaded) {
        return OperationResult::Failure(OperationFailure::Generic(unrecorded_track_failure(
            &e,
            &spec.name,
            &recorded_target,
            &spec.spec_path,
            &source_path,
        )));
    }

    // The recorded form, not the argument: an adapter that echoed the caller's
    // path would name a target the spec does not contain.
    OperationResult::Success(OperationSuccess::DotfileTracked {
        name: spec.name,
        source_path,
        target_path: recorded_target,
        was_already_tracked: false,
        environment: config.environment().to_string(),
        steps_completed: StepCount::new(1, 1),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::MockFileSystem;

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

    // The copy is removed only when the path still holds what this call wrote.
    // The guard before the copy is advisory, so ownership has to be established
    // rather than inherited from it: without the comparison, a file another
    // process put at the path between the write and this removal is deleted.
    //
    // `expect_remove_file().never()` is the assertion. A test that only checked
    // the message could not see the syscall, which is the thing that does harm.
    #[test]
    fn a_copy_whose_content_changed_is_not_removed() {
        let copy = Path::new("/dotfiles/gemrc/gemrc");
        let mut fs = MockFileSystem::default();
        fs.mock_read_file(copy, "someone else's file");
        fs.expect_remove_file().never();

        let removal = remove_own_copy(&fs, copy, "gem: --no-document");

        assert!(
            matches!(removal, CopyRemoval::Unconfirmed),
            "a foreign file must not be claimed"
        );
        let message = copy_fate(copy, &removal);
        assert!(
            message.contains("left alone"),
            "the user is not told the copy survived: {message}"
        );
    }

    // The control, and the reason the test above is not vacuous: with the bytes
    // selfie wrote still at the path, the removal happens. Without this, refusing
    // to remove anything at all would pass that test.
    #[test]
    fn a_copy_that_still_holds_what_was_written_is_removed() {
        let copy = Path::new("/dotfiles/gemrc/gemrc");
        let mut fs = MockFileSystem::default();
        fs.mock_read_file(copy, "gem: --no-document");
        fs.mock_remove_file(copy);

        let removal = remove_own_copy(&fs, copy, "gem: --no-document");

        assert!(
            matches!(removal, CopyRemoval::Removed),
            "selfie's own copy must be removed"
        );
        assert!(
            copy_fate(copy, &removal).contains("was removed"),
            "the copy's fate is not stated"
        );
    }

    // A path selfie cannot read back is a path it cannot claim, so it is left
    // rather than removed on the assumption that the read failure is benign.
    #[test]
    fn a_copy_that_cannot_be_read_back_is_not_removed() {
        let copy = Path::new("/dotfiles/gemrc/gemrc");
        let mut fs = MockFileSystem::default();
        fs.expect_read_file().returning(|_| {
            Err(FileSystemError::IoError(std::sync::Arc::new(
                std::io::Error::other("gone"),
            )))
        });
        fs.expect_remove_file().never();

        let removal = remove_own_copy(&fs, copy, "gem: --no-document");

        assert!(matches!(removal, CopyRemoval::Unconfirmed));
    }

    // Exactly once, counted rather than checked for presence. The message leaves
    // naming the spec to the error, so a variant that stops naming it drops the
    // count to zero and fails here rather than shipping a failure that names no
    // file; re-adding a path to the frame takes it to two, which is the
    // duplication this wording exists to avoid.
    #[test]
    fn a_failed_spec_save_names_the_spec_exactly_once() {
        let spec = "/dotfiles/gemrc.yml";
        let error =
            PackageRepoError::FileSystemError(FileSystemError::IoError(std::sync::Arc::new(
                std::io::Error::other(format!("{spec}: Permission denied (os error 13)")),
            )));

        let message = spec_save_failure(
            &error,
            Path::new("/dotfiles/gemrc/gemrc"),
            &CopyRemoval::Removed,
        );

        assert_eq!(
            message.matches(spec).count(),
            1,
            "the spec must be named exactly once: {message}"
        );
        assert!(
            message.contains("/dotfiles/gemrc/gemrc"),
            "the copy is not named: {message}"
        );
        assert!(
            message.contains("was removed"),
            "the copy's fate is not stated: {message}"
        );
    }

    // A copy that survived says so, and says it differently. The two arms are
    // opposite advice -- one needs nothing from the user, the other needs a file
    // deleted before a retry can work -- so a reader must be able to tell them
    // apart.
    #[test]
    fn a_failed_spec_save_says_when_the_copy_survived() {
        let error = PackageRepoError::UnknownDotfileFields {
            path: PathBuf::from("/packages/creds.yml"),
            fields: "dotfiles[0].var".to_string(),
        };
        let removal = FileSystemError::IoError(std::sync::Arc::new(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Permission denied",
        )));

        let survived = spec_save_failure(
            &error,
            Path::new("/packages/creds/token"),
            &CopyRemoval::Failed(removal),
        );
        let removed = spec_save_failure(
            &error,
            Path::new("/packages/creds/token"),
            &CopyRemoval::Removed,
        );

        assert_ne!(
            survived, removed,
            "a copy that survived reads the same as one that was removed"
        );
        assert!(
            !survived.contains("was removed"),
            "a surviving copy is called removed: {survived}"
        );
        assert!(
            survived.contains("Remove it before retrying"),
            "the remedy is missing: {survived}"
        );
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
