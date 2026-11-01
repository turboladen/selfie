//! What several commands build and run the same way: the repositories and
//! services they call, the sudo policy every write is built with, the track
//! helpers, and opening the user's editor. How a command lays out its output is
//! in [`presentation`](super::presentation).

use crate::terminal_text::{DisplayPath, Name, Shown, lit, shown};
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

use crate::{
    config::CliConfig,
    event_processor::{EventProcessor, Exit},
};
use std::{path::Path, process::Command};

use crate::display_manager::DisplayManager;

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
) -> Result<(Vec<String>, Vec<Shown>), PackageListError> {
    let output = repo.list_packages()?;

    // A name several files claim is offered by neither picker, since every
    // command using it refuses it; the files are named instead.
    let listing = output.names();
    let skipped = output
        .invalid_packages()
        .map(crate::received::skipped_spec)
        .chain(listing.rows.iter().map(crate::received::ambiguous_spec))
        .collect();

    // Not the `name:` field: a dependency or a track destination is resolved by
    // file name, and a field that disagrees with it names nothing selfie finds.
    let mut names: Vec<String> = listing
        .usable
        .iter()
        .copied()
        .filter_map(file_name_of)
        .collect();
    names.sort();
    names.dedup();

    Ok((names, skipped))
}

/// The name `package` is offered and reported under: its spec file's stem as
/// spelled, which lookup resolves ignoring case. `None` for a package with no
/// spec file name.
pub(crate) fn file_name_of(package: &selfie::package::Package) -> Option<String> {
    package.spec_name_as_spelled()
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
    let git = GixGitAdapter::default();
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
pub(crate) fn refuse_under_sudo(config: &CliConfig, display: &DisplayManager) -> Option<Exit> {
    let refusal = sudo_policy(config).refusal(WriteScope::Dotfiles)?;
    display.print_error(crate::received::message(refusal.message()));
    display.print_suggestion(crate::received::message(refusal.suggestion()));
    Some(Exit::Failed)
}

/// The message for a name the namespace check refused, for a command tracking a
/// dotfile under that name.
pub(crate) fn name_check_message(
    name: &str,
    error: &selfie::namespace::NamespaceValidationError,
) -> Shown {
    use selfie::namespace::{NamespaceValidationError as Invalid, Remedy};

    match error {
        // The fact, then what a track can do about it.
        Invalid::Conflict(conflict) => match conflict.remedy() {
            Remedy::ChooseAnotherName => shown!(
                "{} Choose a different name.",
                crate::received::sentence(conflict)
            ),
            Remedy::RemoveTheDotfileSpec => shown!(
                "{} Remove it first or choose a different name.",
                crate::received::sentence(conflict)
            ),
            Remedy::UseThePackage => crate::terminal_text::then_command(
                shown!(
                    "{} To track a file for that package, use `selfie package track-dotfile`, or \
                     choose a different name.",
                    crate::received::sentence(conflict)
                ),
                crate::formatters::selfie_command(
                    &[lit!("package"), lit!("track-dotfile")],
                    name,
                    &[lit!("<file>")],
                )
                .as_ref(),
            ),
        },
        // A package or dotfiles directory that would not read says nothing about the
        // name, and telling the user they cannot use it sends them off to pick another
        // one, which fails in exactly the same way.
        Invalid::PackageDirectoryUnreadable(_) | Invalid::DotfilesDirectoryUnreadable(_) => {
            crate::received::composed(error)
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
) -> Exit {
    // Ahead of building the service, so a run under sudo prints only the
    // refusal, not the "Started" message the service would emit first.
    if let Some(exit) = refuse_under_sudo(config, display) {
        return exit;
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
    result.exit
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
) -> Exit {
    // Ahead of building the service, as `handle_track_standalone` does, so a run
    // under sudo prints only the refusal and not the "Started" message the
    // service emits before its own.
    if let Some(exit) = refuse_under_sudo(config, display) {
        return exit;
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
    result.exit
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
            display.print_info(crate::received::success(success));
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
    success_message: Option<Shown>,
) -> Exit {
    let Some(editor) = configured_editor() else {
        report_missing_editor(display);
        return Exit::Failed;
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
            Exit::Clean
        }
        Ok(_) => {
            display.print_warning(lit!("Editor exited with non-zero status."));
            Exit::Failed
        }
        Err(e) => {
            display.print_error(editor_failed(&editor, &e));
            Exit::Failed
        }
    }
}

// Why `editor` did not start. The editor is what `EDITOR` names, a program
// the user acts on, so it shows as a name: an invisible character in it shows.
fn editor_failed(editor: &str, error: &std::io::Error) -> Shown {
    shown!(
        "Failed to start editor '{}': {}",
        Name::new(editor),
        crate::received::sentence(error)
    )
}

// The editor `EDITOR` names. A unit test runs in-process with the user's
// environment, so under test there is none: a test reaching an editor path,
// or a mutant steering one there, must never launch the user's editor.
fn configured_editor() -> Option<String> {
    if cfg!(test) {
        None
    } else {
        std::env::var("EDITOR").ok()
    }
}

// The error and its remedy, both on stderr.
fn report_missing_editor(display: &DisplayManager) {
    display.print_error(lit!("EDITOR environment variable is not set."));
    display.print_suggestion(lit!("Please set EDITOR and try again."));
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
    if let Some(editor) = configured_editor() {
        Some(editor)
    } else {
        display.print_error(lit!("EDITOR environment variable is not set."));

        if package_exists {
            if let Some(path) = package_path {
                display.print_suggestion(shown!(
                    "Package '{}' exists at {}. Go ahead and open it in your editor of choice!",
                    Name::new(package_name),
                    DisplayPath::of(path)
                ));
            } else {
                display.print_suggestion(shown!(
                    "Package '{}' exists. Set EDITOR to edit it automatically.",
                    Name::new(package_name)
                ));
            }
        } else {
            display.print_suggestion(shown!(
                "Package '{}' doesn't exist yet. Set EDITOR and try again to create it.",
                Name::new(package_name)
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
        GixGitStatusProvider::default(),
        config.selfie_config().clone(),
        cancellation_token,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use selfie::package::port::MockPackageRepository;

    // A package's name in the track-dotfile command is one shell word, after
    // `--` when it starts with a dash. The command has a line of its own, so a
    // backtick inside a quoted name cannot end a quotation in the sentence.
    #[test]
    fn a_package_conflict_offers_a_track_command_that_runs_as_shown() {
        let message = |name: &str| {
            let conflict = selfie::namespace::NamespaceConflict::new(
                name,
                selfie::namespace::NameLocation::Packages,
                vec![std::path::PathBuf::from("/p/x.yml")],
            )
            .expect("a claimant");
            name_check_message(name, &conflict.into()).into_string()
        };

        for (name, command) in [
            ("tool", "selfie package track-dotfile tool <file>"),
            ("-tool", "selfie package track-dotfile -- -tool <file>"),
            ("café", "selfie package track-dotfile 'café' <file>"),
            ("a`b", "selfie package track-dotfile 'a`b' <file>"),
        ] {
            let shown = message(name);
            let lines: Vec<&str> = shown.lines().collect();
            assert_eq!(lines.len(), 2, "{shown}");
            assert!(lines[0].ends_with("or choose a different name."), "{shown}");
            assert_eq!(lines[1], command, "{shown}");
        }
    }

    // A name several files claim gets one remedy wherever they are: the sentence
    // already says the extra files must go, and track-dotfile refuses the name
    // until they do. One claimant keeps the remedy for its kind.
    #[test]
    fn a_name_several_specs_claim_gets_one_remedy() {
        use selfie::namespace::{NameLocation, NamespaceConflict};

        let message = |found_in: NameLocation, paths: &[&str]| {
            let paths = paths.iter().map(std::path::PathBuf::from).collect();
            let conflict = NamespaceConflict::new("dup", found_in, paths).expect("a claimant");
            name_check_message("dup", &conflict.into()).into_string()
        };

        for found_in in [NameLocation::Packages, NameLocation::Dotfiles] {
            let shown = message(found_in, &["/s/dup.yaml", "/s/dup.yml"]);
            assert!(
                shown.ends_with("all but one must be removed. Choose a different name."),
                "{shown}"
            );
            for wrong in ["Remove it", "track-dotfile"] {
                assert!(!shown.contains(wrong), "{wrong}: {shown}");
            }
        }
        let shown = message(NameLocation::Dotfiles, &["/s/dup.yml"]);
        assert!(
            shown.ends_with("Remove it first or choose a different name."),
            "{shown}"
        );
    }

    // Whatever EDITOR the test process inherits, no unit test can launch it:
    // a path reaching the editor is refused as if none were set.
    #[test]
    fn a_unit_test_reaching_the_editor_is_told_editor_is_not_set() {
        let temp = tempfile::tempdir().unwrap();
        let display = DisplayManager::new(false);

        let exit = open_editor(&temp.path().join("spec.yml"), &display, None);

        assert_eq!(configured_editor(), None);
        assert_eq!(exit, Exit::Failed);
        let refusals = display
            .printed()
            .iter()
            .filter(|(_, line)| line.contains("EDITOR environment variable is not set."))
            .count();
        assert_eq!(refusals, 1, "{:?}", display.printed());
    }

    // An editor that would not start is named as a name, so an invisible
    // character in `EDITOR` shows escaped.
    #[test]
    fn an_editor_that_did_not_start_shows_an_invisible_character() {
        let error = std::io::Error::from(std::io::ErrorKind::NotFound);

        let shown = editor_failed("/opt/vi\u{200b}m", &error);

        assert!(
            shown.starts_with("Failed to start editor '/opt/vi\\u{200b}m': "),
            "{shown}"
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
}
