//! The [`DotfileService`] port's adapter.
//!
//! Holds [`DotfileServiceImpl`], which collects the packages an operation covers
//! from the package repository and the standalone dotfiles directory, then hands
//! each operation to its handler.

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    commands::CommandRunner,
    config::SelfieConfig,
    fs::filesystem::FileSystem,
    package::{
        Package,
        event::metadata::OperationType,
        event::{
            EventSender, EventStream, OperationContext, OperationFailure, OperationResult,
            OperationSuccess, PackageEvent,
        },
        port::PackageRepository,
    },
    privilege::{Privilege, SudoPolicy, SudoRefusal, WriteScope},
};

use super::apply::{ApplyContext, handle_apply};
use super::drift::handle_check_drift;
use super::port::{ApplyOptions, DotfileService};
use super::track::{handle_track_for_package, handle_track_standalone};
use super::warning::{ApplyWarning, NameCollision, no_such_package};

/// Whether `package` is the one `folded_name`, already lowercased, names.
///
/// A package is named by its spec file, with case folded, as package lookup
/// resolves a name. The YAML `name:` field does not decide it.
fn is_named(package: &Package, folded_name: &str) -> bool {
    package
        .spec_name()
        .is_some_and(|spec_name| spec_name == folded_name)
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
        filesystem: &F,
        dotfiles_directory: &std::path::Path,
        dotfiles_directory_is_expected: bool,
    ) -> Result<(Vec<Package>, Vec<ApplyWarning>), crate::package::port::PackageListError> {
        Self::collect_packages(
            package_repo,
            dotfiles_repo,
            NameCollision::PackagesWin,
            filesystem,
            dotfiles_directory,
            dotfiles_directory_is_expected,
        )
    }

    /// Collect from both repositories, deciding what a name in both means.
    ///
    /// Deploying has to choose one, because two packages cannot both own a name.
    /// Listing must not: both files exist, and a caller asking what is on disk is
    /// asking about the files rather than about what would win.
    ///
    /// `dotfiles_directory_is_expected` decides whether a dotfiles directory that is
    /// **not there** is worth a warning, and nothing else. A directory that could not
    /// be read or classified is refused either way.
    fn collect_packages(
        package_repo: &R,
        dotfiles_repo: Option<&R>,
        collision: NameCollision,
        filesystem: &F,
        dotfiles_directory: &std::path::Path,
        dotfiles_directory_is_expected: bool,
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
                Err(error) => match super::directory::UnlistedDotfilesDirectory::classify(
                    filesystem,
                    dotfiles_directory,
                    error,
                    dotfiles_directory_is_expected,
                ) {
                    super::directory::UnlistedDotfilesDirectory::OrdinarilyAbsent => {}
                    super::directory::UnlistedDotfilesDirectory::Absent { path, reason } => {
                        warnings.push(ApplyWarning::AbsentDotfilesDirectory { path, reason });
                    }
                    // A directory selfie could not read and a path it could not
                    // classify are both unknown quantities, so both refuse the run
                    // rather than letting it report a completeness it cannot claim.
                    super::directory::UnlistedDotfilesDirectory::Unlistable(error)
                    | super::directory::UnlistedDotfilesDirectory::Unknown(error) => {
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
                &self.filesystem,
                &self.config.dotfiles_directory(),
                self.config.dotfiles_directory_is_expected(),
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
            &self.filesystem,
            &self.config.dotfiles_directory(),
            self.config.dotfiles_directory_is_expected(),
        );
        let fs = self.filesystem.clone();
        let config = self.config.clone();
        // The caller's live token, cloned as `apply_matching` clones it. A fresh
        // one here would leave Ctrl+C with nothing to reach.
        let token = self.cancellation_token.clone();

        Self::create_event_stream(move |tx| async move {
            let sender = EventSender::new_with_context(
                tx,
                OperationType::DotfileDrift,
                String::new(),
                config.environment().to_string(),
                OperationContext::default(),
            );

            sender.send_started().await;

            let outcome = match collected {
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
                        &token,
                        unreadable_repository,
                        unloaded_specs,
                    )
                    .await
                }
                Err(e) => Some(OperationResult::Failure(
                    crate::package::event::OperationFailure::PackageList(e),
                )),
            };

            // The handler says whether it stopped part way; the token is not asked
            // again here. A run whose last entry completed is a whole answer even if
            // the token was cancelled after it, and a collection failure is a
            // failure however the token stands.
            match outcome {
                Some(result) => sender.send_completed(result).await,
                None => sender.send_canceled("Drift check cancelled").await,
            }
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
            &self.filesystem,
            &self.config.dotfiles_directory(),
            self.config.dotfiles_directory_is_expected(),
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
