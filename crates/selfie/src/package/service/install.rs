//!
//! Helps break down the pieces of running the `package install` command.
//!

use crate::{
    commands::runner::{CommandError, CommandRunner},
    config::SelfieConfig,
    package::{
        EnvironmentConfig,
        event::{
            AfterInstall, CheckResult, CheckResultData, EventSender, Gap, OperationFailure,
            OperationResult, OperationSuccess, after_install,
        },
        port::PackageRepository,
        service::{InstallOptions, ProgressTracker},
    },
};

use tokio_util::sync::CancellationToken;

use std::collections::HashMap;

use super::{
    check,
    deps::{self, Installability},
    steps,
};

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_install<PR, CR>(
    package_name: &str,
    repo: &PR,
    config: &SelfieConfig,
    command_runner: &CR,
    sender: &EventSender,
    progress: &mut ProgressTracker,
    token: &CancellationToken,
    options: &InstallOptions,
) -> OperationResult
where
    PR: PackageRepository + Sync,
    CR: CommandRunner,
{
    // Step 1: Resolve dependencies (includes cycle detection)
    progress
        .next(sender, "Resolving package dependencies")
        .await;

    let dep_graph =
        match deps::resolve_dependencies(package_name, repo, config.environment(), sender).await {
            Ok(graph) => graph,
            Err(failure) => return OperationResult::Failure(*failure),
        };

    // Update total steps: 1 (resolve) + 7 per package (fetch, env, check, get_cmd, execute, verify, complete)
    let num_packages = dep_graph.install_order.len();
    let total_steps = 1 + (7 * num_packages);
    progress.set_total_steps(total_steps);

    let mut confirmed = match confirm_installable(
        &dep_graph,
        config,
        command_runner,
        sender,
        progress,
        token,
    )
    .await
    {
        Ok(confirmed) => confirmed,
        Err(failure) => return OperationResult::Failure(*failure),
    };

    // Install each package in dependency order.
    // The last package is always the root (the one the user requested).
    let mut root = None;
    // Every package's gap, attached once to the root's result: only that result
    // reaches the caller, so a dependency's would otherwise be lost.
    let mut gaps = Vec::new();
    for pkg_name in &dep_graph.install_order {
        // Check for cancellation between packages
        if token.is_cancelled() {
            return OperationResult::Failure("Installation cancelled".into());
        }

        let turn = install_single_package(
            pkg_name,
            repo,
            config,
            command_runner,
            sender,
            progress,
            token,
            confirmed.remove(pkg_name),
            dep_graph.required_by.get(pkg_name).map(String::as_str),
        )
        .await;

        match turn.result {
            Ok(installed) => {
                gaps.extend(turn.gap);
                root = Some((pkg_name.as_str(), installed));
            }
            Err(failure) => return OperationResult::Failure(failure),
        }
    }

    // After the main install succeeds, handle recommends (soft dependencies)
    if !options.skip_recommends {
        install_recommends(
            package_name,
            &dep_graph.root_recommends,
            repo,
            config,
            command_runner,
            sender,
            token,
        )
        .await;
    }

    let (name, installed) = root.unwrap_or((
        package_name,
        Installed {
            was_already_installed: false,
            executable_path: None,
        },
    ));
    OperationResult::Success(OperationSuccess::package_installed(
        name.to_string(),
        config.environment().to_string(),
        installed.was_already_installed,
        installed.executable_path,
        gaps,
        (progress.current_step(), progress.total_steps()).into(),
    ))
}

/// A package that a turn installed, or found already installed.
#[derive(Debug, Clone)]
struct Installed {
    was_already_installed: bool,
    executable_path: Option<String>,
}

/// What one package's turn ended with: installed, with the gap its check left
/// when it gave no answer, or the failure to end the run with.
#[derive(Debug, Clone)]
struct Turn {
    result: Result<Installed, OperationFailure>,
    gap: Option<Gap>,
}

impl Turn {
    fn installed(installed: Installed, gap: Option<Gap>) -> Self {
        Self {
            result: Ok(installed),
            gap,
        }
    }

    fn ended(failure: OperationFailure) -> Self {
        Self {
            result: Err(failure),
            gap: None,
        }
    }
}

/// Refuse a package in `graph` that cannot be installed here, before any install
/// command runs, and run the check of each package that can be installed only by
/// already being installed.
///
/// Returns those checks' results, by package, so each package's turn reports it
/// installed without running its check again.
///
/// # Errors
///
/// The first refusal in install order, or the refusal of a package whose check
/// does not say it is installed, naming that package.
async fn confirm_installable<CR: CommandRunner>(
    graph: &deps::DependencyGraph,
    config: &SelfieConfig,
    command_runner: &CR,
    sender: &EventSender,
    progress: &mut ProgressTracker,
    token: &CancellationToken,
) -> Result<HashMap<String, CheckResultData>, Box<OperationFailure>> {
    let verdict = |name: &String| graph.installability.get(name);
    if let Some(Installability::Refused(failure)) = graph
        .install_order
        .iter()
        .filter_map(verdict)
        .find(|verdict| matches!(verdict, Installability::Refused(_)))
    {
        return Err(failure.clone());
    }

    // The check is the step the package's own turn would take first, taken
    // earlier rather than added, so the step count is unchanged.
    let mut confirmed = HashMap::new();
    for name in &graph.install_order {
        let Some(Installability::CheckFirst(check, otherwise)) = verdict(name) else {
            continue;
        };
        let result = check::execute_check_command(
            name,
            config.environment(),
            Some(check),
            config.package_directory(),
            command_runner,
            sender,
            progress,
            &format!("Checking whether {name} is already installed"),
            token,
        )
        .await
        .map_err(|err| Box::new(OperationFailure::from(err)))?;
        // A check that ran and said "not installed" leaves no way to install the
        // package. One that could not answer says why, so the user is not sent to
        // write an install command when the fix is elsewhere.
        match &result.result {
            CheckResult::Success { .. } => {}
            CheckResult::CouldNotRun(failure) => {
                return Err(Box::new(OperationFailure::from(failure)));
            }
            CheckResult::Failed { .. } | CheckResult::NoCheckCommand => {
                return Err(otherwise.clone());
            }
        }
        confirmed.insert(name.clone(), result);
    }
    Ok(confirmed)
}

/// Install a single package (without dependency resolution).
///
/// `pre_checked` is the result of the package's check when it already ran, in
/// which case it is not run again.
#[allow(clippy::too_many_arguments)]
async fn install_single_package<PR, CR>(
    package_name: &str,
    repo: &PR,
    config: &SelfieConfig,
    command_runner: &CR,
    sender: &EventSender,
    progress: &mut ProgressTracker,
    token: &CancellationToken,
    pre_checked: Option<CheckResultData>,
    required_by: Option<&str>,
) -> Turn
where
    PR: PackageRepository,
    CR: CommandRunner,
{
    // Fetch package
    let package_blob = match steps::fetch_package(repo, package_name, sender, progress).await {
        Ok(pkg) => pkg,
        Err(err) => {
            return Turn::ended(err.into());
        }
    };

    // Find environment configuration
    let env_config = match get_environment_config(
        package_name,
        &package_blob,
        config.environment(),
        sender,
        progress,
    )
    .await
    {
        Ok(config) => config,
        Err(failure) => return Turn::ended(*failure),
    };

    // Check if package is already installed. A check that could not start,
    // because nothing can start in the package directory or the operation was
    // cancelled, ends the install here, rather than as a warning followed by
    // the same failure from the install.
    let pre_install_check = match pre_checked {
        Some(check) => check,
        None => match check::execute_check_command(
            package_name,
            config.environment(),
            env_config.check(),
            config.package_directory(),
            command_runner,
            sender,
            progress,
            &format!("Checking whether {package_name} is already installed"),
            token,
        )
        .await
        {
            Ok(check) => check,
            Err(err) => return Turn::ended(err.into()),
        },
    };

    // If package is already installed, exit early
    if let Some(installed) = handle_already_installed_package(
        package_name,
        &pre_install_check,
        command_runner,
        sender,
        progress,
        config,
        token,
    )
    .await
    {
        return Turn::installed(installed, None);
    }

    // Log that we're proceeding with installation
    log_proceeding_with_installation(package_name, &pre_install_check, sender).await;

    // Get install command
    let Ok(install_cmd) = steps::get_command(
        env_config,
        "install",
        EnvironmentConfig::install,
        sender,
        progress,
    )
    .await
    else {
        // Resolution refuses a blank install before any package installs, so this
        // is reached only if the spec changed in between.
        return Turn::ended(OperationFailure::no_install_command(
            package_name.to_string(),
            config.environment().to_string(),
            package_blob.package.path().clone(),
            Vec::new(),
            None,
        ));
    };

    // Execute installation and verification
    let context = InstallationContext {
        package_name,
        install_cmd,
        env_config,
        config,
        pre_install_check: &pre_install_check,
        post_install_note: package_blob.package.post_install_note(),
        required_by,
    };

    execute_installation_and_verification(context, command_runner, sender, progress, token).await
}

async fn handle_already_installed_package<CR>(
    package_name: &str,
    pre_install_check: &CheckResultData,
    command_runner: &CR,
    sender: &EventSender,
    progress: &mut ProgressTracker,
    config: &SelfieConfig,
    token: &CancellationToken,
) -> Option<Installed>
where
    CR: CommandRunner,
{
    if matches!(pre_install_check.result, CheckResult::Success { .. }) {
        sender
            .send_debug(format!("Package '{package_name}' is already installed"))
            .await;

        // Reduce total steps by 4 (get_cmd, execute, verify, complete) since
        // we're skipping the actual installation for this package.
        progress.reduce_total_steps(4);

        let executable_path = find_executable_path(
            package_name,
            config.package_directory(),
            command_runner,
            sender,
            token,
        )
        .await;

        return Some(Installed {
            was_already_installed: true,
            executable_path,
        });
    }
    None
}

async fn find_executable_path<CR>(
    package_name: &str,
    package_dir: &std::path::Path,
    command_runner: &CR,
    sender: &EventSender,
    token: &CancellationToken,
) -> Option<String>
where
    CR: CommandRunner,
{
    let finder_command = format!(
        "which {}",
        shlex::try_quote(package_name).unwrap_or(package_name.into())
    );

    // In the package directory, which the install has just run in. The runner
    // has no way to run a command where selfie was started without entering it
    // again, and selfie's own directory may be one it cannot enter.
    match command_runner
        .execute(&finder_command, package_dir, token)
        .await
    {
        Ok(output) if output.is_success() && !output.stdout_str().trim().is_empty() => {
            let executable_path = output.stdout_str().trim().to_string();
            sender
                .send_debug(format!("Found executable at: {executable_path}"))
                .await;
            Some(executable_path.to_string())
        }
        Ok(_) => {
            sender
                .send_debug(format!(
                    "No executable named '{package_name}' found in PATH"
                ))
                .await;
            None
        }
        // No path rather than a guessed one. This covers the command failing to
        // run *and* its output failing to read part-way through — the latter
        // would otherwise hand back a truncated path that either fails to
        // resolve or, worse, names a different binary than the command found.
        // Rendered with `Display`: `Debug` would print whatever fields a future
        // `CommandError` variant adds.
        Err(err) => {
            sender
                .send_debug(format!(
                    "Could not determine the path of '{package_name}': {err}"
                ))
                .await;
            None
        }
    }
}

async fn log_proceeding_with_installation(
    package_name: &str,
    pre_install_check: &CheckResultData,
    sender: &EventSender,
) {
    match pre_install_check.result {
        CheckResult::Failed { .. } => {
            sender
                .send_debug(format!(
                    "Package '{package_name}' is not installed, proceeding with installation"
                ))
                .await;
        }
        CheckResult::NoCheckCommand => {
            sender
                .send_debug("No check command defined, proceeding with installation")
                .await;
        }
        CheckResult::CouldNotRun(_) => {
            sender
                .send_warning("Check command failed, but proceeding with installation anyway")
                .await;
        }
        CheckResult::Success { .. } => {
            // Already handled in handle_already_installed_package
        }
    }
}

struct InstallationContext<'a> {
    package_name: &'a str,
    install_cmd: &'a str,
    env_config: &'a EnvironmentConfig,
    config: &'a SelfieConfig,
    pre_install_check: &'a CheckResultData,
    post_install_note: Option<&'a str>,
    required_by: Option<&'a str>,
}

async fn execute_installation_and_verification<CR>(
    context: InstallationContext<'_>,
    command_runner: &CR,
    sender: &EventSender,
    progress: &mut ProgressTracker,
    token: &CancellationToken,
) -> Turn
where
    CR: CommandRunner,
{
    // Execute install command with streaming output
    let install_output = match steps::execute_command_streaming(
        command_runner,
        context.package_name,
        context.install_cmd,
        "install",
        context.config,
        sender,
        progress,
        token,
    )
    .await
    {
        Ok(output) => output,
        Err(err) => return Turn::ended(err.into()),
    };

    if !install_output.is_success() {
        sender
            .send_warning(format!(
                "Package '{}' installation command failed",
                context.package_name
            ))
            .await;
        // The install command's stdout has already been streamed to the caller
        // line by line as it ran; it is deliberately not repeated here.
        return Turn::ended(OperationFailure::command_failed(
            context.install_cmd.to_string(),
            Some(install_output.exit_code()),
            install_output.stderr_str().as_ref(),
        ));
    }

    // The post-install check decides the install by ADR 0010's table: one that
    // says not installed, or that never ran, fails it; one that started and gave
    // no answer leaves it unconfirmed, a gap. A check that could not start in
    // the package directory, or was canceled, arrives as the error instead, and
    // keeps that error's own failure and wording.
    let gap = match verify_installation(&context, command_runner, sender, progress, token).await {
        Ok(AfterInstall::Confirmed) => None,
        Ok(AfterInstall::Unconfirmed(gap)) => Some(gap),
        Ok(AfterInstall::NotInstalled { exit_code }) => {
            return Turn::ended(OperationFailure::NotInstalledAfterInstall {
                package_name: context.package_name.to_string(),
                check_command: context.env_config.check().unwrap_or_default().to_string(),
                exit_code,
                required_by: context.required_by.map(str::to_string),
            });
        }
        Ok(AfterInstall::Failed(failure)) => {
            return Turn::ended(OperationFailure::from(&failure));
        }
        Err(err) => return Turn::ended(err.into()),
    };

    let executable_path = find_executable_path(
        context.package_name,
        context.config.package_directory(),
        command_runner,
        sender,
        token,
    )
    .await;

    // Emit post-install note if this was a fresh install and the package has one
    if !matches!(
        context.pre_install_check.result,
        CheckResult::Success { .. }
    ) && let Some(note) = context.post_install_note
    {
        sender
            .send_post_install_note(context.package_name, note)
            .await;
    }

    // Final step: Report success
    progress
        .next(sender, "Package installation completed")
        .await;

    Turn::installed(
        Installed {
            was_already_installed: false,
            executable_path,
        },
        gap,
    )
}

async fn get_environment_config<'a>(
    package_name: &str,
    package_blob: &'a crate::package::GetPackage,
    current_env: &str,
    sender: &EventSender,
    progress: &mut ProgressTracker,
) -> Result<&'a EnvironmentConfig, Box<OperationFailure>> {
    progress.next(sender, "Checking package environment").await;

    // Get environment configuration
    let Some(env_config) = package_blob.package.environments().get(current_env) else {
        return Err(Box::new(
            steps::missing_environment(package_name, package_blob, current_env).into(),
        ));
    };

    sender
        .send_debug(format!(
            "Found environment configuration for '{current_env}'"
        ))
        .await;
    Ok(env_config)
}

/// Run the post-install check and report its verdict.
///
/// # Errors
///
/// The [`CommandError`] that kept the check from starting when nothing can run
/// in the package directory: it cannot be entered, the shell cannot start, or
/// the install was cancelled.
async fn verify_installation<CR>(
    context: &InstallationContext<'_>,
    command_runner: &CR,
    sender: &EventSender,
    progress: &mut ProgressTracker,
    token: &CancellationToken,
) -> Result<AfterInstall, CommandError>
where
    CR: CommandRunner,
{
    if context.pre_install_check.check_command.is_some() {
        let post_install_check = check::execute_check_command(
            context.package_name,
            context.config.environment(),
            context.env_config.check(),
            context.config.package_directory(),
            command_runner,
            sender,
            progress,
            &format!("Verifying the installation of {}", context.package_name),
            token,
        )
        .await?;

        return Ok(after_install(
            &post_install_check.result,
            context.package_name,
        ));
    } else {
        progress
            .next(
                sender,
                "Skipping installation verification (no check command)",
            )
            .await;
    }
    Ok(AfterInstall::Confirmed)
}

/// Install recommended (soft) dependencies for a package.
///
/// Recommends are one-level deep only — we do NOT follow recommends of recommends.
/// Each recommend's hard dependencies ARE resolved and installed.
/// Failures are emitted as `RecommendFailed` events but never propagate to the parent result.
/// When a cancel leaves recommends untried, one `RecommendsUntried` event names
/// them all.
///
/// `recommends` must be the root package's list for `config.environment()`, as
/// `DependencyGraph::root_recommends` holds it.
async fn install_recommends<PR, CR>(
    package_name: &str,
    recommends: &[String],
    repo: &PR,
    config: &SelfieConfig,
    command_runner: &CR,
    sender: &EventSender,
    token: &CancellationToken,
) where
    PR: PackageRepository + Sync,
    CR: CommandRunner,
{
    if recommends.is_empty() {
        return;
    }

    sender
        .send_debug(format!(
            "Package '{package_name}' recommends: {recommends:?}"
        ))
        .await;

    // Install recommends concurrently in chunks, bounded by max_concurrency.
    // Uses chunks+join_all (not semaphore+spawn) because the function takes
    // borrowed references that can't move into 'static tokio tasks.
    let max_concurrent = config.max_concurrency().get();

    // Once canceled, every recommend not yet started returns at once without
    // trying, in this chunk and every later one. All of them are named, so a
    // canceled install never reads as one that tried every recommend.
    let mut untried: Vec<String> = Vec::new();
    for chunk in recommends.chunks(max_concurrent) {
        let futures: Vec<_> = chunk
            .iter()
            .map(|name| {
                install_recommend_in_bulk(name, repo, config, command_runner, sender, token)
            })
            .collect();

        let started = futures::future::join_all(futures).await;
        untried.extend(
            chunk
                .iter()
                .zip(started)
                .filter(|(_, started)| !started)
                .map(|(name, _)| name.clone()),
        );
    }

    if !untried.is_empty() {
        sender.send_recommends_untried(untried).await;
    }
}

/// Install a single recommend within a bulk operation, and say whether it was
/// tried: `false` when a cancel came before it started, or before it installed
/// the next of its packages.
///
/// A tried recommend emits started, then succeeded or failed; one the cancel
/// stopped part way emits only started. Failures are reported but never
/// propagate — recommends are soft dependencies.
async fn install_recommend_in_bulk<PR, CR>(
    recommend_name: &str,
    repo: &PR,
    config: &SelfieConfig,
    command_runner: &CR,
    sender: &EventSender,
    token: &CancellationToken,
) -> bool
where
    PR: PackageRepository + Sync,
    CR: CommandRunner,
{
    if token.is_cancelled() {
        return false;
    }

    sender.send_recommend_started(recommend_name).await;

    // Each recommend gets its own progress tracker (7 steps per package)
    let mut rec_progress = ProgressTracker::new(7);

    match install_single_recommend(
        recommend_name,
        repo,
        config,
        command_runner,
        sender,
        &mut rec_progress,
        token,
    )
    .await
    {
        Ok(()) => {
            sender.send_recommend_succeeded(recommend_name).await;
        }
        // Stopped between its packages, so nothing of it failed: it is named
        // with the untried ones.
        Err(RecommendError::Interrupted) => return false,
        // Everything else is a failure, decided by what happened and never by
        // the token: a command that failed, one the cancel interrupted, and one
        // the cancel stopped just before it spawned, which the runner reports
        // the same way as an interrupted one.
        Err(RecommendError::Failed(error)) => {
            sender.send_recommend_failed(recommend_name, &error).await;
        }
    }
    true
}

/// Why a recommend was not installed.
enum RecommendError {
    /// A cancel came before it installed the next of its packages.
    Interrupted,
    /// Resolving or installing one of its packages failed.
    Failed(String),
}

/// Try to install a single recommended package (with its hard dependencies).
async fn install_single_recommend<PR, CR>(
    recommend_name: &str,
    repo: &PR,
    config: &SelfieConfig,
    command_runner: &CR,
    sender: &EventSender,
    progress: &mut ProgressTracker,
    token: &CancellationToken,
) -> Result<(), RecommendError>
where
    PR: PackageRepository + Sync,
    CR: CommandRunner,
{
    // Resolve hard dependencies for this recommend
    let dep_graph = deps::resolve_dependencies(recommend_name, repo, config.environment(), sender)
        .await
        .map_err(|f| RecommendError::Failed(f.to_string()))?;
    // A recommend that cannot be installed is refused before its dependencies
    // install, exactly as the package that recommends it would be.
    let mut confirmed =
        confirm_installable(&dep_graph, config, command_runner, sender, progress, token)
            .await
            .map_err(|f| RecommendError::Failed(f.to_string()))?;

    // Install each package in dependency order
    for pkg_name in &dep_graph.install_order {
        if token.is_cancelled() {
            return Err(RecommendError::Interrupted);
        }

        let turn = install_single_package(
            pkg_name,
            repo,
            config,
            command_runner,
            sender,
            progress,
            token,
            confirmed.remove(pkg_name),
            dep_graph.required_by.get(pkg_name).map(String::as_str),
        )
        .await;

        if let Err(failure) = turn.result {
            return Err(RecommendError::Failed(failure.to_string()));
        }
        // A recommend leaves the run clean whatever happens to it, so a check
        // that gave no answer for it, or for one of its own dependencies, is a
        // warning, never one of the run's gaps.
        if let Some(gap) = turn.gap {
            let whose = if gap.package_name() == recommend_name {
                format!("Recommended package '{recommend_name}'")
            } else {
                format!(
                    "'{}', a dependency of recommended package '{recommend_name}',",
                    gap.package_name()
                )
            };
            sender
                .send_warning(format!(
                    "{whose} installed, but its check gave no answer: {}",
                    gap.reason()
                ))
                .await;
        }
    }

    Ok(())
}
