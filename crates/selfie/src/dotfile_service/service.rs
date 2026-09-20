//! The [`DotfileService`] port's adapter.
//!
//! Holds [`DotfileServiceImpl`], which collects the packages an operation covers
//! from the package repository and the standalone dotfiles directory, then hands
//! each operation to its handler.

use std::path::{Path, PathBuf};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    commands::CommandRunner,
    config::SelfieConfig,
    dotfile_service::deploy::compute_checksum,
    fs::{
        filesystem::{FileSystem, FileSystemError},
        target::{
            TargetRejection, deploy_target, expand_target_path, portable_target, repository_path,
        },
    },
    package::{
        DotfileEntry, Package,
        event::metadata::OperationType,
        event::{
            EventSender, EventStream, OperationContext, OperationFailure, OperationResult,
            OperationSuccess, PackageEvent, StepCount,
        },
        port::{PackageRepoError, PackageRepository},
    },
    privilege::{Privilege, SudoPolicy, SudoRefusal, WriteScope},
};

use super::apply::{ApplyContext, handle_apply};
use super::drift::handle_check_drift;
use super::port::{ApplyOptions, DotfileService};
use super::state_file::{StateLoad, StateSaveError, load_deploy_state, save_deploy_state};

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
