//! Interactive track command handler
//!
//! This module handles the `selfie track <file>` CLI command — an interactive
//! shortcut that prompts the user to choose whether the file belongs to an
//! existing package or should become a new standalone dotfile, then delegates
//! to the appropriate tracking handler.

use crate::terminal_text::{DisplayPath, Name, Shown, lit, shown};
use selfie::dotfile_service::track::Tracker;
use std::collections::HashSet;

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
    display_manager::{Blank, Choice, DisplayManager, PromptFailure, Suggestion},
    event_processor::Exit,
};

/// An entry of the destination picker.
#[derive(Clone, Debug, PartialEq)]
enum Destination {
    /// An existing package, by its raw name.
    Existing(String),
    /// A new standalone dotfile, named after the file.
    NewStandalone,
    /// A new standalone dotfile under a name the user types.
    TypeAName,
}

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
) -> Exit {
    info!("Interactive track for '{}'", file);

    // Before the prompt, not after it. Every path out of `prompt_track_choice`
    // ends in a service call the library refuses, so asking first would collect
    // a package choice and a spec name and then throw both away.
    if let Some(exit) = common::refuse_under_sudo(config, display) {
        return exit;
    }

    let repo = create_package_repository(config);

    // The namespace check below uses the same repository.
    let dotfiles_repo = create_dotfiles_repository(config);

    // Check if this file is already tracked anywhere
    let (trackers, unchecked) = find_existing_trackers(file, config, &dotfiles_repo);

    if !trackers.is_empty() {
        // A spec selfie could not read may track the file as well, so the count
        // below may be short; say so first.
        let mut reported: HashSet<Shown> = HashSet::new();
        for warning in unchecked {
            if reported.insert(warning.clone()) {
                display.print_warning(warning);
            }
        }
        return report_existing_trackers(&trackers, display);
    }

    // Collect available package names for the prompt. Done before anything from
    // the scan above is printed: this reads the package directory too, and its
    // failure ends the run, so a warning that the same directory could not be
    // checked would only be a quieter version of the error that follows it.
    let (package_names, skipped) = match load_package_names(&repo) {
        Ok(loaded) => loaded,
        Err(msg) => {
            display.print_error(msg);
            return Exit::Failed;
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
    let mut reported: HashSet<Shown> = HashSet::new();
    for warning in unchecked.into_iter().chain(skipped) {
        if reported.insert(warning.clone()) {
            display.print_warning(warning);
        }
    }

    let choice = match prompt_track_choice(&package_names, file, display) {
        Ok(choice) => choice,
        Err(failure) => {
            return display.refuse_prompt(
                &failure,
                lit!("Choosing where to track a file"),
                lit!(
                    "Name the destination instead: `selfie package track-dotfile <package> <file>` \
                     to add it to an existing package, or `selfie dotfiles track <name> <file>` to \
                     make it a standalone dotfile."
                ),
            );
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
                return Exit::Failed;
            }
            common::handle_track_standalone(name, file, config, display, cancellation_token).await
        }
        // Nothing was tracked, so the run did not do what it was asked.
        TrackChoice::Declined => {
            display.print_run_note(lit!("Canceled."));
            Exit::Failed
        }
    }
}

/// Report every spec that already tracks a file, and the [`Exit`] for them: the
/// highest of each one's.
fn report_existing_trackers(trackers: &[Tracker], display: &DisplayManager) -> Exit {
    let mut warned = HashSet::new();
    // A fold visits every tracker, so each is reported even after one fails: an
    // `any` would stop printing at the first.
    let exit = trackers
        .iter()
        .map(|tracked| report_existing_tracker(tracked, display, &mut warned))
        .fold(Exit::Clean, Exit::max);

    // Two specs deploying one file overwrite each other on every apply, so the
    // file is not handled, whatever each entry says on its own.
    if let [_, _, ..] = trackers {
        let names =
            crate::formatters::comma_joined(trackers.iter().map(|t| Name::new(&t.spec_name)));
        display.print_error(shown!(
            "{} specs track this file: {}. Keep it in one of them.",
            trackers.len(),
            names
        ));
        return exit.max(Exit::Failed);
    }
    exit
}

/// Report a file some spec already tracks, and the [`Exit`] for it.
///
/// Returns [`Exit::Failed`] for an entry no apply can ever deploy, and
/// [`Exit::Clean`] otherwise. A warning about the target is printed once,
/// however many specs track it: `warned` holds the ones already printed.
fn report_existing_tracker(
    tracked: &Tracker,
    display: &DisplayManager,
    warned: &mut HashSet<String>,
) -> Exit {
    let Tracker {
        spec_name,
        spec_path,
        target,
        ..
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
            display.print_error(shown!(
                "'{}' is tracked in spec '{}', but {}",
                DisplayPath::new(target),
                Name::new(spec_name),
                crate::received::message(rejection.message())
            ));
            // `NoHome` is the machine's state rather than the spec's: that entry
            // is correct and editing it would break a spec that works everywhere
            // else, so it keeps the rule's own advice. The other two are defects
            // in a file the user can open, and the path to open is what they
            // need rather than advice on choosing a target.
            if matches!(rejection, selfie::fs::TargetRejection::NoHome) {
                display.print_suggestion(crate::received::message(rejection.suggestion()));
            } else {
                display.print_suggestion(shown!(
                    "Edit {} to correct the target.",
                    DisplayPath::of(spec_path)
                ));
            }
            return Exit::Failed;
        }
    };

    // The same silence the library breaks for its own two track commands: this
    // answer is given before the library is reached, so an already-tracked target
    // that is not a regular file would otherwise be reported here as plainly
    // tracked and mentioned by nothing. Shares the library's wording so the two
    // cannot describe one situation differently.
    if let Some(warning) = selfie::dotfile_service::track::already_tracked_refusal(&fs, &expanded)
        && warned.insert(warning.clone())
    {
        display.print_warning(crate::received::message(warning));
    }

    display.print_info(shown!(
        "Already tracking '{}' in spec '{}'",
        DisplayPath::new(target),
        Name::new(spec_name)
    ));
    Exit::Clean
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
    let picked = display.fuzzy_select(
        lit!("Where should this file be tracked?"),
        &destinations(package_names),
    )?;

    let Some(destination) = picked else {
        return Ok(TrackChoice::Declined);
    };

    resolve_choice(destination, file, display)
}

/// The picker's entries: existing packages, then the two ways to make a new
/// standalone dotfile.
// A package's entry holds its raw name, so picking a spec whose name the picker
// shows escaped tracks into that spec, not into one named by its escaped
// spelling.
fn destinations(package_names: &[String]) -> Vec<Choice<Destination>> {
    package_names
        .iter()
        .map(|name| Choice::named(name, Destination::Existing(name.clone())))
        .chain([
            Choice::labeled(lit!("→ New standalone dotfile"), Destination::NewStandalone),
            Choice::labeled(lit!("→ Let me type a name"), Destination::TypeAName),
        ])
        .collect()
}

/// The action for the picked `destination`, asking for a name when it needs
/// one.
///
/// # Errors
///
/// [`PromptFailure`] when the name prompt got no answer.
fn resolve_choice(
    destination: Destination,
    file: &str,
    display: &DisplayManager,
) -> Result<TrackChoice, PromptFailure> {
    // A blank answer under the file's own name declines it; asked to type a
    // name, the user is asked again, so one stray Enter does not end the run.
    let (suggested, blank) = match destination {
        Destination::Existing(name) => return Ok(TrackChoice::ExistingPackage(name)),
        Destination::NewStandalone => (
            crate::received::spec_name_suggestion(&suggest_name(file)),
            Blank::Allowed,
        ),
        Destination::TypeAName => (
            Suggestion::Nothing,
            Blank::Refused(lit!("Spec name cannot be empty.")),
        ),
    };

    Ok(match prompt_for_name(suggested, blank, display)? {
        Some(name) => TrackChoice::NewStandalone(name),
        None => TrackChoice::Declined,
    })
}

/// Prompt for a name for the new dotfile spec, offering `suggested`, with a
/// blank answer handled as `blank` says. `None` for a blank answer it allows,
/// which declines.
///
/// # Errors
///
/// [`PromptFailure`] when the prompt got no answer.
fn prompt_for_name(
    suggested: Suggestion,
    blank: Blank,
    display: &DisplayManager,
) -> Result<Option<String>, PromptFailure> {
    let name = display.input(lit!("Name for the new dotfile spec"), suggested, blank)?;
    Ok(Some(name).filter(|name| !Name::is_blank(name)))
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

/// What to tell the user when the dotfiles directory would not list, or `None` when
/// there is nothing worth saying.
///
/// The scan's question is "does a spec here already track this file", and a directory
/// with no specs in it answers no. So an absence is not a warning on its own. What
/// earns one is an absence the user is about to be asked questions about: a standalone
/// entry is written into the dotfiles directory, and every state but a readable
/// directory refuses that write. Saying it before the prompt is what keeps the user
/// from choosing a name selfie cannot use.
fn dotfiles_absence_warning(config: &CliConfig, error: &PackageListError) -> Option<Shown> {
    let path = error.path();
    Some(match error.state().clone() {
        // A directory that classified cleanly and still would not list
        // could not be *listed*, which is the same answer an unlistable
        // one gets.
        DirectoryState::Directory => shown!(
            "The dotfiles directory at {} could not be listed, so selfie cannot tell whether a \
             spec in it already tracks this file: {}",
            DisplayPath::of(path),
            crate::received::sentence(error)
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
            let sentence = shown!(
                "The dotfiles directory at {} {}. A standalone dotfile cannot be tracked until \
                 that is fixed.",
                DisplayPath::of(path),
                crate::received::composed(&reason)
            );
            match crate::formatters::mkdir_command(path).filter(|_| reason.creatable()) {
                Some(mkdir) => crate::terminal_text::noted(
                    shown!("{} Create it with: {}", sentence, mkdir),
                    &mkdir,
                ),
                None => sentence,
            }
        }
        DirectoryState::Unlistable(_) => shown!(
            "The dotfiles directory could not be listed, so selfie cannot tell whether a spec in \
             it already tracks this file: {} — {}",
            DisplayPath::of(path),
            crate::received::sentence(error)
        ),
        DirectoryState::Unknown(_) => shown!(
            "The dotfiles directory could not be checked: {} — {}",
            DisplayPath::of(path),
            crate::received::sentence(error)
        ),
    })
}

/// What to tell the user when the package directory would not list.
///
/// Unlike the dotfiles directory, the package directory is always expected, so
/// every state earns a warning.
fn package_listing_warning(error: &PackageListError) -> Shown {
    // The error's own sentence starts with the path, so it is not named twice.
    shown!(
        "The package directory {}, so its specs were not checked for an entry that already \
         tracks this file",
        crate::received::sentence(error)
    )
}

/// Find every package or standalone dotfile spec that already tracks a file.
///
/// Scans both the packages directory and the dotfiles directory for a dotfile
/// entry whose target matches the given file path. Returns each spec that tracks
/// it, with the entry's own target, in the order scanned, paired with a
/// warning for every spec it could not read, for a package directory it could not
/// list, and for a dotfiles directory it could not read or write a new entry into.
///
/// A caller that ignores the second list is treating "nothing selfie could read
/// tracks this file" as "nothing tracks it", and a spec it could not read may
/// already carry the entry.
///
/// The entry's target rather than the argument, because the two differ: the spec
/// holds `~/…` and the caller may pass an absolute path for the same file.
fn find_existing_trackers(
    file: &str,
    config: &CliConfig,
    dotfiles_repo: &YamlPackageRepository<RealFileSystem>,
) -> (Vec<Tracker>, Vec<Shown>) {
    let fs = RealFileSystem;
    let expanded = selfie::fs::expand_target_path(&fs, file);
    let mut skipped = Vec::new();
    let mut packages: Vec<selfie::package::Package> = Vec::new();
    // The two directories may be one, or one may link to the other, so a spec
    // file is counted once however many scans list it.
    let mut seen = HashSet::new();

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
    // The names the package directory claims, which set aside a dotfiles spec of
    // the same name: apply does not use it, so it tracks nothing.
    let mut claimed = std::collections::BTreeSet::new();

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
            skipped.push(crate::received::skipped_spec(invalid));
        }
        let listing = output.names();
        // A dotfiles spec under a name the package directory claims is set aside,
        // as apply sets it aside, without a word.
        let set_aside = |name: &str| !is_package_directory && claimed.contains(name);
        // A name several files claim is used by no command, but any of its files
        // may track this one, so each is named as unchecked.
        for row in &listing.rows {
            if !set_aside(&row.package_name) {
                skipped.push(crate::received::ambiguous_spec(row));
            }
        }

        // Every spec apply uses, not the first: a file two specs track is one the
        // user has to hear about twice. The library's matcher, so this and the
        // track commands find the same trackers.
        for pkg in listing.usable {
            let set_aside = pkg.spec_name().is_some_and(|name| set_aside(&name));
            if !set_aside
                && seen.insert(
                    std::fs::canonicalize(pkg.path()).unwrap_or_else(|_| pkg.path().clone()),
                )
            {
                packages.push(pkg.clone());
            }
        }
        if is_package_directory {
            claimed = listing.claimed;
        }
    }
    let trackers = selfie::dotfile_service::track::trackers_of(&packages, &fs, &expanded);
    (trackers, skipped)
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
fn load_package_names(repo: &impl PackageRepository) -> Result<(Vec<String>, Vec<Shown>), Shown> {
    match common::package_names_and_skipped(repo) {
        Ok(loaded) => Ok(loaded),
        // A package directory that does not exist yet holds no packages, and the
        // file can still go into a new standalone spec. Anything else at the path
        // is selfie unable to look, which may not be offered as "nothing there".
        Err(listing) if listing.may_be_created() => Ok((Vec::new(), Vec::new())),
        Err(e) => Err(shown!(
            "Failed to list packages: {}",
            crate::received::sentence(e)
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_undeployable_tracker_fails_the_run() {
        let temp = tempfile::tempdir().unwrap();
        let display = DisplayManager::new(false);

        let exit = report_existing_trackers(
            &[Tracker {
                spec_name: "only".to_string(),
                spec_path: temp.path().join("only.yml"),
                target: "relative/rc".to_string(),
                origin: selfie::package::SpecOrigin::PackageDirectory,
            }],
            &display,
        );

        assert_eq!(exit, Exit::Failed, "{:?}", display.printed());
    }

    // Every tracker is reported even when the first one fails, so a fold that
    // stops at the first failure would leave the second spec unnamed.
    #[test]
    fn every_tracker_is_reported_after_an_undeployable_one() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("rc");
        std::fs::write(&file, "x").unwrap();
        let tracker = |spec_name: &str, target: &str| Tracker {
            spec_name: spec_name.to_string(),
            spec_path: temp.path().join(format!("{spec_name}.yml")),
            target: target.to_string(),
            origin: selfie::package::SpecOrigin::PackageDirectory,
        };
        let display = DisplayManager::new(false);

        let exit = report_existing_trackers(
            &[
                tracker("first", "relative/rc"),
                tracker("second", &file.display().to_string()),
            ],
            &display,
        );

        assert_eq!(exit, Exit::Failed);
        let printed: Vec<String> = display
            .printed()
            .into_iter()
            .map(|(_, line)| line)
            .collect();
        let count = |needle: &str| printed.iter().filter(|line| line.contains(needle)).count();
        assert_eq!(
            count("'relative/rc' is tracked in spec 'first', but"),
            1,
            "{printed:?}"
        );
        assert_eq!(count("in spec 'second'"), 1, "{printed:?}");
        assert_eq!(count("2 specs track this file"), 1, "{printed:?}");
    }

    // A derived name the prompt can only show escaped is noted above the
    // prompt, and Enter alone declines rather than tracking under it.
    #[tokio::test]
    async fn an_escaped_suggestion_is_noted_and_enter_declines() {
        use crate::display_manager::answer;

        let (temp, _file, config) = untracked_file();
        let file = temp.path().join(".bat\u{200b}rc");
        std::fs::write(&file, "x").unwrap();
        // With no packages, index 0 is "New standalone dotfile".
        let display = DisplayManager::new(false)
            .answering(vec![answer(Some(0_usize)), answer(String::new())]);

        let exit = handle_track(
            file.to_str().unwrap(),
            &config,
            &display,
            CancellationToken::new(),
        )
        .await;

        assert_eq!(exit, Exit::Failed, "{:?}", display.printed());
        let notes = display
            .printed()
            .iter()
            .filter(|(_, line)| line.contains(r"Suggested: bat\u{200b}rc (shown escaped"))
            .count();
        assert_eq!(notes, 1, "{:?}", display.printed());
    }

    // Picking a package the picker shows escaped tracks into it by its raw name.
    #[test]
    fn a_picked_package_is_tracked_into_by_its_raw_name() {
        let names = vec!["plain".to_string(), "evil\u{1b}[2J".to_string()];
        let display = DisplayManager::new(false)
            .answering(vec![crate::display_manager::answer(Some(1_usize))]);

        let choice = prompt_track_choice(&names, "~/.batrc", &display).unwrap();

        assert_eq!(
            choice,
            TrackChoice::ExistingPackage("evil\u{1b}[2J".to_string())
        );
    }

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
    fn find_existing_trackers_reports_a_spec_it_could_not_read() {
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
            find_existing_trackers("~/.config/fish/config.fish", &config, &dotfiles_repo);

        assert!(found.is_empty(), "nothing readable tracks that file");
        assert_eq!(skipped.len(), 1, "the unreadable spec must be reported");
        assert!(skipped[0].contains("brokenpkg.yaml"), "got: {}", skipped[0]);
    }

    // A dotfiles spec a package spec of the same name sets aside deploys nothing,
    // and neither does a file of a name two files claim, so neither is a tracker.
    // The ambiguous files are named as unchecked instead.
    #[test]
    fn find_existing_trackers_counts_only_specs_apply_uses() {
        use selfie::config::SelfieConfigBuilder;

        let temp = tempfile::TempDir::new().unwrap();
        let packages = temp.path().join("packages");
        let dotfiles = temp.path().join("dotfiles");
        std::fs::create_dir_all(&packages).unwrap();
        std::fs::create_dir_all(&dotfiles).unwrap();
        let entry = "dotfiles:\n  - source: x/rc\n    target: ~/.config/fish/config.fish\n";
        std::fs::write(
            packages.join("fish.yml"),
            "name: fish\nenvironments:\n  test-env:\n    install: \"true\"\n",
        )
        .unwrap();
        std::fs::write(dotfiles.join("fish.yml"), format!("name: fish\n{entry}")).unwrap();
        for ext in ["yml", "yaml"] {
            std::fs::write(
                packages.join(format!("dup.{ext}")),
                format!("name: dup\n{entry}environments:\n  test-env:\n    install: \"true\"\n"),
            )
            .unwrap();
        }
        let dotfiles_repo =
            YamlPackageRepository::new(RealFileSystem, dotfiles, SpecOrigin::DotfilesDirectory);
        let config = CliConfig::wrap_for_test(
            SelfieConfigBuilder::default()
                .environment("test-env")
                .package_directory(packages)
                .build(),
        );

        let (found, skipped) =
            find_existing_trackers("~/.config/fish/config.fish", &config, &dotfiles_repo);

        assert!(found.is_empty(), "{found:?}");
        assert_eq!(skipped.len(), 2, "{skipped:?}");
        assert!(
            skipped.iter().all(|w| w.contains("dup.yaml, dup.yml")),
            "{skipped:?}"
        );
    }

    // Dotfiles files claiming one name are set aside whole when a package spec has
    // the name, as apply sets them aside, so they are not named as unchecked.
    #[test]
    fn find_existing_trackers_is_silent_about_a_set_aside_ambiguity() {
        use selfie::config::SelfieConfigBuilder;

        let temp = tempfile::TempDir::new().unwrap();
        let packages = temp.path().join("packages");
        let dotfiles = temp.path().join("dotfiles");
        std::fs::create_dir_all(&packages).unwrap();
        std::fs::create_dir_all(&dotfiles).unwrap();
        std::fs::write(
            packages.join("fish.yml"),
            "name: fish\nenvironments:\n  test-env:\n    install: \"true\"\n",
        )
        .unwrap();
        for ext in ["yml", "yaml"] {
            std::fs::write(dotfiles.join(format!("fish.{ext}")), "name: fish\n").unwrap();
        }
        let dotfiles_repo =
            YamlPackageRepository::new(RealFileSystem, dotfiles, SpecOrigin::DotfilesDirectory);
        let config = CliConfig::wrap_for_test(
            SelfieConfigBuilder::default()
                .environment("test-env")
                .package_directory(packages)
                .build(),
        );

        let (found, skipped) =
            find_existing_trackers("~/.config/fish/config.fish", &config, &dotfiles_repo);

        assert!(found.is_empty(), "{found:?}");
        assert!(skipped.is_empty(), "{skipped:?}");
    }

    // The control: with every spec readable, a run that finds no tracker has
    // nothing to report, so a `skipped` that is never empty would fail here.
    #[test]
    fn find_existing_trackers_reports_nothing_for_a_clean_directory() {
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
            find_existing_trackers("~/.config/fish/config.fish", &config, &dotfiles_repo);

        assert!(found.is_empty());
        assert!(skipped.is_empty(), "got: {skipped:?}");
    }

    // The package directory is always expected, so one that will not list is a
    // place the scan could not check, and the answer says so.
    #[test]
    fn find_existing_trackers_warns_about_a_package_directory_that_is_not_there() {
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
            find_existing_trackers("~/.config/fish/config.fish", &config, &dotfiles_repo);

        assert!(found.is_empty());
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

    // An absent dotfiles directory whose path holds an invisible character ends
    // its warning with the `mkdir` that creates it, and the escaped form of that
    // command follows on the next line.
    #[test]
    fn an_absent_dotfiles_directory_reading_as_another_notes_its_remedy() {
        use selfie::config::SelfieConfigBuilder;

        let path = std::path::PathBuf::from("/nonexistent/dot\u{200B}files");
        let config = CliConfig::wrap_for_test(
            SelfieConfigBuilder::default()
                .environment("test-env")
                .package_directory(std::path::PathBuf::from("/nonexistent/packages"))
                .dotfiles_directory(path.clone())
                .build(),
        );
        let error = PackageListError::new(
            path,
            DirectoryState::Absent(selfie::fs::AbsentReason::Empty),
        );

        let warning = dotfiles_absence_warning(&config, &error)
            .expect("an absence the user named warns")
            .into_string();

        let lines: Vec<&str> = warning.lines().collect();
        assert_eq!(lines.len(), 2, "{warning}");
        assert!(
            lines[0].ends_with("Create it with: mkdir -p -- '/nonexistent/dot\u{200B}files'"),
            "{warning}"
        );
        assert_eq!(
            lines[1], "Escaped: mkdir -p -- '/nonexistent/dot\\u{200b}files'",
            "{warning}"
        );
    }

    // A dotfiles directory that is not there holds no spec that could track the
    // file, so it is not something the scan failed to check.
    #[test]
    fn find_existing_trackers_is_silent_about_a_dotfiles_directory_that_is_not_there() {
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
            find_existing_trackers("~/.config/fish/config.fish", &config, &dotfiles_repo);

        assert!(found.is_empty());
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
                    .map(|name| {
                        PackageBuilder::default()
                            .name(name)
                            .path(format!("/packages/{name}.yml"))
                            .build()
                    })
                    .collect(),
            ))
        });

        let (names, skipped) = load_package_names(&repo).unwrap();
        assert_eq!(names, vec!["alacritty", "fnm", "zsh"]);
        assert!(skipped.is_empty());
    }

    // A name two files claim is refused by every command, so the picker does not
    // offer it, and says why.
    #[test]
    fn the_track_picker_does_not_offer_an_ambiguous_name() {
        use selfie::package::PackageBuilder;
        use selfie::package::port::{ListPackagesOutput, MockPackageRepository};

        let mut repo = MockPackageRepository::new();
        repo.expect_list_packages().returning(|| {
            Ok(ListPackagesOutput::from_packages(
                ["bat.yml", "bat.yaml", "fnm.yml"]
                    .into_iter()
                    .map(|file| {
                        PackageBuilder::default()
                            .name(file.split('.').next().unwrap())
                            .path(format!("/packages/{file}"))
                            .build()
                    })
                    .collect(),
            ))
        });

        let (names, skipped) = load_package_names(&repo).unwrap();
        assert_eq!(names, vec!["fnm"]);
        assert_eq!(skipped.len(), 2, "{skipped:?}");
        assert!(
            skipped.iter().all(|w| w.contains("bat.yaml, bat.yml")),
            "{skipped:?}"
        );
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
                Ok(PackageBuilder::default()
                    .name("fnm")
                    .path("/test/packages/fnm.yml")
                    .build()),
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

        let exit = handle_track(&file, &config, &display, CancellationToken::new()).await;

        assert_eq!(exit, Exit::Failed);
    }

    // A blank answer under the file's own name is a decline too: the track
    // says it was canceled, and no name reaches the spec-name rule.
    #[tokio::test]
    async fn a_blank_name_fails() {
        use crate::display_manager::answer;

        // With no packages, index 0 is "New standalone dotfile".
        let (_temp, file, config) = untracked_file();
        let display = DisplayManager::new(false)
            .answering(vec![answer(Some(0_usize)), answer("  ".to_string())]);

        let exit = handle_track(&file, &config, &display, CancellationToken::new()).await;

        // Declined, not passed on as a name to be refused, which also fails.
        assert_eq!(exit, Exit::Failed);
        let lines = |needle: &str| {
            display
                .printed()
                .iter()
                .filter(|(_, line)| line.contains(needle))
                .count()
        };
        assert_eq!(lines("Canceled."), 1, "{:?}", display.printed());
        assert_eq!(lines("not a valid spec name"), 0, "{:?}", display.printed());
    }

    // Asked to type a name, a blank answer is asked again rather than
    // declining the track.
    #[test]
    fn a_blank_typed_name_is_asked_again() {
        use crate::display_manager::answer;

        // After the one package, index 2 is "Let me type a name".
        let display = DisplayManager::new(false).answering(vec![
            answer(Some(2_usize)),
            answer(String::new()),
            answer("mine".to_string()),
        ]);

        let choice = prompt_track_choice(&["bat".to_string()], "~/.batrc", &display).unwrap();

        assert_eq!(choice, TrackChoice::NewStandalone("mine".to_string()));
        let refusals = display
            .printed()
            .iter()
            .filter(|(_, line)| line.contains("Spec name cannot be empty."))
            .count();
        assert_eq!(refusals, 1, "{:?}", display.printed());
    }

    // Ctrl+C at the destination picker ends the run canceled.
    #[test]
    fn ctrl_c_at_the_destination_picker_is_canceled() {
        let display = DisplayManager::new(false).answering(vec![crate::display_manager::ctrl_c()]);

        let failure = prompt_track_choice(&["bat".to_string()], "~/.batrc", &display).unwrap_err();

        assert_eq!(failure.exit(), crate::event_processor::Exit::Canceled);
    }

    // Ctrl+C at the name prompt ends the run canceled, not declined.
    #[test]
    fn ctrl_c_at_the_name_prompt_is_canceled() {
        use crate::display_manager::{answer, ctrl_c};

        // Index 1 is the "new standalone dotfile" entry after the one package.
        let display = DisplayManager::new(false).answering(vec![answer(Some(1_usize)), ctrl_c()]);

        let failure = prompt_track_choice(&["bat".to_string()], "~/.batrc", &display).unwrap_err();

        assert_eq!(failure.exit(), crate::event_processor::Exit::Canceled);
    }

    #[test]
    fn resolve_choice_selects_existing_package() {
        let result = resolve_choice(
            Destination::Existing("alacritty".to_string()),
            "~/.config/test.toml",
            &DisplayManager::new(false),
        );
        assert_eq!(
            result.unwrap(),
            TrackChoice::ExistingPackage("alacritty".to_string())
        );
    }
}
