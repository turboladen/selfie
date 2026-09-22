//! Interactive track command handler
//!
//! This module handles the `selfie track <file>` CLI command — an interactive
//! shortcut that prompts the user to choose whether the file belongs to an
//! existing package or should become a new standalone dotfile, then delegates
//! to the appropriate tracking handler.

use std::{collections::HashSet, io::IsTerminal as _};

use dialoguer::{FuzzySelect, Input, theme::ColorfulTheme};
use selfie::{
    fs::real::RealFileSystem,
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
    display_manager::DisplayManager,
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
    /// User cancelled
    Cancelled,
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

    // Refused rather than attempted: `FuzzySelect` reads keys in a loop of its own,
    // and with no terminal that loop never ends, re-rendering the menu until it
    // floods the output and pins a core. `interact_opt` returns no `Err` to handle.
    //
    // **stderr**, not stdin. `FuzzySelect::interact_opt` prompts on `Term::stderr`,
    // and console's `read_key` answers `Key::Unknown` at once when that terminal is
    // not attended, while console reads input from `/dev/tty` when stdin is not one.
    // So `selfie track x 2>log` from a terminal spins with a tty on stdin, and
    // `selfie track x </dev/null` from a terminal would have worked. A guard on
    // stdin gets both backwards.
    if !std::io::stderr().is_terminal() {
        display.print_error(
            "Choosing where to track a file needs a terminal. Name the destination instead: \
             `selfie package track-dotfile <package> <file>` to add it to an existing package, or \
             `selfie dotfiles track <name> <file>` to make it a standalone dotfile."
                .to_string(),
        );
        // Non-zero, and not treated as a cancellation: nothing was tracked, and
        // exiting 0 would tell a script the file is handled.
        return 1;
    }

    let choice = prompt_track_choice(&package_names, file);

    match choice {
        TrackChoice::ExistingPackage(ref name) => {
            common::handle_track_for_package(name, file, config, display, cancellation_token).await
        }
        TrackChoice::NewStandalone(ref name) => {
            // Validate namespace before creating
            if let Err(e) = namespace::validate_unique_name(
                name,
                &repo,
                Some(&dotfiles_repo),
                &selfie::fs::RealFileSystem,
                &config.selfie_config().dotfiles_directory(),
            ) {
                display.print_error(format!("Cannot use name '{name}': {e}"));
                return 1;
            }
            common::handle_track_standalone(name, file, config, display, cancellation_token).await
        }
        TrackChoice::Cancelled => {
            display.print_info("Cancelled.");
            0
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
fn prompt_track_choice(package_names: &[String], file: &str) -> TrackChoice {
    // Build the selection list: existing packages + sentinel options
    let mut items: Vec<String> = package_names.to_vec();
    items.push(NEW_STANDALONE.to_string());
    items.push(TYPE_A_NAME.to_string());

    let selection = FuzzySelect::with_theme(&ColorfulTheme::default())
        .with_prompt("Where should this file be tracked?")
        .items(&items)
        .default(0)
        .interact_opt();

    let choice = match selection {
        Ok(Some(idx)) => idx,
        Ok(None) | Err(_) => return TrackChoice::Cancelled,
    };

    resolve_choice(&items, choice, file)
}

/// Pure function: given the selection list and the chosen index, determine action.
fn resolve_choice(items: &[String], choice: usize, file: &str) -> TrackChoice {
    let selected = &items[choice];

    if selected == TYPE_A_NAME {
        match prompt_for_name() {
            Some(name) => TrackChoice::NewStandalone(name),
            None => TrackChoice::Cancelled,
        }
    } else if selected == NEW_STANDALONE {
        let suggested = suggest_name(file);
        match prompt_for_name_with_default(&suggested) {
            Some(name) => TrackChoice::NewStandalone(name),
            None => TrackChoice::Cancelled,
        }
    } else {
        TrackChoice::ExistingPackage(selected.clone())
    }
}

/// Prompt the user to type a dotfile name (no default).
fn prompt_for_name() -> Option<String> {
    Input::with_theme(&ColorfulTheme::default())
        .with_prompt("Name for the new dotfile spec")
        .interact_text()
        .ok()
        .filter(|s: &String| !s.trim().is_empty())
}

/// Prompt for a name, pre-filling with a suggested default.
fn prompt_for_name_with_default(default: &str) -> Option<String> {
    Input::with_theme(&ColorfulTheme::default())
        .with_prompt("Name for the new dotfile spec")
        .default(default.to_string())
        .interact_text()
        .ok()
        .filter(|s: &String| !s.trim().is_empty())
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

/// Check if a file is already tracked by any package or standalone dotfile.
///
/// Scans both the packages directory and the dotfiles directory for a dotfile
/// entry whose target matches the given file path. Returns the name of the
/// package that tracks it and the entry's own target, or `None`, paired with a
/// warning for every spec and every directory it could not read.
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
    // Each repository is carried with the name of the directory it reads, so a
    // run told one of them could not be listed knows which one to go and look
    // at. The path is left to the error, which already carries it.
    let repos = [
        (&package_repo, "package directory"),
        (dotfiles_repo, "dotfiles directory"),
    ];

    for (repo, directory) in repos {
        // A spec selfie could not read may already track this file, and so may
        // every spec in a directory it could not list, so both are reported.
        // A directory that is not there holds no spec that could track it.
        let output = match repo.list_packages() {
            Ok(output) => output,
            Err(PackageListError::PackageDirectoryNotFound(_)) => continue,
            Err(e) => {
                skipped.push(format!("Could not check the {directory}: {e}"));
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
/// # Errors
///
/// A message to display when the package directory cannot be listed. There is
/// nowhere to put the file if selfie cannot see the directory it would go in.
fn load_package_names(repo: &impl PackageRepository) -> Result<(Vec<String>, Vec<String>), String> {
    common::package_names_and_skipped(repo).map_err(|e| format!("Failed to list packages: {e}"))
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

    #[test]
    fn resolve_choice_selects_existing_package() {
        let items = vec![
            "alacritty".to_string(),
            "fnm".to_string(),
            NEW_STANDALONE.to_string(),
            TYPE_A_NAME.to_string(),
        ];
        let result = resolve_choice(&items, 0, "~/.config/test.toml");
        assert_eq!(
            result,
            TrackChoice::ExistingPackage("alacritty".to_string())
        );
    }
}
