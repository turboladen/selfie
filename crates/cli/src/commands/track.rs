//! Interactive track command handler
//!
//! This module handles the `selfie track <file>` CLI command — an interactive
//! shortcut that prompts the user to choose whether the file belongs to an
//! existing package or should become a new standalone dotfile, then delegates
//! to the appropriate tracking handler.

use std::collections::HashSet;

use dialoguer::{FuzzySelect, Input, theme::ColorfulTheme};
use selfie::{
    fs::{DirectoryState, real::RealFileSystem},
    namespace,
    package::{
        SpecOrigin,
        port::{PackageListError, PackageRepository},
        repository::yaml::YamlPackageRepository,
    },
};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::{
    commands::common::{self, create_dotfiles_repository, create_package_repository},
    config::CliConfig,
    display_manager::{DisplayManager, PromptFailure},
    event_processor::Exit,
};

/// Sentinel item appended after real package names in the select list.
const NEW_STANDALONE: &str = "→ New standalone dotfile";
/// Sentinel item for free-text name entry.
const TYPE_A_NAME: &str = "→ Let me type a name";

/// What the user chose in the interactive prompt.
#[derive(Debug, PartialEq)]
enum TrackChoice {
    /// Add the file to an existing package
    ExistingPackage(String),
    /// Create a new standalone dotfile with the given name
    NewStandalone(String),
    /// The user declined: Esc at the picker, or an empty name.
    Declined,
}

/// Handle the `selfie track` interactive command
pub(crate) async fn handle_track(
    file: &str,
    config: &CliConfig,
    display: &DisplayManager,
    cancellation_token: CancellationToken,
) -> i32 {
    info!("Interactive track for '{}'", file);

    // Before the prompt, not after it. Every path out of `prompt_track_choice`
    // ends in a service call the library refuses, so asking first would collect
    // a package choice and a spec name and then throw both away.
    if let Some(code) = common::refuse_under_sudo(config, display) {
        return code;
    }

    let repo = create_package_repository(config);

    // The namespace check below uses the same repository.
    let dotfiles_repo = create_dotfiles_repository(config);

    // Check if this file is already tracked anywhere
    let (existing_tracker, unchecked) = find_existing_tracker(file, config, &dotfiles_repo);

    if let Some(tracked) = existing_tracker {
        return report_existing_tracker(&tracked, display);
    }

    // Collect available package names for the prompt. Done before anything from
    // the scan above is printed: this reads the package directory too, and its
    // failure ends the run, so a warning that the same directory could not be
    // checked would only be a quieter version of the error that follows it.
    let (package_names, skipped) = match load_package_names(&repo) {
        Ok(loaded) => loaded,
        Err(msg) => {
            display.print_error(msg);
            return 1;
        }
    };

    // Reaching here means no spec selfie could read tracks this file, and that
    // the package directory itself was readable. Where something else could not
    // be read -- an individual spec, or the dotfiles directory -- "nothing
    // tracks it" is not what selfie established, and the run is about to offer
    // to write a second entry for it.
    //
    // Both scans read the package directory, so an unreadable spec there
    // composes the same sentence twice; the second printing is noise.
    let mut reported: HashSet<String> = HashSet::new();
    for warning in unchecked.into_iter().chain(skipped) {
        if reported.insert(warning.clone()) {
            display.print_warning(warning);
        }
    }

    let choice = match prompt_track_choice(&package_names, file, display) {
        Ok(choice) => choice,
        Err(failure) => {
            return display
                .refuse_prompt(
                    &failure,
                    "Choosing where to track a file",
                    "Name the destination instead: `selfie package track-dotfile <package> <file>` \
                     to add it to an existing package, or `selfie dotfiles track <name> <file>` to \
                     make it a standalone dotfile.",
                )
                .code();
        }
    };

    match choice {
        TrackChoice::ExistingPackage(ref name) => {
            common::handle_track_for_package(name, file, config, display, cancellation_token).await
        }
        TrackChoice::NewStandalone(ref name) => {
            // Validate namespace before creating
            if let Err(e) = namespace::validate_unique_name(name, &repo, Some(&dotfiles_repo)) {
                display.print_error(common::name_check_message(name, &e));
                return 1;
            }
            common::handle_track_standalone(name, file, config, display, cancellation_token).await
        }
        // Nothing was tracked, so the run did not do what it was asked.
        TrackChoice::Declined => {
            display.print_run_note("Cancelled.");
            Exit::Failed.code()
        }
    }
}

/// Report a file some spec already tracks, and the exit code for it.
///
/// Returns 1 for an entry no apply can ever deploy, 0 otherwise.
fn report_existing_tracker(tracked: &ExistingTracker, display: &DisplayManager) -> i32 {
    let ExistingTracker {
        spec_name,
        spec_path,
        target,
    } = tracked;

    let fs = RealFileSystem;

    // `deploy_target` rather than `TargetRejection::of`: the textual rule cannot
    // see a `~/…` whose home directory could not be determined, which falls
    // through to a relative path that apply refuses. Asking the same function the
    // library asks is also what keeps the two from drifting.
    //
    // Reported as tracked *and* undeployable. Saying only the first half tells the
    // user the file is handled when no deploy will ever touch it, with an exit
    // code a script reads as done.
    let expanded = match selfie::fs::deploy_target(&fs, target) {
        Ok(expanded) => expanded,
        Err(rejection) => {
            display.print_error(format!(
                "'{target}' is tracked in spec '{spec_name}', but {}",
                rejection.message()
            ));
            // `NoHome` is the machine's state rather than the spec's: that entry
            // is correct and editing it would break a spec that works everywhere
            // else, so it keeps the rule's own advice. The other two are defects
            // in a file the user can open, and the path to open is what they
            // need rather than advice on choosing a target.
            if matches!(rejection, selfie::fs::TargetRejection::NoHome) {
                display.print_suggestion(rejection.suggestion());
            } else {
                display.print_suggestion(format!(
                    "Edit {} to correct the target.",
                    spec_path.display()
                ));
            }
            return 1;
        }
    };

    // The same silence the library breaks for its own two track commands: this
    // answer is given before the library is reached, so an already-tracked target
    // that is not a regular file would otherwise be reported here as plainly
    // tracked and mentioned by nothing. Shares the library's wording so the two
    // cannot describe one situation differently.
    if let Some(warning) = selfie::dotfile_service::track::already_tracked_refusal(&fs, &expanded) {
        display.print_warning(warning);
    }

    display.print_info(format!("Already tracking '{target}' in spec '{spec_name}'"));
    0
}

/// Present the interactive selection prompt and return the user's choice.
///
/// # Errors
///
/// [`PromptFailure`] when either prompt got no answer.
fn prompt_track_choice(
    package_names: &[String],
    file: &str,
    display: &DisplayManager,
) -> Result<TrackChoice, PromptFailure> {
    // Build the selection list: existing packages + sentinel options
    let mut items: Vec<String> = package_names.to_vec();
    items.push(NEW_STANDALONE.to_string());
    items.push(TYPE_A_NAME.to_string());

    let selection = display.prompt(
        FuzzySelect::with_theme(&ColorfulTheme::default())
            .with_prompt("Where should this file be tracked?")
            .items(&items)
            .default(0),
    );

    let Some(choice) = selection? else {
        return Ok(TrackChoice::Declined);
    };

    resolve_choice(&items, choice, file, display)
}

/// Given the selection list and the chosen index, determine the action, asking
/// for a name when the choice needs one.
///
/// # Errors
///
/// [`PromptFailure`] when the name prompt got no answer.
fn resolve_choice(
    items: &[String],
    choice: usize,
    file: &str,
    display: &DisplayManager,
) -> Result<TrackChoice, PromptFailure> {
    let selected = &items[choice];

    let default = if selected == TYPE_A_NAME {
        None
    } else if selected == NEW_STANDALONE {
        Some(suggest_name(file))
    } else {
        return Ok(TrackChoice::ExistingPackage(selected.clone()));
    };

    Ok(match prompt_for_name(default, display)? {
        Some(name) => TrackChoice::NewStandalone(name),
        None => TrackChoice::Declined,
    })
}

/// Prompt for a name for the new dotfile spec, pre-filled with `default` when
/// there is one. `None` for an empty answer.
///
/// # Errors
///
/// [`PromptFailure`] when the prompt got no answer.
fn prompt_for_name(
    default: Option<String>,
    display: &DisplayManager,
) -> Result<Option<String>, PromptFailure> {
    let theme = ColorfulTheme::default();
    let mut input = Input::with_theme(&theme).with_prompt("Name for the new dotfile spec");
    if let Some(default) = default {
        input = input.default(default);
    }
    let name: String = display.prompt(crate::display_manager::TextLine(input))?;
    Ok(Some(name).filter(|name| !name.trim().is_empty()))
}

/// Derive a suggested spec name from the file path.
///
/// Takes the final component, strips a leading `.` (so `.dprint.jsonc` becomes
/// `dprint.jsonc`), and drops the extension to give a short default name.
fn suggest_name(file_path: &str) -> String {
    let path = std::path::Path::new(file_path);
    let stem = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();

    // Strip leading dot
    let stem = stem.strip_prefix('.').unwrap_or(&stem);

    // Drop extension to get a terse name
    match stem.rsplit_once('.') {
        Some((base, _ext)) if !base.is_empty() => base.to_string(),
        _ => stem.to_string(),
    }
}

/// The spec entry that already tracks the file the caller named.
struct ExistingTracker {
    spec_name: String,
    /// The file to open to change the entry, which a refusal has to name.
    spec_path: std::path::PathBuf,
    /// The entry's own target, which differs from the argument: the spec holds
    /// `~/…` and the caller may name the same file absolutely.
    target: String,
}

/// What to tell the user when the dotfiles directory would not list, or `None` when
/// there is nothing worth saying.
///
/// The scan's question is "does a spec here already track this file", and a directory
/// with no specs in it answers no. So an absence is not a warning on its own. What
/// earns one is an absence the user is about to be asked questions about: a standalone
/// entry is written into the dotfiles directory, and every state but a readable
/// directory refuses that write. Saying it before the prompt is what keeps the user
/// from choosing a name selfie cannot use.
fn dotfiles_absence_warning(config: &CliConfig, error: &PackageListError) -> Option<String> {
    let path = error.path();
    Some(match error.state().clone() {
        // A directory that classified cleanly and still would not list
        // could not be *listed*, which is the same answer an unlistable
        // one gets.
        DirectoryState::Directory => format!(
            "The dotfiles directory at {} could not be listed, so selfie cannot tell whether a spec in it already tracks this file: {error}",
            path.display()
        ),
        // An empty path is silent unless the user named it, which is the rule
        // `dotfiles_directory_is_expected` states: an empty default is the
        // ordinary condition of anyone who keeps no standalone dotfiles, and
        // a word about it on every `selfie track` is noise. Every other state
        // warns either way. A plain file or a dangling link cannot come from
        // leaving the setting out, and what is behind an unreadable directory
        // is unknown whether or not the user named the path.
        DirectoryState::Absent(selfie::fs::AbsentReason::Empty)
            if !config.selfie_config().dotfiles_directory_is_expected() =>
        {
            return None;
        }
        DirectoryState::Absent(reason) => {
            // The path leads and the clause follows, because a clause may
            // carry a colon of its own — the dangling-symlink one names a
            // destination — and two colons in one sentence read as one
            // path followed by another.
            let mut sentence = format!(
                "The dotfiles directory at {} {}. A standalone dotfile cannot be tracked until that is fixed.",
                path.display(),
                reason.clause()
            );
            if let Some(command) = reason.remedy(path) {
                sentence.push(' ');
                sentence.push_str(&command);
            }
            sentence
        }
        DirectoryState::Unlistable(_) => format!(
            "The dotfiles directory could not be listed, so selfie cannot tell whether a spec in it already tracks this file: {} — {error}",
            path.display()
        ),
        DirectoryState::Unknown(_) => format!(
            "The dotfiles directory could not be checked: {} — {error}",
            path.display()
        ),
    })
}

/// What to tell the user when the package directory would not list.
///
/// Unlike the dotfiles directory, the package directory is always expected, so
/// every state earns a warning.
fn package_listing_warning(error: &PackageListError) -> String {
    // The error's own sentence starts with the path, so it is not named twice.
    format!(
        "The package directory {error}, so its specs were not checked for an entry that already tracks this file"
    )
}

/// Check if a file is already tracked by any package or standalone dotfile.
///
/// Scans both the packages directory and the dotfiles directory for a dotfile
/// entry whose target matches the given file path. Returns the name of the
/// package that tracks it and the entry's own target, or `None`, paired with a
/// warning for every spec it could not read, for a package directory it could not
/// list, and for a dotfiles directory it could not read or write a new entry into.
///
/// A caller that ignores the second list is treating "nothing selfie could read
/// tracks this file" as "nothing tracks it", and a spec it could not read may
/// already carry the entry.
///
/// The entry's target rather than the argument, because the two differ: the spec
/// holds `~/…` and the caller may pass an absolute path for the same file.
fn find_existing_tracker(
    file: &str,
    config: &CliConfig,
    dotfiles_repo: &YamlPackageRepository<RealFileSystem>,
) -> (Option<ExistingTracker>, Vec<String>) {
    let fs = RealFileSystem;
    let expanded = selfie::fs::expand_target_path(&fs, file);
    let mut skipped = Vec::new();

    let package_repo = YamlPackageRepository::new(
        RealFileSystem,
        config.selfie_config().package_directory().to_path_buf(),
        SpecOrigin::PackageDirectory,
    );
    // Each repository is carried with which directory it reads, so a run told one
    // of them could not be listed knows which one to go and look at.
    //
    // The package directory warns too, although `handle_track` reads it again
    // straight after this scan and ends the run if that fails: this function's
    // answer has to be honest about what it did not check for any caller, not
    // only for the one that happens to re-read the directory.
    let repos = [(&package_repo, true), (dotfiles_repo, false)];

    for (repo, is_package_directory) in repos {
        // A spec selfie could not read may already track this file, and so may
        // every spec in a directory it could not list, so both are reported.
        let output = match repo.list_packages() {
            Ok(output) => output,
            Err(error) => {
                skipped.extend(if is_package_directory {
                    Some(package_listing_warning(&error))
                } else {
                    dotfiles_absence_warning(config, &error)
                });
                continue;
            }
        };

        for invalid in output.invalid_packages() {
            skipped.push(selfie::package::service::skipped_spec_warning(invalid));
        }

        for pkg in output.valid_packages() {
            for (_scope, entry) in pkg.dotfiles_with_scope() {
                let entry_expanded = selfie::fs::expand_target_path(&fs, entry.target());
                if entry_expanded == expanded {
                    return (
                        Some(ExistingTracker {
                            spec_name: pkg.name().to_string(),
                            spec_path: pkg.path().to_path_buf(),
                            target: entry.target().to_string(),
                        }),
                        skipped,
                    );
                }
            }
        }
    }
    (None, skipped)
}

/// Load sorted package names from the repository, along with a warning for every
/// spec file that could not be loaded.
///
/// A package directory with nothing at its path yields no names: a fresh machine
/// can still track the file as a new standalone dotfile.
///
/// # Errors
///
/// A message to display when the package directory is there and cannot be
/// listed, or something that is not a directory holds its path.
fn load_package_names(repo: &impl PackageRepository) -> Result<(Vec<String>, Vec<String>), String> {
    match common::package_names_and_skipped(repo) {
        Ok(loaded) => Ok(loaded),
        // A package directory that does not exist yet holds no packages, and the
        // file can still go into a new standalone spec. Anything else at the path
        // is selfie unable to look, which may not be offered as "nothing there".
        Err(listing) if listing.may_be_created() => Ok((Vec::new(), Vec::new())),
        Err(e) => Err(format!("Failed to list packages: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggest_name_strips_leading_dot_and_extension() {
        assert_eq!(suggest_name("~/.dprint.jsonc"), "dprint");
        assert_eq!(suggest_name("/home/user/.config/starship.toml"), "starship");
    }

    #[test]
    fn suggest_name_handles_no_extension() {
        assert_eq!(suggest_name("/home/user/.bashrc"), "bashrc");
    }

    #[test]
    fn suggest_name_handles_no_leading_dot() {
        assert_eq!(suggest_name("/etc/alacritty.toml"), "alacritty");
    }

    #[test]
    fn suggest_name_handles_deeply_nested_path() {
        assert_eq!(suggest_name("~/.config/fish/conf.d/fnm.fish"), "fnm");
    }

    // A spec selfie could not read may already track the file being offered, so
    // "nothing tracks it" and "nothing selfie could read tracks it" have to be
    // distinguishable to the caller. Asserted on the returned list because the
    // prompt that follows needs a terminal, which a test cannot give it.
    #[test]
    fn find_existing_tracker_reports_a_spec_it_could_not_read() {
        use selfie::config::SelfieConfigBuilder;

        let temp = tempfile::TempDir::new().unwrap();
        let packages = temp.path().join("packages");
        std::fs::create_dir_all(&packages).unwrap();
        std::fs::write(packages.join("brokenpkg.yaml"), "{{{\n").unwrap();
        let dotfiles = temp.path().join("dotfiles");
        std::fs::create_dir_all(&dotfiles).unwrap();
        let dotfiles_repo =
            YamlPackageRepository::new(RealFileSystem, dotfiles, SpecOrigin::DotfilesDirectory);

        let config = CliConfig::wrap_for_test(
            SelfieConfigBuilder::default()
                .environment("test-env")
                .package_directory(packages)
                .build(),
        );

        let (found, skipped) =
            find_existing_tracker("~/.config/fish/config.fish", &config, &dotfiles_repo);

        assert!(found.is_none(), "nothing readable tracks that file");
        assert_eq!(skipped.len(), 1, "the unreadable spec must be reported");
        assert!(skipped[0].contains("brokenpkg.yaml"), "got: {}", skipped[0]);
    }

    // The control: with every spec readable, a run that finds no tracker has
    // nothing to report, so a `skipped` that is never empty would fail here.
    #[test]
    fn find_existing_tracker_reports_nothing_for_a_clean_directory() {
        use selfie::config::SelfieConfigBuilder;

        let temp = tempfile::TempDir::new().unwrap();
        let packages = temp.path().join("packages");
        std::fs::create_dir_all(&packages).unwrap();
        std::fs::write(
            packages.join("bat.yaml"),
            "name: bat\nenvironments:\n  test-env:\n    install: \"true\"\n",
        )
        .unwrap();
        let dotfiles = temp.path().join("dotfiles");
        std::fs::create_dir_all(&dotfiles).unwrap();
        let dotfiles_repo =
            YamlPackageRepository::new(RealFileSystem, dotfiles, SpecOrigin::DotfilesDirectory);

        let config = CliConfig::wrap_for_test(
            SelfieConfigBuilder::default()
                .environment("test-env")
                .package_directory(packages)
                .build(),
        );

        let (found, skipped) =
            find_existing_tracker("~/.config/fish/config.fish", &config, &dotfiles_repo);

        assert!(found.is_none());
        assert!(skipped.is_empty(), "got: {skipped:?}");
    }

    // The package directory is always expected, so one that will not list is a
    // place the scan could not check, and the answer says so.
    #[test]
    fn find_existing_tracker_warns_about_a_package_directory_that_is_not_there() {
        use selfie::config::SelfieConfigBuilder;

        let temp = tempfile::TempDir::new().unwrap();
        let packages = temp.path().join("packages");
        let dotfiles = temp.path().join("dotfiles");
        std::fs::create_dir_all(&dotfiles).unwrap();
        let dotfiles_repo =
            YamlPackageRepository::new(RealFileSystem, dotfiles, SpecOrigin::DotfilesDirectory);

        let config = CliConfig::wrap_for_test(
            SelfieConfigBuilder::default()
                .environment("test-env")
                .package_directory(packages.clone())
                .build(),
        );

        let (found, skipped) =
            find_existing_tracker("~/.config/fish/config.fish", &config, &dotfiles_repo);

        assert!(found.is_none());
        assert_eq!(skipped.len(), 1, "got: {skipped:?}");
        assert!(
            skipped[0].starts_with("The package directory /"),
            "got: {}",
            skipped[0]
        );
        assert!(
            skipped[0].matches(&packages.display().to_string()).count() == 1,
            "the warning must name the directory once: {}",
            skipped[0]
        );
    }

    // A dotfiles directory that is not there holds no spec that could track the
    // file, so it is not something the scan failed to check.
    #[test]
    fn find_existing_tracker_is_silent_about_a_dotfiles_directory_that_is_not_there() {
        use selfie::config::SelfieConfigBuilder;

        let temp = tempfile::TempDir::new().unwrap();
        let packages = temp.path().join("packages");
        std::fs::create_dir_all(&packages).unwrap();
        let dotfiles_repo = YamlPackageRepository::new(
            RealFileSystem,
            temp.path().join("dotfiles"),
            SpecOrigin::DotfilesDirectory,
        );

        let config = CliConfig::wrap_for_test(
            SelfieConfigBuilder::default()
                .environment("test-env")
                .package_directory(packages)
                .build(),
        );

        let (found, skipped) =
            find_existing_tracker("~/.config/fish/config.fish", &config, &dotfiles_repo);

        assert!(found.is_none());
        assert!(skipped.is_empty(), "got: {skipped:?}");
    }

    // A fresh machine has no package directory yet. That is no packages to offer,
    // not a failure; a file at the path still is one.
    #[test]
    fn load_package_names_treats_a_package_directory_not_there_yet_as_empty() {
        use selfie::fs::{AbsentReason, DirectoryState};
        use selfie::package::port::{MockPackageRepository, PackageListError};

        let mut repo = MockPackageRepository::new();
        repo.expect_list_packages().returning(|| {
            Err(PackageListError::new(
                "/packages".into(),
                DirectoryState::Absent(AbsentReason::Empty),
            ))
        });
        assert_eq!(load_package_names(&repo), Ok((Vec::new(), Vec::new())));

        let mut repo = MockPackageRepository::new();
        repo.expect_list_packages().returning(|| {
            Err(PackageListError::new(
                "/packages".into(),
                DirectoryState::Absent(AbsentReason::Occupied {
                    kind: "regular file",
                }),
            ))
        });
        let refusal = load_package_names(&repo).expect_err("a file at the path is a failure");
        assert!(
            refusal.starts_with("Failed to list packages: /packages"),
            "{refusal}"
        );
    }

    #[test]
    fn load_package_names_returns_sorted() {
        use selfie::package::PackageBuilder;
        use selfie::package::port::{ListPackagesOutput, MockPackageRepository};

        let mut repo = MockPackageRepository::new();
        repo.expect_list_packages().returning(|| {
            Ok(ListPackagesOutput::from_packages(
                ["zsh", "alacritty", "fnm"]
                    .into_iter()
                    .map(|name| PackageBuilder::default().name(name).build())
                    .collect(),
            ))
        });

        let (names, skipped) = load_package_names(&repo).unwrap();
        assert_eq!(names, vec!["alacritty", "fnm", "zsh"]);
        assert!(skipped.is_empty());
    }

    // The picker cannot offer a spec selfie could not read, so the caller has to
    // be handed something to say about it.
    #[test]
    fn load_package_names_names_the_spec_it_could_not_read() {
        use selfie::package::PackageBuilder;
        use selfie::package::port::{ListPackagesOutput, MockPackageRepository};

        let mut repo = MockPackageRepository::new();
        repo.expect_list_packages().returning(|| {
            Ok(ListPackagesOutput::from_results(vec![
                Ok(PackageBuilder::default().name("fnm").build()),
                Err(selfie::package::port::PackageParseError::new(
                    "/test/packages/broken.yml",
                    selfie::package::port::PackageParseKind::IrregularFile {
                        kind: "named pipe (fifo)",
                    },
                )),
            ]))
        });

        let (names, skipped) = load_package_names(&repo).unwrap();
        assert_eq!(names, vec!["fnm"]);
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].contains("broken.yml"), "got: {}", skipped[0]);
        assert!(
            skipped[0].contains("named pipe (fifo)"),
            "got: {}",
            skipped[0]
        );
    }

    // A file in a fresh home with an empty package directory, and a config
    // naming that directory.
    fn untracked_file() -> (tempfile::TempDir, String, CliConfig) {
        let temp = tempfile::tempdir().unwrap();
        let packages = temp.path().join("packages");
        std::fs::create_dir(&packages).unwrap();
        let file = temp.path().join("x.conf");
        std::fs::write(&file, "x").unwrap();
        let config = CliConfig::wrap_for_test(test_common::test_config_with_dir(&packages));
        (temp, file.to_string_lossy().into_owned(), config)
    }

    // Esc at the destination picker is a decline: nothing was tracked, so the
    // run fails.
    #[tokio::test]
    async fn esc_at_the_destination_picker_fails() {
        let (_temp, file, config) = untracked_file();
        let display = DisplayManager::new(false)
            .answering(vec![crate::display_manager::answer(None::<usize>)]);

        let code = handle_track(&file, &config, &display, CancellationToken::new()).await;

        assert_eq!(code, 1);
    }

    // A blank name is a decline too. dialoguer asks again on an empty line, so
    // only blanks reach the check.
    #[tokio::test]
    async fn a_blank_name_fails() {
        use crate::display_manager::answer;

        // With no packages, index 1 is "Let me type a name".
        let (_temp, file, config) = untracked_file();
        let display = DisplayManager::new(false)
            .answering(vec![answer(Some(1_usize)), answer("  ".to_string())]);

        let code = handle_track(&file, &config, &display, CancellationToken::new()).await;

        assert_eq!(code, 1);
    }

    // Ctrl+C at the destination picker ends the run canceled.
    #[test]
    fn ctrl_c_at_the_destination_picker_is_canceled() {
        let display = DisplayManager::new(false).answering(vec![crate::display_manager::ctrl_c()]);

        let failure = prompt_track_choice(&["bat".to_string()], "~/.batrc", &display).unwrap_err();

        assert_eq!(failure.exit(), crate::event_processor::Exit::Cancelled);
    }

    // Ctrl+C at the name prompt ends the run canceled, not declined.
    #[test]
    fn ctrl_c_at_the_name_prompt_is_canceled() {
        use crate::display_manager::{answer, ctrl_c};

        // Index 1 is the "new standalone dotfile" entry after the one package.
        let display = DisplayManager::new(false).answering(vec![answer(Some(1_usize)), ctrl_c()]);

        let failure = prompt_track_choice(&["bat".to_string()], "~/.batrc", &display).unwrap_err();

        assert_eq!(failure.exit(), crate::event_processor::Exit::Cancelled);
    }

    #[test]
    fn resolve_choice_selects_existing_package() {
        let items = vec![
            "alacritty".to_string(),
            "fnm".to_string(),
            NEW_STANDALONE.to_string(),
            TYPE_A_NAME.to_string(),
        ];
        let result = resolve_choice(
            &items,
            0,
            "~/.config/test.toml",
            &DisplayManager::new(false),
        );
        assert_eq!(
            result.unwrap(),
            TrackChoice::ExistingPackage("alacritty".to_string())
        );
    }
}
