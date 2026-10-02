//! Common utilities for package commands
//!
//! This module provides shared functionality used across multiple package commands
//! to reduce code duplication and maintain consistency.

use comfy_table::{ContentArrangement, Table, presets};
use console::style;

use selfie::{
    commands::ShellCommandRunner,
    dotfile_service::{port::DotfileService, service::DotfileServiceImpl},
    fs::{filesystem::FileSystem, real::RealFileSystem},
    git::GixGitAdapter,
    package::{
        SpecOrigin, SpecService,
        event::PackageEvent,
        git_adapter::GixGitStatusProvider,
        port::{PackageListError, PackageRepository},
        repository::yaml::YamlPackageRepository,
        service::{PackageService, PackageServiceImpl},
    },
    privilege::{RealPrivilege, SudoPolicy, WriteScope},
    sync_service::{SyncService, service::SyncServiceImpl},
};
use tokio_util::sync::CancellationToken;

use crate::{config::CliConfig, event_processor::EventProcessor};
use std::{path::Path, process::Command};

use crate::display_manager::{DisplayManager, INDENT};

/// Create a package repository instance with the configured package directory
pub(crate) fn create_package_repository(
    config: &CliConfig,
) -> YamlPackageRepository<RealFileSystem> {
    create_package_repository_with_fs(config, RealFileSystem)
}

/// Create a package repository with a specific filesystem implementation
/// This is useful for testing with `MockFileSystem`
pub(crate) fn create_package_repository_with_fs<F: FileSystem>(
    config: &CliConfig,
    fs: F,
) -> YamlPackageRepository<F> {
    YamlPackageRepository::new(
        fs,
        config.package_directory().clone(),
        SpecOrigin::PackageDirectory,
    )
}

/// The names of the packages that loaded, sorted, along with a warning for every
/// spec file that did not.
///
/// Each name comes from [`file_name_of`], the name selfie looks the package up
/// by; a package with no spec file name is left out.
///
/// # Errors
///
/// [`PackageListError`] if the package directory itself cannot be listed. The
/// caller decides what that means: one command aborts on it, another treats a
/// directory that is not there yet as holding no candidates.
// Shared by the two interactive pickers -- `spec create`'s dependency list and
// `track`'s destination list -- so neither can quietly stop naming the files it
// left out. The warnings are returned rather than printed so they can be
// asserted: `print_warning` goes to stderr, which a unit test cannot observe.
pub(crate) fn package_names_and_skipped(
    repo: &impl PackageRepository,
) -> Result<(Vec<String>, Vec<String>), PackageListError> {
    let output = repo.list_packages()?;

    let skipped = output
        .invalid_packages()
        .map(selfie::package::service::skipped_spec_warning)
        .collect();

    // Not the `name:` field: a dependency or a track destination is resolved by
    // file name, and a field that disagrees with it names nothing selfie finds.
    let mut names: Vec<String> = output.valid_packages().filter_map(file_name_of).collect();
    names.sort();
    names.dedup();

    Ok((names, skipped))
}

/// The name `package` is offered and reported under: its spec file's stem as
/// spelled, which lookup resolves ignoring case. `None` for a package with no
/// spec file name.
pub(crate) fn file_name_of(package: &selfie::package::Package) -> Option<String> {
    // `spec_name` decides whether the file names a spec at all; its answer is
    // folded to lower case, so the stem is taken as the file spells it.
    package.spec_name()?;
    package
        .path()
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
}

/// Build the command runner every CLI service uses.
///
/// The one place the CLI picks a shell, so every command runs a user's commands
/// the same way.
// `clippy.toml` makes a non-login runner a build error crate-wide, including
// inside this function.
fn create_command_runner(config: &CliConfig) -> ShellCommandRunner {
    ShellCommandRunner::login_shell(config.command_timeout())
}

/// The standalone dotfiles repository for the configured `dotfiles_directory`.
///
/// Built whether or not the directory exists. The dotfile service decides what
/// a missing or unreadable directory means for each operation.
pub(crate) fn create_dotfiles_repository(
    config: &CliConfig,
) -> YamlPackageRepository<RealFileSystem> {
    YamlPackageRepository::new(
        RealFileSystem,
        config.selfie_config().dotfiles_directory(),
        SpecOrigin::DotfilesDirectory,
    )
}

/// Create a `DotfileServiceImpl` over the package repository and the standalone
/// dotfiles repository.
///
/// This is the standard setup for any command that needs `DotfileService`. The
/// service reports a configured dotfiles directory that does not exist.
pub(crate) fn create_dotfile_service(
    config: &CliConfig,
    cancellation_token: CancellationToken,
) -> DotfileServiceImpl<
    YamlPackageRepository<RealFileSystem>,
    RealFileSystem,
    ShellCommandRunner,
    RealPrivilege,
> {
    DotfileServiceImpl::new(
        create_package_repository(config),
        create_dotfiles_repository(config),
        RealFileSystem,
        create_command_runner(config),
        config.selfie_config().clone(),
        cancellation_token,
        sudo_policy(config),
    )
}

/// Create a `SyncServiceImpl` with `GixGitAdapter` and `DotfileService`.
///
/// This is the standard setup for any command that needs `SyncService`.
pub(crate) fn create_sync_service(
    config: &CliConfig,
    cancellation_token: CancellationToken,
) -> impl SyncService {
    let git = GixGitAdapter;
    let dotfile_service = create_dotfile_service(config, cancellation_token);
    // Its own policy rather than one read back out of `dotfile_service`: sync
    // commits and pushes as root even though it deploys nothing, and root-owned
    // git objects in a user-owned repository do not self-heal.
    SyncServiceImpl::new(
        git,
        dotfile_service,
        config.selfie_config().clone(),
        sudo_policy(config),
    )
}

/// The sudo refusal every service that writes is built with.
///
/// One function so the CLI cannot hand `--allow-sudo` to one service and forget
/// the other. The refusal itself lives in the library and holds for any caller;
/// what is true only by convention is that every CLI write path is built through
/// the two constructors above. Nothing enforces that — `apply` once built its own
/// service — so do not read it as a guarantee.
fn sudo_policy(config: &CliConfig) -> SudoPolicy<RealPrivilege> {
    let policy = SudoPolicy::new(RealPrivilege);
    if config.allow_sudo() {
        policy.allowing_sudo()
    } else {
        policy
    }
}

/// Report the sudo refusal before a handler does any work of its own.
///
/// **Not the gate.** The gate is in the library and refuses whatever the CLI
/// does; this is an early exit so a handler does not do visible work it is about
/// to throw away. Both read the same [`sudo_policy`], so they cannot disagree
/// about whether to refuse — only about how far the run got first.
///
/// Both track helpers call this before building the dotfile service, which skips
/// the "Started" message the service would otherwise print ahead of its own
/// refusal. `selfie track` calls it before its interactive prompts as well,
/// whose answers a refusal afterward would discard.
pub(crate) fn refuse_under_sudo(config: &CliConfig, display: &DisplayManager) -> Option<i32> {
    let refusal = sudo_policy(config).refusal(WriteScope::Dotfiles)?;
    display.print_error(refusal.message());
    display.print_suggestion(refusal.suggestion());
    Some(1)
}

/// The message for a name the namespace check refused, for a command tracking a
/// dotfile under that name.
pub(crate) fn name_check_message(
    name: &str,
    error: &selfie::namespace::NamespaceValidationError,
) -> String {
    use selfie::namespace::NamespaceValidationError as Invalid;

    match error {
        // The fact, then what a track can do about it.
        Invalid::Conflict(conflict) => match conflict.found_in {
            selfie::namespace::NameLocation::Dotfiles => {
                format!("{conflict} Remove it first or choose a different name.")
            }
            selfie::namespace::NameLocation::Packages => format!(
                "{conflict} To track a file for that package, use 'selfie package track-dotfile \
                 {name} <file>', or choose a different name."
            ),
        },
        // A package or dotfiles directory that would not read says nothing about the
        // name, and telling the user they cannot use it sends them off to pick another
        // one, which fails in exactly the same way.
        Invalid::PackageDirectoryUnreadable(_) | Invalid::DotfilesDirectoryUnreadable(_) => {
            error.to_string()
        }
    }
}

/// Track a standalone dotfile via `DotfileServiceImpl::track_standalone`.
///
/// Shared by `selfie dotfiles track` and `selfie track` (interactive).
pub(crate) async fn handle_track_standalone(
    name: &str,
    file: &str,
    config: &CliConfig,
    display: &DisplayManager,
    cancellation_token: CancellationToken,
) -> i32 {
    // Ahead of building the service, so a run under sudo prints only the
    // refusal, not the "Started" message the service would emit first.
    if let Some(code) = refuse_under_sudo(config, display) {
        return code;
    }

    let service = create_dotfile_service(config, cancellation_token);
    let event_stream = service.track_standalone(name, file).await;

    let processor = EventProcessor::new(display.clone());
    let display_for_handler = display.clone();
    let result = processor
        .process_events(event_stream, move |event| {
            handle_already_tracked(event, &display_for_handler)
        })
        .await;
    result.exit_code
}

/// Track a file for an existing package via `DotfileServiceImpl::track_for_package`.
///
/// Shared by `selfie package track-dotfile` and `selfie track` (interactive).
pub(crate) async fn handle_track_for_package(
    package_name: &str,
    file: &str,
    config: &CliConfig,
    display: &DisplayManager,
    cancellation_token: CancellationToken,
) -> i32 {
    // Ahead of building the service, as `handle_track_standalone` does, so a run
    // under sudo prints only the refusal and not the "Started" message the
    // service emits before its own.
    if let Some(code) = refuse_under_sudo(config, display) {
        return code;
    }

    let service = create_dotfile_service(config, cancellation_token);
    let event_stream = service.track_for_package(package_name, file).await;

    let processor = EventProcessor::new(display.clone());
    let display_for_handler = display.clone();
    let result = processor
        .process_events(event_stream, move |event| {
            handle_already_tracked(event, &display_for_handler)
        })
        .await;
    result.exit_code
}

/// Custom event handler that renders already-tracked results as info (ℹ) instead
/// of success (✓), since no work was performed.
fn handle_already_tracked(event: &PackageEvent, display: &DisplayManager) -> bool {
    use selfie::package::event::{OperationResult, OperationSuccess};

    match event {
        PackageEvent::Completed {
            result:
                OperationResult::Success(
                    success @ OperationSuccess::DotfileTracked {
                        was_already_tracked: true,
                        ..
                    },
                ),
            ..
        } => {
            display.print_info(success.to_string());
            true
        }
        _ => false,
    }
}

/// Open a file in the user's preferred editor
///
/// Handles common editor functionality including:
/// - Checking for EDITOR environment variable
/// - Adding --wait flag for VS Code
/// - Executing the editor command
/// - Providing appropriate success/failure messages
pub(crate) fn open_editor(
    file_path: &Path,
    display: &DisplayManager,
    success_message: Option<String>,
) -> i32 {
    let Ok(editor) = std::env::var("EDITOR") else {
        report_missing_editor(display);
        return 1;
    };

    let mut cmd = Command::new(&editor);
    cmd.arg(file_path);

    // For VS Code, wait for the file to be closed
    if editor == "code" {
        cmd.arg("--wait");
    }

    match cmd.status() {
        Ok(status) if status.success() => {
            if let Some(message) = success_message {
                display.print_success(message);
            }
            0
        }
        Ok(_) => {
            display.print_warning("Editor exited with non-zero status.");
            1
        }
        Err(e) => {
            display.print_error(format!("Failed to start editor '{editor}': {e}"));
            1
        }
    }
}

// The error and its remedy, both on stderr.
fn report_missing_editor(display: &DisplayManager) {
    display.print_error("EDITOR environment variable is not set.");
    display.print_suggestion("Please set EDITOR and try again.");
}

/// Check if EDITOR environment variable is set and provide helpful error messages
///
/// Returns the editor command if available, or reports an error and returns None.
/// Provides context-specific error messages for different scenarios.
pub(crate) fn check_editor_available(
    display: &DisplayManager,
    package_name: &str,
    package_exists: bool,
    package_path: Option<&Path>,
) -> Option<String> {
    if let Ok(editor) = std::env::var("EDITOR") {
        Some(editor)
    } else {
        display.print_error("EDITOR environment variable is not set.");

        if package_exists {
            if let Some(path) = package_path {
                display.print_suggestion(format!(
                    "Package '{}' exists at {}. Go ahead and open it in your editor of choice!",
                    package_name,
                    path.display()
                ));
            } else {
                display.print_suggestion(format!(
                    "Package '{package_name}' exists. Set EDITOR to edit it automatically."
                ));
            }
        } else {
            display.print_suggestion(format!(
                "Package '{package_name}' doesn't exist yet. Set EDITOR and try again to create it."
            ));
        }
        None
    }
}

/// Create a package service with repository and command runner
pub(crate) fn create_package_service(
    config: &CliConfig,
    cancellation_token: CancellationToken,
) -> impl PackageService + SpecService {
    let repo = create_package_repository(config);
    let command_runner = create_command_runner(config);
    PackageServiceImpl::new(
        repo,
        create_dotfiles_repository(config),
        command_runner,
        GixGitStatusProvider,
        config.selfie_config().clone(),
        cancellation_token,
    )
}

/// Create a formatted table with consistent styling
pub(crate) fn create_formatted_table() -> Table {
    let mut table = Table::new();
    table
        .load_style(presets::UTF8_FULL_CONDENSED.with_rounded_corners())
        .set_content_arrangement(ContentArrangement::Dynamic);
    table
}

/// Environment names in the order given, which is the order the spec file gives
/// them, with the current environment marked `*`.
pub(crate) fn format_environment_names(
    environments: &[String],
    current_environment: &str,
    config: &CliConfig,
) -> String {
    environments
        .iter()
        .map(|env_name| {
            if env_name == current_environment {
                let env = format!("*{env_name}");
                if config.use_colors() {
                    style(env).bold().green().to_string()
                } else {
                    env
                }
            } else if config.use_colors() {
                style(env_name).dim().green().to_string()
            } else {
                env_name.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Format a key with consistent styling
pub(crate) fn format_field_key(key: &str, use_colors: bool) -> String {
    if use_colors {
        style(key).cyan().bold().to_string()
    } else {
        key.to_string()
    }
}

/// Format a value with consistent styling
pub(crate) fn format_field_value(value: &str, use_colors: bool) -> String {
    if use_colors {
        style(value).white().to_string()
    } else {
        value.to_string()
    }
}

/// Display environment error with available environments for a specific package
pub(crate) fn display_environment_summary(
    package_name: &str,
    current_environment: &str,
    available_environments: &[String],
    config: &CliConfig,
    display: &DisplayManager,
    context: &str, // "check" or "install"
) {
    if available_environments.is_empty() {
        display_generic_environment_suggestion(
            package_name,
            current_environment,
            config,
            display,
            context,
        );
    } else {
        display.print_error(format!(
            "Package '{package_name}' doesn't support environment '{current_environment}'."
        ));
        display.print_note(format!("{INDENT}Available environments for this package:"));

        let mut table = create_formatted_table();
        // Printed on stderr, so sized to the terminal stderr is on.
        table.use_stderr();
        table.set_header(vec!["Environment"]);

        // Sort environments, highlighting the current one if present
        let mut sorted_envs = available_environments.to_vec();
        sorted_envs.sort();

        for env in sorted_envs {
            let env_display = if config.use_colors() {
                if env == current_environment {
                    console::style(&env).green().bold().to_string()
                } else {
                    env.clone()
                }
            } else {
                env
            };
            table.add_row(vec![env_display]);
        }

        display.print_note(format!("{table}"));

        if config.use_colors() {
            display.print_suggestion(format!(
                "{} with one of the environments above",
                console::style(format!(
                    "selfie package {context} --environment <env> <package>"
                ))
                .yellow()
            ));
        } else {
            display.print_suggestion(format!(
                "selfie package {context} --environment <env> <package> with one of the environments above"
            ));
        }
    }
}

/// Report that `package_name` has no `kind` command ("check", "install") in
/// `environment`, naming the environments that have one (stderr).
pub(crate) fn display_missing_command(
    display: &DisplayManager,
    kind: &str,
    package_name: &str,
    environment: &str,
    others: &[String],
) {
    display.print_error(format!(
        "No {kind} command defined for '{package_name}' in environment '{environment}'."
    ));
    if !others.is_empty() {
        // Sorted, so the line reads the same on every run.
        let mut others = others.to_vec();
        others.sort();
        display.print_note(format!(
            "Environments with {kind} commands: {}",
            others.join(", ")
        ));
    }
}

/// Display generic environment suggestion when specific environment info is not available
pub(crate) fn display_generic_environment_suggestion(
    package_name: &str,
    current_environment: &str,
    config: &CliConfig,
    display: &DisplayManager,
    context: &str, // "check" or "install"
) {
    display.print_error(format!(
        "Package '{package_name}' doesn't support environment '{current_environment}'."
    ));
    display.print_note(format!("{INDENT}Try one of these options:"));
    if config.use_colors() {
        display.print_note(format!(
            "{INDENT}• {} to {} with a different environment",
            console::style(format!(
                "selfie package {context} --environment <env> <package>"
            ))
            .yellow(),
            context
        ));
        display.print_note(format!(
            "{INDENT}• {} to see which environments this package supports",
            console::style("selfie spec info <package>").yellow()
        ));
    } else {
        display.print_note(format!(
            "{INDENT}• selfie package {context} --environment <env> <package> to {context} with a different environment"
        ));
        display.print_note(format!(
            "{INDENT}• selfie spec info <package> to see which environments this package supports"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use selfie::package::port::MockPackageRepository;
    use test_common::test_config_with_dir;

    // The environments that have the command are listed in order, whatever
    // order the spec's map gave them.
    #[test]
    fn a_missing_command_lists_the_others_in_order() {
        let display = DisplayManager::new(false);

        display_missing_command(
            &display,
            "check",
            "bat",
            "test",
            &["macos-work".to_string(), "linux".to_string()],
        );

        assert_eq!(
            display.printed().last().map(|(_, line)| line.as_str()),
            Some("Environments with check commands: linux, macos-work")
        );
    }

    // A missing editor and its remedy are both on stderr.
    #[test]
    fn a_missing_editor_is_reported_on_stderr() {
        use crate::display_manager::Channel;

        let display = DisplayManager::new(false);
        report_missing_editor(&display);

        assert_eq!(
            display.printed(),
            vec![
                (
                    Channel::Stderr,
                    "EDITOR environment variable is not set.".to_string()
                ),
                (
                    Channel::Stderr,
                    "Please set EDITOR and try again.".to_string()
                ),
            ]
        );
    }

    // An environment the package does not support is a failure, so every line
    // explaining it, the table and the suggestion included, is on stderr.
    #[test]
    fn an_environment_summary_is_all_on_stderr() {
        use crate::display_manager::Channel;

        let config = CliConfig::wrap_for_test(test_config_with_dir(std::path::Path::new("/p")));
        let display = DisplayManager::new(false);

        display_environment_summary(
            "bat",
            "test",
            &["macos".to_string(), "linux".to_string()],
            &config,
            &display,
            "check",
        );

        let printed = display.printed();
        assert!(printed.len() > 3, "{printed:?}");
        assert!(
            printed.iter().all(|(stream, _)| *stream == Channel::Stderr),
            "{printed:?}"
        );
        assert!(
            printed
                .iter()
                .any(|(_, line)| line.contains("doesn't support environment 'test'")),
            "{printed:?}"
        );
    }

    // The generic form, for a package that lists no environments, as well.
    #[test]
    fn a_generic_environment_suggestion_is_all_on_stderr() {
        use crate::display_manager::Channel;

        let config = CliConfig::wrap_for_test(test_config_with_dir(std::path::Path::new("/p")));
        let display = DisplayManager::new(false);

        display_generic_environment_suggestion("bat", "test", &config, &display, "check");

        let printed = display.printed();
        assert!(printed.len() > 2, "{printed:?}");
        assert!(
            printed.iter().all(|(stream, _)| *stream == Channel::Stderr),
            "{printed:?}"
        );
    }

    // A picker offers the name selfie looks a package up by, the file's, even
    // where the `name:` field says otherwise; a package with no spec file name
    // is not offered at all.
    #[test]
    fn pickers_offer_the_name_the_file_gives() {
        use selfie::package::PackageBuilder;
        use selfie::package::port::ListPackagesOutput;

        let mut repo = MockPackageRepository::new();
        repo.expect_list_packages().returning(|| {
            Ok(ListPackagesOutput::from_packages(vec![
                PackageBuilder::default()
                    .name("foo")
                    .path("/packages/bar.yml")
                    .build(),
                PackageBuilder::default()
                    .name("neovim")
                    .path("/packages/Neovim.yml")
                    .build(),
                PackageBuilder::default().name("in-memory").build(),
            ]))
        });

        let (names, skipped) = package_names_and_skipped(&repo).unwrap();

        // As the file spells it: lookup ignores case, and the user sees the file.
        assert_eq!(names, vec!["Neovim", "bar"]);
        assert!(skipped.is_empty());
    }

    #[test]
    fn test_vs_code_wait_flag_logic() {
        // Test that VS Code gets the --wait flag
        let editor = "code";
        let mut cmd = Command::new(editor);
        cmd.arg("/tmp/test.yml");

        if editor == "code" {
            cmd.arg("--wait");
        }

        let args: Vec<_> = cmd.get_args().collect();
        assert!(
            args.iter()
                .any(|arg| *arg == std::ffi::OsStr::new("--wait"))
        );
    }

    #[test]
    fn test_format_environment_names() {
        // Test environment name formatting without filesystem operations
        let package_dir = std::path::PathBuf::from("/test/packages");
        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));
        let environments = vec!["test".to_string(), "production".to_string()];

        let result = format_environment_names(&environments, "test", &config);

        // Just test that it doesn't panic and returns something
        assert!(!result.is_empty());
        assert!(result.contains("test"));
    }

    // The spec file's order, on every surface: the same file must not list its
    // environments one way here and another way in the MCP server's answer. The
    // current environment is marked where it stands, not moved to the front.
    #[test]
    fn test_format_environment_names_ordering() {
        let package_dir = std::path::PathBuf::from("/test/packages");
        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));

        let environments = vec!["zeta".to_string(), "alpha".to_string(), "mid".to_string()];

        let result = format_environment_names(&environments, "mid", &config);

        assert_eq!(result, "zeta, alpha, *mid");
    }

    #[test]
    fn test_format_environment_names_single_environment() {
        let package_dir = std::path::PathBuf::from("/test/packages");
        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));

        let environments = vec!["macos-work".to_string()];
        let result = format_environment_names(&environments, "macos-work", &config);

        assert_eq!(result, "*macos-work");
    }

    #[test]
    fn test_format_environment_names_current_not_present() {
        let package_dir = std::path::PathBuf::from("/test/packages");
        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));

        let environments = vec!["arch-home".to_string(), "ubuntu-server".to_string()];
        let result = format_environment_names(&environments, "macos-work", &config);

        // Should not contain asterisk since current environment is not in the list
        assert!(!result.contains('*'));
        assert_eq!(result, "arch-home, ubuntu-server");
    }

    #[test]
    fn test_format_field_key_and_value() {
        let key = format_field_key("Test Key", false);
        assert_eq!(key, "Test Key");

        let value = format_field_value("Test Value", false);
        assert_eq!(value, "Test Value");

        // Test with colors (just ensure no panic)
        let _colored_key = format_field_key("Test Key", true);
        let _colored_value = format_field_value("Test Value", true);
    }
}
