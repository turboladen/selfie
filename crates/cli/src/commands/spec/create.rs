use crate::terminal_text::{DisplayPath, Name, Shown, lit, shown, then_command};
use selfie::{
    namespace::{self, NamespaceValidationError},
    package::{
        EnvironmentConfig, Environments, SpecService,
        event::{OperationResult, OperationSuccess, PackageEvent},
        port::PackageRepository,
    },
};
use std::path::PathBuf;
use tracing::info;

use crate::formatters::selfie_command;

use crate::{
    config::CliConfig,
    display_manager::{
        Blank, Choice, DisplayManager, PromptFailure, Suggestion, is_blank_command_answer,
    },
    event_processor::{EventProcessor, Exit},
};

use crate::commands::common;

const MAX_NAME_RETRIES: usize = 3;

enum PackageNameResult {
    CreateNew(String),     // Use this name to create a new package
    EditExisting(PathBuf), // User wants to edit the existing package at this path
    Canceled,              // User canceled the operation
}

pub(crate) async fn handle_create(
    service: &impl SpecService,
    package_name: &str,
    config: &CliConfig,
    display: &DisplayManager,
    interactive: bool,
) -> Exit {
    info!("Creating package: {}", package_name);

    // Refused before the name check, so a run that cannot ask prints only why.
    if interactive && !display.can_prompt() {
        return refuse_interactive(display, &PromptFailure::NoTerminal);
    }

    // Create repository for name validation (UI flow decisions)
    let repo = common::create_package_repository(config);

    // Asked first, so the name checked below is the one the spec is written
    // under: the check never sees a name the prompts change afterwards.
    let asked_name;
    let package_name = if interactive {
        display.print_run_note(lit!("Creating package interactively..."));
        asked_name = match prompt_package_name(package_name, display) {
            Ok(name) => name,
            Err(exit) => return exit,
        };
        asked_name.as_str()
    } else {
        package_name
    };

    // Get a valid package name or handle existing package scenarios
    let package_name = match get_valid_package_name(package_name, &repo, config, display) {
        Ok(PackageNameResult::CreateNew(name)) => name,
        Ok(PackageNameResult::EditExisting(path)) => {
            display.print_run_note(shown!(
                "Opening existing package for editing at {}",
                DisplayPath::of(&path)
            ));
            let success_message = shown!("Package editing completed at {}", DisplayPath::of(&path));
            return common::open_editor(&path, display, Some(success_message));
        }
        // A create that wrote nothing did not do what it was asked, so a script
        // must not read it as success.
        Ok(PackageNameResult::Canceled) => {
            display.print_run_note(lit!("Package creation canceled."));
            return Exit::Failed;
        }
        Err(exit) => return exit,
    };

    // Build the Package (CLI handles interactive prompting)
    let package = if interactive {
        match create_package_interactive(&package_name, config, display) {
            Ok(pkg) => pkg,
            Err(exit) => return exit,
        }
    } else {
        create_basic_package(&package_name, config)
    };

    let (created_name, file_path, created_exit) = match write_spec(service, package, display).await
    {
        Ok(created) => created,
        Err(exit) => return exit,
    };

    if !interactive {
        display.print_info(lit!(
            "Package created. Use 'selfie spec edit' to customize it."
        ));
        return created_exit;
    }

    let edit_now = display.confirm(
        lit!("Would you like to open the package file for editing now?"),
        true,
    );

    let edit_later = || {
        display.print_info(then_command(
            lit!("Package created. You can edit it later with `selfie spec edit`.").into(),
            selfie_command(&[lit!("spec"), lit!("edit")], &created_name, &[]).as_ref(),
        ));
    };
    match edit_now {
        // Asked for, so the editor's failure is the run's; a create's own score
        // survives a successful edit.
        Ok(true) => {
            let success_message = shown!(
                "Package '{}' created and saved at {}",
                Name::new(&created_name),
                DisplayPath::of(&file_path)
            );
            created_exit.max(common::open_editor(
                &file_path,
                display,
                Some(success_message),
            ))
        }
        Ok(false) => {
            display.print_info(lit!(
                "Package created. You can edit it later with 'selfie spec edit'."
            ));
            created_exit
        }
        // The spec is already written, so only Ctrl+C changes how the run ends: a
        // question that could not be asked or answered was not needed. Either
        // way the user hears that the spec exists.
        Err(PromptFailure::Interrupted) => {
            edit_later();
            Exit::Canceled
        }
        Err(failure) => {
            display.note_unanswered_prompt(&failure);
            edit_later();
            created_exit
        }
    }
}

/// Write `package` through the service, printing what the run reports, and
/// return the name and path the spec was created under, with how the create
/// scored.
///
/// # Errors
///
/// The [`Exit`] when the spec was not created: [`Exit::Failed`],
/// [`Exit::Canceled`] or [`Exit::Usage`]. A create that scored
/// [`Exit::Clean`] or [`Exit::Found`] returns `Ok` with that score.
pub(super) async fn write_spec(
    service: &impl SpecService,
    package: selfie::package::Package,
    display: &DisplayManager,
) -> Result<(String, PathBuf, Exit), Exit> {
    let event_stream = service.create(package).await;

    // The name and path the library created the spec under. The name is the
    // file's, which the library judges the spec by.
    let mut created: Option<(String, PathBuf)> = None;
    let processor = EventProcessor::new(display.clone());
    let result = processor
        .process_events(event_stream, |event| match event {
            PackageEvent::Completed {
                result:
                    OperationResult::Success(OperationSuccess::PackageCreated {
                        package_name,
                        file_path,
                        ..
                    }),
                ..
            } => {
                created = Some((package_name.clone(), file_path.clone()));
                false // Let default handler print success message
            }
            PackageEvent::ValidationResultCompleted {
                validation_result, ..
            } => {
                // The spec is still written, and the answer on stdout is that it was
                // created, so the issues go to stderr.
                crate::commands::validation_display::print_issues(
                    display,
                    &validation_result.issues,
                );
                true
            }
            _ => false, // Default handling for everything else
        })
        .await;

    match result.exit {
        Exit::Failed | Exit::Canceled | Exit::Usage => return Err(result.exit),
        Exit::Clean | Exit::Found => {}
    }
    created
        .map(|(name, path)| (name, path, result.exit))
        .ok_or(Exit::Failed)
}

/// Why a spec cannot be created under `name`, given the namespace check's
/// refusal.
pub(super) fn name_check_refusal(name: &str, error: &NamespaceValidationError) -> Shown {
    match error {
        NamespaceValidationError::Conflict(conflict) => {
            shown!(
                "{} Choose a different name.",
                crate::received::sentence(conflict)
            )
        }
        // The directory is the problem, not the name.
        NamespaceValidationError::PackageDirectoryUnreadable(_)
        | NamespaceValidationError::DotfilesDirectoryUnreadable(_) => shown!(
            "Cannot create '{}': {}",
            Name::new(name),
            crate::received::composed(error)
        ),
    }
}

/// Report a prompt of `spec create --interactive` that got no answer, and return
/// the [`Exit`].
fn refuse_interactive(display: &DisplayManager, failure: &PromptFailure) -> Exit {
    display.refuse_prompt(
        failure,
        lit!("spec create --interactive"),
        lit!("Leave off --interactive to write a template, then edit it with 'selfie spec edit'."),
    )
}

/// What to do about a package already under the name asked for.
#[derive(Clone, Copy)]
enum Existing {
    Edit,
    Rename,
    Cancel,
}

fn get_valid_package_name(
    initial_name: &str,
    repo: &impl PackageRepository,
    config: &CliConfig,
    display: &DisplayManager,
) -> Result<PackageNameResult, Exit> {
    let mut current_name = initial_name.to_string();
    let mut retry_count = 0;

    // A dotfiles directory that is genuinely not there holds no names. One that
    // will not read cannot answer, and the check refuses on it rather than letting
    // a spec be created beside a name that may already exist.
    let dotfiles_repo = common::create_dotfiles_repository(config);

    loop {
        // Check namespace conflict (packages + dotfiles directories)
        match namespace::validate_unique_name(&current_name, repo, Some(&dotfiles_repo)) {
            // Not a retry with a different name: the directory is the problem, not
            // the name, so prompting again would ask the user to guess their way
            // past an unreadable directory.
            Err(
                error @ (NamespaceValidationError::PackageDirectoryUnreadable(_)
                | NamespaceValidationError::DotfilesDirectoryUnreadable(_)),
            ) => {
                display.print_error(name_check_refusal(&current_name, &error));
                return Err(Exit::Failed);
            }
            Err(NamespaceValidationError::Conflict(conflict)) => {
                // Only a package the check found, and this command can load, is
                // offered for editing. Where the name was found is the check's
                // answer; a failed load does not move it to the dotfiles directory.
                let editable = match conflict.found_in {
                    namespace::NameLocation::Packages => repo.get_package(&current_name).ok(),
                    namespace::NameLocation::Dotfiles => None,
                };
                if let Some(existing_package) = editable {
                    display.print_warning(shown!(
                        "{} Edit it, or choose a different name.",
                        crate::received::sentence(&conflict)
                    ));

                    let action = display.select(
                        lit!("What would you like to do?"),
                        &[
                            Choice::labeled(lit!("Edit the existing package"), Existing::Edit),
                            Choice::labeled(
                                lit!("Create a new package with a different name"),
                                Existing::Rename,
                            ),
                            Choice::labeled(lit!("Cancel"), Existing::Cancel),
                        ],
                    );

                    match action {
                        Ok(Existing::Edit) => {
                            return Ok(PackageNameResult::EditExisting(
                                existing_package.file_path().to_path_buf(),
                            ));
                        }
                        Ok(Existing::Rename) => {
                            // Fall through to prompt for new name below
                        }
                        Ok(Existing::Cancel) => return Ok(PackageNameResult::Canceled),
                        Err(failure) => {
                            return Err(display.refuse_prompt(
                                &failure,
                                lit!("Choosing what to do about an existing package"),
                                then_command(
                                    lit!(
                                        "Edit it with `selfie spec edit`, or run `selfie spec \
                                         create` with a different name."
                                    )
                                    .into(),
                                    selfie_command(
                                        &[lit!("spec"), lit!("edit")],
                                        &current_name,
                                        &[],
                                    )
                                    .as_ref(),
                                ),
                            ));
                        }
                    }
                } else {
                    // Not a package this command can edit, so a different name is
                    // the way out. Only a single package spec could have been
                    // edited, so only it failed to load.
                    let why = match conflict.remedy() {
                        namespace::Remedy::UseThePackage => {
                            lit!(" It could not be loaded, so it cannot be edited here.")
                        }
                        namespace::Remedy::ChooseAnotherName
                        | namespace::Remedy::RemoveTheDotfileSpec => lit!(""),
                    };
                    display.print_warning(shown!(
                        "{}{} Choose a different name.",
                        crate::received::sentence(&conflict),
                        why
                    ));
                }

                // Prompt for a new name
                retry_count += 1;
                if retry_count > MAX_NAME_RETRIES {
                    display.print_error(shown!(
                        "Too many retry attempts ({}). Please try again later.",
                        MAX_NAME_RETRIES
                    ));
                    return Err(Exit::Failed);
                }

                current_name = loop {
                    let name = match display.input(
                        shown!(
                            "Enter a new package name (attempt {}/{})",
                            retry_count,
                            MAX_NAME_RETRIES
                        ),
                        Suggestion::Nothing,
                        Blank::Refused(lit!("Package name cannot be empty.")),
                    ) {
                        Ok(name) => name,
                        Err(failure) => {
                            return Err(display.refuse_prompt(
                                &failure,
                                lit!("Choosing a different name"),
                                lit!("Run 'selfie spec create' again with a different name."),
                            ));
                        }
                    };
                    // As the first name prompt does: a name the rule refuses would
                    // be refused only when the spec is written.
                    match crate::received::invalid_name_refusal(&name) {
                        None => break name,
                        Some(refusal) => display.print_warning(refusal),
                    }
                };
                continue;
            }
            Ok(()) => {
                // Name is unique — proceed with creation
                return Ok(PackageNameResult::CreateNew(current_name));
            }
        }
    }
}

/// The template spec for `package_name`: one environment, the configured one,
/// with placeholder commands, written to `<name>.yml` in the package directory.
pub(super) fn create_basic_package(
    package_name: &str,
    config: &CliConfig,
) -> selfie::package::Package {
    let mut environments = Environments::new();

    // Use the environment from config (which may be overridden by --environment)
    let env_name = config.environment();
    let env_config = EnvironmentConfig::new(
        format!("# TODO: Add install command for {package_name}"),
        Some(format!("# TODO: Add check command for {package_name}")),
        None,
        Vec::new(),
        Vec::new(),
    );

    environments.insert(env_name.to_string(), env_config);

    selfie::package::Package::new(
        package_name.to_string(),
        None,
        None,
        Vec::new(),
        None,
        environments,
        config
            .package_directory()
            .join(format!("{package_name}.yml")),
    )
}

fn create_package_interactive(
    package_name: &str,
    config: &CliConfig,
    display: &DisplayManager,
) -> Result<selfie::package::Package, Exit> {
    let homepage = prompt_package_homepage(display)?;
    let description = prompt_package_description(display)?;
    let environments = prompt_environments(package_name, config, display)?;

    // The file is named for the package, as `create_basic_package` names it:
    // selfie finds a spec by its file name, so a separate one would be a name
    // the namespace check never saw.
    Ok(selfie::package::Package::new(
        package_name.to_string(),
        homepage,
        description,
        Vec::new(),
        None,
        environments,
        config
            .package_directory()
            .join(format!("{package_name}.yml")),
    ))
}

fn prompt_package_name(default_name: &str, display: &DisplayManager) -> Result<String, Exit> {
    loop {
        let name = display
            .input(
                lit!("Package name"),
                crate::received::spec_name_suggestion(default_name),
                Blank::Refused(lit!("Package name cannot be empty.")),
            )
            .map_err(|failure| refuse_interactive(display, &failure))?;

        // Here, not only when the spec is written: every answer after this one
        // would be thrown away with it.
        match crate::received::invalid_name_refusal(&name) {
            None => return Ok(name),
            Some(refusal) => display.print_warning(refusal),
        }
    }
}

fn prompt_package_homepage(display: &DisplayManager) -> Result<Option<String>, Exit> {
    let homepage = display
        .input(
            lit!("Homepage URL (optional)"),
            Suggestion::Nothing,
            Blank::Allowed,
        )
        .map_err(|failure| refuse_interactive(display, &failure))?;

    Ok(if homepage.trim().is_empty() {
        None
    } else {
        Some(homepage)
    })
}

fn prompt_package_description(display: &DisplayManager) -> Result<Option<String>, Exit> {
    let description = display
        .input(
            lit!("Description (optional)"),
            Suggestion::Nothing,
            Blank::Allowed,
        )
        .map_err(|failure| refuse_interactive(display, &failure))?;

    Ok(if description.trim().is_empty() {
        None
    } else {
        Some(description)
    })
}

fn prompt_environments(
    package_name: &str,
    config: &CliConfig,
    display: &DisplayManager,
) -> Result<Environments, Exit> {
    let mut environments = Environments::new();

    loop {
        display.print_run_note(lit!("Adding environment configuration..."));

        let env_name = prompt_environment_name(&environments, config, display)?;
        let install_cmd = prompt_install_command(display)?;
        let check_cmd = prompt_check_command(package_name, display)?;
        let dependencies = prompt_dependencies(config, display)?;

        let env_config =
            EnvironmentConfig::new(install_cmd, check_cmd, None, dependencies, Vec::new());
        environments.insert(env_name, env_config);

        if !prompt_add_another_environment(display)? {
            break;
        }
    }

    Ok(environments)
}

/// The name to offer for the next environment: the configured one, then
/// "production", and none once the spec has both.
fn default_environment_name<'a>(
    existing_environments: &Environments,
    config: &'a CliConfig,
) -> Option<&'a str> {
    // A default the spec already has could only be refused.
    [config.environment(), "production"]
        .into_iter()
        .find(|name| !existing_environments.contains_key(*name))
}

fn prompt_environment_name(
    existing_environments: &Environments,
    config: &CliConfig,
    display: &DisplayManager,
) -> Result<String, Exit> {
    let default_env = default_environment_name(existing_environments, config);

    loop {
        // Keepable: an environment name is any YAML key, and the suggestion is
        // the configured environment, which the user chose.
        let suggested = default_env.map_or(Suggestion::Nothing, |name| {
            Suggestion::Keepable(name.to_string())
        });
        // Refused blank: cleared or never filled, the name would key the
        // environment `""`.
        let name = display
            .input(
                lit!("Environment name"),
                suggested,
                Blank::Refused(lit!("Environment name cannot be empty.")),
            )
            .map_err(|failure| refuse_interactive(display, &failure))?;

        // A second entry under one name would replace the first.
        if !existing_environments.contains_key(&name) {
            return Ok(name);
        }
        display.print_warning(shown!(
            "Environment '{}' is already in this spec. Choose a different name.",
            Name::new(&name)
        ));
    }
}

fn prompt_install_command(display: &DisplayManager) -> Result<String, Exit> {
    display
        .input(
            lit!("Install command (required)"),
            Suggestion::Nothing,
            Blank::RefusedCommand(lit!("Install command cannot be empty or only a comment.")),
        )
        .map_err(|failure| refuse_interactive(display, &failure))
}

fn prompt_check_command(
    package_name: &str,
    display: &DisplayManager,
) -> Result<Option<String>, Exit> {
    // Pre-filled, so clearing it leaves the spec with no check command.
    let check_cmd = display
        .input(
            lit!("Check command (optional)"),
            Suggestion::Keepable(format!("command -v {package_name}")),
            Blank::Allowed,
        )
        .map_err(|failure| refuse_interactive(display, &failure))?;

    // This judges blank as the install prompt does. A comment-only check is
    // saved as no check, which is what install and status make of it anyway,
    // and so is an answer showing nothing, which the library would hand to the
    // shell.
    Ok(if is_blank_command_answer(&check_cmd) {
        None
    } else {
        Some(check_cmd)
    })
}

/// The package names offerable as dependencies, sorted, with a warning for every
/// spec file that could not be read.
///
/// A package directory that is not there yet yields no names and no warnings,
/// so the first spec anyone writes can still be created.
///
/// # Errors
///
/// A message to display when the package directory is there and cannot be
/// listed.
// Split from the prompt below so the skipped files can be asserted. The prompt
// needs a terminal, so a test driving the binary cannot reach past it, and the
// dropped-silently case is the whole reason this reports anything at all.
fn available_dependency_names(
    repo: &impl PackageRepository,
) -> Result<(Vec<String>, Vec<Shown>), Shown> {
    match common::package_names_and_skipped(repo) {
        Ok(loaded) => Ok(loaded),
        // The first package anyone writes has no directory to list yet, and
        // having nothing to depend on is the right answer for it. Any other
        // failure is selfie unable to look, or a path the spec cannot be written
        // under, and neither may be offered as "there is nothing there".
        Err(listing) if listing.may_be_created() => Ok((Vec::new(), Vec::new())),
        Err(e) => Err(shown!(
            "Failed to list packages: {}",
            crate::received::composed(&e)
        )),
    }
}

fn prompt_dependencies(config: &CliConfig, display: &DisplayManager) -> Result<Vec<String>, Exit> {
    let repo = common::create_package_repository(config);

    let (available_packages, skipped) = available_dependency_names(&repo).map_err(|msg| {
        display.print_error(msg);
        Exit::Failed
    })?;

    // A spec selfie could not read is not offered: nothing says it would load
    // when the dependency is resolved. Naming these stops the picker reading as
    // every package the user has.
    for warning in skipped {
        display.print_warning(warning);
    }

    if available_packages.is_empty() {
        return Ok(Vec::new());
    }

    let choices: Vec<Choice<String>> = available_packages
        .iter()
        .map(|name| Choice::named(name, name.clone()))
        .collect();
    display
        .multi_select(
            lit!("Dependencies (select with space, confirm with enter)"),
            &choices,
        )
        .map_err(|failure| refuse_interactive(display, &failure))
}

fn prompt_add_another_environment(display: &DisplayManager) -> Result<bool, Exit> {
    display
        .confirm(lit!("Add another environment?"), false)
        .map_err(|failure| refuse_interactive(display, &failure))
}

#[cfg(test)]
mod tests {
    use selfie::package::port::PackageListError;

    use super::*;
    use futures::StreamExt;
    use selfie::package::SpecService;
    use selfie::package::event::{OperationResult, OperationSuccess, PackageEvent};
    use selfie::package::port::MockPackageRepository;
    use std::path::PathBuf;
    use test_common::{test_config_with_dir, test_config_with_dir_and_env};

    // A fresh package directory holding `existing` specs, a config naming it,
    // and a service over it.
    fn packages_holding(
        existing: &[&str],
    ) -> (tempfile::TempDir, PathBuf, CliConfig, impl SpecService) {
        let temp = tempfile::tempdir().unwrap();
        let packages = temp.path().join("packages");
        std::fs::create_dir(&packages).unwrap();
        for name in existing {
            std::fs::write(
                packages.join(format!("{name}.yml")),
                format!(
                    "name: {name}\nenvironments:\n  {}:\n    install: \"true\"\n",
                    test_common::TEST_ENV
                ),
            )
            .unwrap();
        }
        let config = CliConfig::wrap_for_test(test_config_with_dir(&packages));
        let service = test_common::create_test_service_with_config(test_config_with_dir(&packages));
        (temp, packages, config, service)
    }

    // A different name chosen at the "already exists" menu is held to the
    // spec-name rule as the first one is, and asked for again.
    #[tokio::test]
    async fn a_replacement_name_the_rule_refuses_is_asked_again() {
        use crate::display_manager::answer;

        let (_temp, packages, config, service) = packages_holding(&["tool"]);
        let display = DisplayManager::new(false).answering(vec![
            answer(1_usize), // a different name
            answer("my\u{200B}app".to_string()),
            answer("fresh".to_string()),
        ]);

        let exit = handle_create(&service, "tool", &config, &display, false).await;

        assert_eq!(exit, Exit::Clean, "{:?}", display.printed());
        assert!(packages.join("fresh.yml").exists());
        assert!(
            display
                .printed()
                .iter()
                .any(|(_, line)| line.contains(r"'my\u{200b}app': it is not a valid spec name")),
            "{:?}",
            display.printed()
        );
    }

    // Ctrl+C at the "already exists" menu ends the run canceled.
    #[tokio::test]
    async fn ctrl_c_at_the_taken_name_menu_is_canceled() {
        let (_temp, packages, config, service) = packages_holding(&["tool"]);
        let display = DisplayManager::new(false).answering(vec![crate::display_manager::ctrl_c()]);

        let exit = handle_create(&service, "tool", &config, &display, false).await;

        assert_eq!(exit, Exit::Canceled);
        assert_eq!(std::fs::read_dir(&packages).unwrap().count(), 1);
    }

    // Ctrl+C at the first interactive prompt ends the run canceled, writing
    // nothing.
    #[tokio::test]
    async fn ctrl_c_at_an_interactive_prompt_is_canceled() {
        let (_temp, packages, config, service) = packages_holding(&[]);
        let display = DisplayManager::new(false).answering(vec![crate::display_manager::ctrl_c()]);

        let exit = handle_create(&service, "fresh", &config, &display, true).await;

        assert_eq!(exit, Exit::Canceled);
        assert_eq!(std::fs::read_dir(&packages).unwrap().count(), 0);
    }

    // The name the interactive prompt gives is the one checked and written: a
    // name a standalone spec holds is refused there and asked for again, and
    // the file takes the second answer's name. Nothing asks for a file name.
    #[tokio::test]
    async fn the_interactively_named_spec_is_checked_and_written_under_that_name() {
        use crate::display_manager::answer;

        let (temp, packages, config, service) = packages_holding(&[]);
        let dotfiles = temp.path().join("dotfiles");
        std::fs::create_dir(&dotfiles).unwrap();
        std::fs::write(
            dotfiles.join("other.yml"),
            "name: other\ndotfiles:\n  - source: other\n    target: ~/.other\n",
        )
        .unwrap();
        let display = DisplayManager::new(false).answering(vec![
            answer("other".to_string()), // package name: a standalone spec has it
            answer("fresh".to_string()), // a different name
            answer(String::new()),       // homepage
            answer(String::new()),       // description
            answer(test_common::TEST_ENV.to_string()),
            answer("true".to_string()), // install
            answer(String::new()),      // check
            answer(false),              // another environment?
            answer(false),              // open the editor now?
        ]);

        let exit = handle_create(&service, "arg", &config, &display, true).await;

        assert_eq!(exit, Exit::Clean, "{:?}", display.printed());
        let written: Vec<String> = std::fs::read_dir(&packages)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(written, vec!["fresh.yml".to_string()]);
        let spec = std::fs::read_to_string(packages.join("fresh.yml")).unwrap();
        assert!(spec.contains("name: fresh"), "{spec}");
    }

    // A name the spec-name rule refuses is refused at its prompt and asked for
    // again, before anything else is asked.
    #[tokio::test]
    async fn an_interactive_name_the_rule_refuses_is_asked_again() {
        use crate::display_manager::answer;

        let (_temp, packages, config, service) = packages_holding(&[]);
        let display = DisplayManager::new(false).answering(vec![
            answer("my\u{200B}app".to_string()),
            answer("fresh".to_string()),
            answer(String::new()), // homepage
            answer(String::new()), // description
            answer(test_common::TEST_ENV.to_string()),
            answer("true".to_string()), // install
            answer(String::new()),      // check
            answer(false),              // another environment?
            answer(false),              // open the editor now?
        ]);

        let exit = handle_create(&service, "arg", &config, &display, true).await;

        assert_eq!(exit, Exit::Clean, "{:?}", display.printed());
        assert!(packages.join("fresh.yml").exists());
        assert!(
            display
                .printed()
                .iter()
                .any(|(_, line)| line.contains(r"'my\u{200b}app': it is not a valid spec name")),
            "{:?}",
            display.printed()
        );
    }

    // A configured environment the prompt can only show escaped is noted above
    // an empty prompt, and Enter keeps its raw bytes.
    #[test]
    fn enter_keeps_a_configured_environment_shown_escaped() {
        use crate::display_manager::answer;

        let config = CliConfig::wrap_for_test(test_common::test_config_with_dir_and_env(
            "/nonexistent",
            "dev\u{1b}",
        ));
        let display = DisplayManager::new(false).answering(vec![answer(String::new())]);

        let name = prompt_environment_name(&Environments::new(), &config, &display).unwrap();

        assert_eq!(name, "dev\u{1b}");
        assert_eq!(
            lines_containing(&display, r"Suggested: dev\u{1b} (Enter keeps it)"),
            1
        );
    }

    // A configured environment that is blank, such as U+00A0, is shown escaped
    // and refused on Enter rather than kept as a blank-looking key.
    #[test]
    fn a_blank_configured_environment_is_not_kept() {
        use crate::display_manager::answer;

        let config = CliConfig::wrap_for_test(test_common::test_config_with_dir_and_env(
            "/nonexistent",
            "\u{a0}",
        ));
        let display = DisplayManager::new(false)
            .answering(vec![answer(String::new()), answer("prod".to_string())]);

        let name = prompt_environment_name(&Environments::new(), &config, &display).unwrap();

        assert_eq!(name, "prod");
        assert_eq!(
            lines_containing(&display, "Environment name cannot be empty."),
            1
        );
    }

    // An answer holding nothing visible, such as a Hangul filler, is blank at
    // each required prompt: refused as empty and asked again.
    #[test]
    fn an_invisible_answer_is_blank_at_each_required_prompt() {
        use crate::display_manager::answer;

        let display = DisplayManager::new(false).answering(vec![
            answer("\u{3164}".to_string()),
            answer("fresh".to_string()),
        ]);
        assert_eq!(prompt_package_name("fresh", &display).unwrap(), "fresh");
        assert_eq!(
            lines_containing(&display, "Package name cannot be empty."),
            1
        );

        let config = CliConfig::wrap_for_test(test_common::test_config_with_dir("/nonexistent"));
        let display = DisplayManager::new(false).answering(vec![
            answer(" \u{ffa0}".to_string()),
            answer("prod".to_string()),
        ]);
        let name = prompt_environment_name(&Environments::new(), &config, &display).unwrap();
        assert_eq!(name, "prod");
        assert_eq!(
            lines_containing(&display, "Environment name cannot be empty."),
            1
        );

        let display = DisplayManager::new(false).answering(vec![
            answer("\u{115f}\u{200b}".to_string()),
            answer("true".to_string()),
        ]);
        assert_eq!(prompt_install_command(&display).unwrap(), "true");
        assert_eq!(
            lines_containing(
                &display,
                "Install command cannot be empty or only a comment."
            ),
            1
        );
    }

    // An install answer that runs nothing, empty, whitespace or only a comment,
    // is refused as install would refuse it, and asked again.
    #[test]
    fn a_comment_only_install_answer_is_asked_again() {
        use crate::display_manager::answer;

        let display = DisplayManager::new(false).answering(vec![
            answer("# TODO".to_string()),
            answer("   # later".to_string()),
            answer("true".to_string()),
        ]);

        assert_eq!(prompt_install_command(&display).unwrap(), "true");
        assert_eq!(
            lines_containing(
                &display,
                "Install command cannot be empty or only a comment."
            ),
            2
        );
    }

    // A check answer the install prompt would call blank is no check: a
    // comment-only one, as install and status read it, and one showing nothing,
    // which the library would run. A real one is kept as typed.
    #[test]
    fn a_blank_check_answer_is_no_check() {
        use crate::display_manager::answer;

        let display = DisplayManager::new(false).answering(vec![answer("# TODO".to_string())]);
        assert_eq!(prompt_check_command("x", &display).unwrap(), None);

        // An answer showing nothing is refused at the install prompt, so it is
        // no check here.
        let display =
            DisplayManager::new(false).answering(vec![answer("\u{115f}\u{200b}".to_string())]);
        assert_eq!(prompt_check_command("x", &display).unwrap(), None);

        let display = DisplayManager::new(false).answering(vec![answer("true".to_string())]);
        assert_eq!(
            prompt_check_command("x", &display).unwrap().as_deref(),
            Some("true")
        );
    }

    // A pre-filled environment name the user cleared is refused and asked for
    // again; no environment is keyed `""`.
    #[test]
    fn a_cleared_environment_name_is_asked_again() {
        use crate::display_manager::answer;

        let config = CliConfig::wrap_for_test(test_common::test_config_with_dir("/nonexistent"));
        let display = DisplayManager::new(false)
            .answering(vec![answer(String::new()), answer("prod".to_string())]);

        let name = prompt_environment_name(&Environments::new(), &config, &display).unwrap();

        assert_eq!(name, "prod");
        assert_eq!(
            lines_containing(&display, "Environment name cannot be empty."),
            1
        );
    }

    // An empty package name is refused as empty, not as a name the rule
    // refuses, at the first prompt and at the "different name" prompt alike.
    // The first prompt shows a name it cannot pre-fill in its note, each time
    // it asks.
    #[tokio::test]
    async fn an_empty_package_name_is_refused_as_empty() {
        use crate::display_manager::answer;

        let display = DisplayManager::new(false)
            .answering(vec![answer(String::new()), answer("fresh".to_string())]);

        let name = prompt_package_name("my\u{200b}app", &display).unwrap();

        assert_eq!(name, "fresh");
        assert_eq!(
            lines_containing(&display, "Package name cannot be empty."),
            1
        );
        assert_eq!(lines_containing(&display, "Refusing to create"), 0);
        assert_eq!(
            lines_containing(&display, r"Suggested: my\u{200b}app (shown escaped"),
            2
        );

        let (_temp, packages, config, service) = packages_holding(&["tool"]);
        let display = DisplayManager::new(false).answering(vec![
            answer(1_usize), // a different name
            answer(String::new()),
            answer("fresh".to_string()),
        ]);

        let exit = handle_create(&service, "tool", &config, &display, false).await;

        assert_eq!(exit, Exit::Clean, "{:?}", display.printed());
        assert!(packages.join("fresh.yml").exists());
        assert_eq!(
            lines_containing(&display, "Package name cannot be empty."),
            1
        );
        assert_eq!(lines_containing(&display, "Refusing to create"), 0);
    }

    // Answers every prompt of an interactive create, in order, up to and including
    // "Add another environment?": the name, homepage, description, environment,
    // install command and check command.
    fn answers_through_the_last_environment() -> Vec<crate::display_manager::Scripted> {
        use crate::display_manager::answer;
        vec![
            answer("fresh".to_string()),
            answer(String::new()), // homepage
            answer(String::new()), // description
            answer(test_common::TEST_ENV.to_string()),
            answer("true".to_string()), // install
            answer(String::new()),      // check
            answer(false),              // another environment?
        ]
    }

    fn lines_containing(display: &DisplayManager, needle: &str) -> usize {
        display
            .printed()
            .iter()
            .filter(|(_, line)| line.contains(needle))
            .count()
    }

    // With no terminal at the editor question the spec is already written, so
    // the run reports the create and does not refuse the question.
    #[tokio::test]
    async fn no_terminal_at_the_editor_question_reports_the_create() {
        let (_temp, packages, config, service) = packages_holding(&[]);
        let display = DisplayManager::new(false).answering(answers_through_the_last_environment());

        let exit = handle_create(&service, "arg", &config, &display, true).await;

        assert_eq!(exit, Exit::Clean, "{:?}", display.printed());
        assert!(packages.join("fresh.yml").exists());
        assert_eq!(
            display.printed().last().map(|(_, line)| line.as_str()),
            Some(
                "Package created. You can edit it later with `selfie spec edit`.\n  selfie spec edit fresh"
            )
        );
        assert_eq!(lines_containing(&display, "needs a terminal"), 0);
    }

    // Ctrl+C at the editor question ends the run canceled, with the spec
    // written.
    #[tokio::test]
    async fn ctrl_c_at_the_editor_question_is_canceled_with_the_spec_written() {
        let (_temp, packages, config, service) = packages_holding(&[]);
        let mut answers = answers_through_the_last_environment();
        answers.push(crate::display_manager::ctrl_c());
        let display = DisplayManager::new(false).answering(answers);

        let exit = handle_create(&service, "arg", &config, &display, true).await;

        assert_eq!(exit, Exit::Canceled);
        assert!(packages.join("fresh.yml").exists());
        assert_eq!(
            display.printed().last().map(|(_, line)| line.as_str()),
            Some(
                "Package created. You can edit it later with `selfie spec edit`.\n  selfie spec edit fresh"
            )
        );
    }

    // A terminal that fails at the editor question leaves the spec written, so
    // the run reports the create, and says the answer could not be read.
    #[tokio::test]
    async fn an_unreadable_answer_at_the_editor_question_reports_the_create() {
        let (_temp, packages, config, service) = packages_holding(&[]);
        let mut answers = answers_through_the_last_environment();
        answers.push(Err(std::io::Error::other("the terminal went away").into()));
        let display = DisplayManager::new(false).answering(answers);

        let exit = handle_create(&service, "arg", &config, &display, true).await;

        assert_eq!(exit, Exit::Clean, "{:?}", display.printed());
        assert!(packages.join("fresh.yml").exists());
        assert_eq!(
            lines_containing(
                &display,
                "Could not read the answer: the terminal went away"
            ),
            1,
            "{:?}",
            display.printed()
        );
        assert_eq!(
            display.printed().last().map(|(_, line)| line.as_str()),
            Some(
                "Package created. You can edit it later with `selfie spec edit`.\n  selfie spec edit fresh"
            )
        );
    }

    // A create that ends canceled before the spec is reported written exits
    // canceled, not as a failure.
    #[tokio::test]
    async fn a_canceled_create_exits_canceled() {
        use selfie::package::{
            event::{EventStream, OperationContext, OperationInfo, metadata::OperationType},
            service::MockSpecService,
        };

        let mut service = MockSpecService::new();
        service.expect_create().returning(|_| {
            Box::pin(async {
                let events = vec![PackageEvent::Canceled {
                    operation_info: OperationInfo {
                        id: uuid::Uuid::new_v4(),
                        operation_type: OperationType::PackageCreate,
                        package_name: "fresh".to_string(),
                        environment: "test".to_string(),
                        context: OperationContext::default(),
                        timestamp: std::time::Instant::now(),
                    },
                    reason: "Ctrl+C".to_string(),
                }];
                Box::pin(futures::stream::iter(events)) as EventStream
            })
        });
        let (_temp, _packages, config, _service) = packages_holding(&[]);
        let display = DisplayManager::new(false);

        let exit = handle_create(&service, "fresh", &config, &display, false).await;

        assert_eq!(exit, Exit::Canceled, "{:?}", display.printed());
    }

    // Cancel at the "already exists" menu writes nothing and exits as a
    // decline does.
    #[tokio::test]
    async fn cancel_at_the_taken_name_menu_exits_as_a_decline() {
        use crate::display_manager::{Channel, answer};

        let (_temp, packages, config, service) = packages_holding(&["tool"]);
        let display = DisplayManager::new(false).answering(vec![answer(2_usize)]);

        let exit = handle_create(&service, "tool", &config, &display, false).await;

        assert_eq!(exit, Exit::Failed);
        assert_eq!(std::fs::read_dir(&packages).unwrap().count(), 1);
        let stderr: Vec<String> = display
            .printed()
            .into_iter()
            .filter(|(channel, _)| *channel == Channel::Stderr)
            .map(|(_, line)| line)
            .collect();
        // Stderr holds the warning naming the taken name, then the note, and no
        // error.
        assert_eq!(
            stderr,
            [
                format!(
                    "'tool' is already a package ({}). Edit it, or choose a different name.",
                    packages.join("tool.yml").display()
                ),
                "Package creation canceled.".to_string(),
            ],
        );
    }

    // No terminal partway through the interactive prompts is a usage error,
    // and nothing is written.
    #[tokio::test]
    async fn no_terminal_partway_through_is_a_usage_error() {
        let (_temp, packages, config, service) = packages_holding(&[]);
        let mut answers = answers_through_the_last_environment();
        answers.truncate(3); // keeps name, homepage and description; then the script runs out
        let display = DisplayManager::new(false).answering(answers);

        let exit = handle_create(&service, "arg", &config, &display, true).await;

        assert_eq!(exit, Exit::Usage);
        assert_eq!(std::fs::read_dir(&packages).unwrap().count(), 0);
        assert_eq!(
            lines_containing(&display, "spec create --interactive needs a terminal"),
            1,
            "{:?}",
            display.printed()
        );
        assert_eq!(lines_containing(&display, "Leave off --interactive"), 1);
    }

    // Ctrl+C at the dependency picker, which is asked only when another spec
    // exists, ends the run canceled.
    #[tokio::test]
    async fn ctrl_c_at_the_dependency_picker_is_canceled() {
        let (_temp, packages, config, service) = packages_holding(&["other"]);
        let mut answers = answers_through_the_last_environment();
        answers.truncate(6); // keeps the answers up to the check command
        answers.push(crate::display_manager::ctrl_c());
        let display = DisplayManager::new(false).answering(answers);

        let exit = handle_create(&service, "arg", &config, &display, true).await;

        assert_eq!(exit, Exit::Canceled, "{:?}", display.printed());
        let written: Vec<String> = std::fs::read_dir(&packages)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(written, vec!["other.yml".to_string()]);
    }

    // No terminal at the prompt for a different name is a usage error, with the
    // way to choose one without a prompt.
    #[tokio::test]
    async fn no_terminal_at_the_replacement_name_is_a_usage_error() {
        use crate::display_manager::answer;

        let (_temp, packages, config, service) = packages_holding(&["tool"]);
        let display = DisplayManager::new(false).answering(vec![answer(1_usize)]);

        let exit = handle_create(&service, "tool", &config, &display, false).await;

        assert_eq!(exit, Exit::Usage);
        assert_eq!(std::fs::read_dir(&packages).unwrap().count(), 1);
        assert_eq!(
            lines_containing(
                &display,
                "Run 'selfie spec create' again with a different name."
            ),
            1,
            "{:?}",
            display.printed()
        );
    }

    // A name a standalone spec holds goes straight to the prompt for another;
    // a fourth taken name ends the run.
    #[tokio::test]
    async fn a_fourth_taken_name_is_too_many() {
        use crate::display_manager::answer;

        let (temp, packages, config, service) = packages_holding(&[]);
        let dotfiles = temp.path().join("dotfiles");
        std::fs::create_dir(&dotfiles).unwrap();
        std::fs::write(
            dotfiles.join("other.yml"),
            "name: other\ndotfiles:\n  - source: other\n    target: ~/.other\n",
        )
        .unwrap();
        let display = DisplayManager::new(false).answering(vec![
            answer("other".to_string()),
            answer("other".to_string()),
            answer("other".to_string()),
        ]);

        let exit = handle_create(&service, "other", &config, &display, false).await;

        assert_eq!(exit, Exit::Failed, "{:?}", display.printed());
        assert_eq!(std::fs::read_dir(&packages).unwrap().count(), 0);
        assert_eq!(lines_containing(&display, "Too many retry attempts"), 1);
    }

    // A repeated environment name is refused and asked again, so the first
    // environment keeps its commands and both are written.
    #[test]
    fn a_repeated_environment_name_is_asked_again() {
        use crate::display_manager::answer;

        let (_temp, _packages, config, _service) = packages_holding(&[]);
        let display = DisplayManager::new(false).answering(vec![
            answer("work".to_string()),
            answer("install-work".to_string()),
            answer(String::new()), // check
            answer(true),          // another environment?
            answer("work".to_string()),
            answer("home".to_string()),
            answer("install-home".to_string()),
            answer(String::new()), // check
            answer(false),         // another environment?
        ]);

        let environments = prompt_environments("tool", &config, &display).unwrap();

        let installs: Vec<(&str, Option<&str>)> = environments
            .iter()
            .map(|(name, env)| (name.as_str(), env.install()))
            .collect();
        assert_eq!(
            installs,
            vec![
                ("work", Some("install-work")),
                ("home", Some("install-home"))
            ]
        );
    }

    // The default offered for a further environment is never one the spec
    // already has.
    #[test]
    fn the_environment_default_skips_a_taken_name() {
        let config = CliConfig::wrap_for_test(test_config_with_dir_and_env("/p", "production"));
        let env = || EnvironmentConfig::new("true".to_string(), None, None, Vec::new(), Vec::new());
        let mut existing = Environments::new();

        assert_eq!(
            default_environment_name(&existing, &config),
            Some("production")
        );
        existing.insert("production".to_string(), env());
        assert_eq!(default_environment_name(&existing, &config), None);

        let config = CliConfig::wrap_for_test(test_config_with_dir_and_env("/p", "work"));
        let mut existing = Environments::new();
        existing.insert("work".to_string(), env());
        assert_eq!(
            default_environment_name(&existing, &config),
            Some("production")
        );
    }

    // Helper: collect the final `OperationResult` from an event stream.
    async fn collect_result(
        mut stream: selfie::package::event::EventStream,
    ) -> Option<OperationResult> {
        let mut result = None;
        while let Some(event) = stream.next().await {
            if let PackageEvent::Completed {
                result: op_result, ..
            } = event
            {
                result = Some(op_result);
            }
        }
        result
    }

    // ── Package template / structure tests (pure functions, no service needed) ──

    #[test]
    fn test_basic_package_template() {
        let package_dir = PathBuf::from("/test/packages");
        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));

        let package = create_basic_package("template-test", &config);

        assert_eq!(package.name(), "template-test");

        assert!(package.environments().contains_key("test-env"));

        let env = package.environments().get("test-env").unwrap();
        // The placeholders are comments, which name the package and run nothing,
        // so install and check refuse the template until it is edited.
        let yaml = selfie::yaml::serialize(&package).unwrap();
        assert!(
            yaml.contains("# TODO: Add install command for template-test"),
            "{yaml}"
        );
        assert!(
            yaml.contains("# TODO: Add check command for template-test"),
            "{yaml}"
        );
        assert_eq!(env.install(), None);
        assert_eq!(env.check(), None);
        assert!(env.dependencies().is_empty());
    }

    #[test]
    fn test_create_package_interactive_components() {
        let env_config = EnvironmentConfig::new(
            "brew install test".to_string(),
            Some("command -v test".to_string()),
            None,
            vec!["dependency1".to_string(), "dependency2".to_string()],
            Vec::new(),
        );

        assert_eq!(env_config.install(), Some("brew install test"));
        assert_eq!(env_config.check(), Some("command -v test"));
        assert_eq!(env_config.dependencies(), &["dependency1", "dependency2"]);
    }

    #[test]
    fn test_vs_code_wait_flag_logic() {
        let mut cmd = std::process::Command::new("code");
        cmd.arg("/tmp/test.yml");

        let editor = "code";
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
    fn test_package_template_structure() {
        let package_dir = PathBuf::from("/test/packages");
        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));

        let package = create_basic_package("structure-test", &config);

        assert_eq!(package.name(), "structure-test");

        assert!(package.description().is_none());
        assert!(package.homepage().is_none());

        let environments = package.environments();
        assert_eq!(environments.len(), 1);
        assert!(environments.contains_key("test-env"));

        let default_env = environments.get("test-env").unwrap();
        assert_eq!(default_env.install(), None);
        assert_eq!(default_env.check(), None);
        assert!(default_env.dependencies().is_empty());
    }

    #[test]
    fn test_create_basic_package_with_custom_environment() {
        let package_dir = PathBuf::from("/test/packages");
        let config =
            CliConfig::wrap_for_test(test_config_with_dir_and_env(&package_dir, "staging"));

        let package = create_basic_package("test-staging", &config);

        assert_eq!(package.name(), "test-staging");

        let environments = package.environments();
        assert!(environments.contains_key("staging"));
        assert!(!environments.contains_key("default"));

        let staging_env = &environments["staging"];
        let yaml = selfie::yaml::serialize(&package).unwrap();
        assert!(
            yaml.contains("# TODO: Add install command for test-staging"),
            "{yaml}"
        );
        assert!(staging_env.dependencies().is_empty());
    }

    #[test]
    fn test_handle_create_respects_environment_flag() {
        let package_dir = PathBuf::from("/test/packages");
        let config =
            CliConfig::wrap_for_test(test_config_with_dir_and_env(&package_dir, "production"));

        let package = create_basic_package("prod-test", &config);

        assert_eq!(package.name(), "prod-test");
        assert!(package.environments().contains_key("production"));
        assert!(!package.environments().contains_key("default"));
    }

    #[test]
    fn test_package_name_validation_logic() {
        let package_dir = PathBuf::from("/test/packages");
        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));

        let package = create_basic_package("new-unique-name", &config);

        assert_eq!(package.name(), "new-unique-name");
    }

    #[test]
    fn test_create_basic_package_structure() {
        let package_dir = PathBuf::from("/test/packages");
        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));

        let package = create_basic_package("structure-test", &config);

        assert_eq!(package.name(), "structure-test");

        let environments = package.environments();
        assert!(environments.contains_key("test-env"));
    }

    #[test]
    fn test_package_creation_respects_config_environment() {
        let package_dir = PathBuf::from("/test/packages");
        let config =
            CliConfig::wrap_for_test(test_config_with_dir_and_env(&package_dir, "production"));

        let package = create_basic_package("env-test", &config);

        let environments = package.environments();
        assert!(environments.contains_key("production"));
        assert!(!environments.contains_key("test-env"));
    }

    // Two package files claiming the name are not one that failed to load, so
    // the warning says only what the conflict says. One unloadable file keeps
    // the reason it cannot be edited.
    #[test]
    fn a_name_several_packages_claim_is_not_called_unloadable() {
        let warning_for = |files: &[(&str, &str)]| {
            let temp = tempfile::tempdir().unwrap();
            let packages = temp.path().join("packages");
            std::fs::create_dir_all(&packages).unwrap();
            for (file, body) in files {
                std::fs::write(packages.join(file), body).unwrap();
            }
            let config = CliConfig::wrap_for_test(test_config_with_dir(&packages));
            let repo = common::create_package_repository(&config);
            let display = DisplayManager::new(false);
            let _ = get_valid_package_name("twin", &repo, &config, &display);
            display
                .printed()
                .iter()
                .map(|(_, line)| line.clone())
                .find(|line| line.contains("twin"))
                .expect("the conflict is reported")
        };
        let spec = format!(
            "name: twin\nenvironments:\n  {}:\n    install: \"true\"\n",
            test_common::TEST_ENV
        );

        let several = warning_for(&[("twin.yml", &spec), ("Twin.yaml", &spec)]);
        assert!(!several.contains("could not be loaded"), "{several}");
        assert!(
            several.ends_with("all but one must be removed. Choose a different name."),
            "{several}"
        );

        let unloadable = warning_for(&[("twin.yml", "name: [unclosed\n")]);
        assert!(
            unloadable.contains("It could not be loaded, so it cannot be edited here."),
            "{unloadable}"
        );
    }

    // ── Service-layer tests (persistence goes through PackageService::create) ──

    #[tokio::test]
    async fn test_create_via_service_success() {
        let temp_dir = tempfile::tempdir().unwrap();
        let service = test_common::create_test_service(&temp_dir);
        let config = CliConfig::wrap_for_test(test_config_with_dir(temp_dir.path()));

        let package = create_basic_package("test-package", &config);

        let stream = service.create(package).await;
        let result = collect_result(stream).await.unwrap();

        match result {
            OperationResult::Success(OperationSuccess::PackageCreated {
                package_name,
                file_path,
                ..
            }) => {
                assert_eq!(package_name, "test-package");
                assert_eq!(file_path, temp_dir.path().join("test-package.yml"));
                assert!(file_path.exists(), "Package file should be created on disk");
            }
            other => panic!("Expected PackageCreated, got: {other:?}"),
        }
    }

    // The interactive prompts can give a file name other than the package name. The
    // run is named, like its result, by the file name: that is the name selfie
    // finds the spec by afterwards.
    #[tokio::test]
    async fn a_create_run_is_named_by_the_file_it_writes() {
        let temp_dir = tempfile::tempdir().unwrap();
        let service = test_common::create_test_service(&temp_dir);
        let config = CliConfig::wrap_for_test(test_config_with_dir(temp_dir.path()));
        let basic = create_basic_package("myapp", &config);
        let package = selfie::package::Package::new(
            basic.name().to_string(),
            None,
            None,
            Vec::new(),
            None,
            basic.environments().clone(),
            temp_dir.path().join("bar.yml"),
        );

        let mut stream = service.create(package).await;
        let mut completed = None;
        while let Some(event) = stream.next().await {
            if let PackageEvent::Completed {
                operation_info,
                result,
            } = event
            {
                completed = Some((operation_info.package_name, result));
            }
        }

        let Some((
            run_name,
            OperationResult::Success(OperationSuccess::PackageCreated { package_name, .. }),
        )) = completed
        else {
            panic!("expected the create to succeed, got: {completed:?}");
        };
        assert_eq!(run_name, "bar");
        assert_eq!(package_name, "bar");
    }

    #[tokio::test]
    async fn test_create_via_service_already_exists() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config = CliConfig::wrap_for_test(test_config_with_dir(temp_dir.path()));

        // Pre-create a package file so the service finds it
        let _ = test_common::create_service_test_package_file(&temp_dir, "existing-pkg", true);

        let service = test_common::create_test_service(&temp_dir);
        let package = create_basic_package("existing-pkg", &config);

        let stream = service.create(package).await;
        let result = collect_result(stream).await.unwrap();

        assert!(
            matches!(result, OperationResult::Failure(_)),
            "Expected failure for existing package"
        );
    }

    #[tokio::test]
    async fn test_create_via_service_with_custom_environment() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config =
            CliConfig::wrap_for_test(test_config_with_dir_and_env(temp_dir.path(), "production"));
        let service = test_common::create_test_service_for_env(&temp_dir, "production");

        let package = create_basic_package("prod-pkg", &config);

        assert!(package.environments().contains_key("production"));
        assert!(!package.environments().contains_key("default"));

        let stream = service.create(package).await;
        let result = collect_result(stream).await.unwrap();

        match result {
            OperationResult::Success(OperationSuccess::PackageCreated { environment, .. }) => {
                assert_eq!(environment, "production");
            }
            other => panic!("Expected PackageCreated, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_create_via_service_file_written_correctly() {
        let temp_dir = tempfile::tempdir().unwrap();
        let service = test_common::create_test_service(&temp_dir);
        let config = CliConfig::wrap_for_test(test_config_with_dir(temp_dir.path()));

        let package = create_basic_package("file-check", &config);

        let stream = service.create(package).await;
        let result = collect_result(stream).await.unwrap();

        assert!(matches!(result, OperationResult::Success(_)));

        // Verify the file was actually written with correct content
        let file_path = temp_dir.path().join("file-check.yml");
        let contents = std::fs::read_to_string(&file_path).unwrap();
        assert!(contents.contains("file-check"));
    }

    // ── Dependency-picker tests ──────────────────────────────────────────────

    fn dependency_repo(
        results: Vec<Result<selfie::package::Package, selfie::package::port::PackageParseError>>,
    ) -> selfie::package::port::MockPackageRepository {
        let mut repo = selfie::package::port::MockPackageRepository::new();
        repo.expect_list_packages().returning(move || {
            Ok(selfie::package::port::ListPackagesOutput::from_results(
                results.clone(),
            ))
        });
        repo
    }

    fn named(name: &str) -> selfie::package::Package {
        selfie::package::PackageBuilder::default()
            .name(name)
            .path(format!("/test/packages/{name}.yml"))
            .build()
    }

    #[test]
    fn available_dependency_names_are_sorted_and_report_nothing_skipped() {
        let repo = dependency_repo(vec![Ok(named("zsh")), Ok(named("alacritty"))]);

        let (names, skipped) = available_dependency_names(&repo).unwrap();

        assert_eq!(names, vec!["alacritty", "zsh"]);
        assert!(skipped.is_empty());
    }

    // The picker lists a spec selfie could read, and a file it could not read is
    // left out, so the caller is handed something to say about it.
    #[test]
    fn available_dependency_names_names_the_spec_it_could_not_read() {
        let repo = dependency_repo(vec![
            Ok(named("alacritty")),
            Err(selfie::package::port::PackageParseError::new(
                "/test/packages/ghost.yml",
                selfie::package::port::PackageParseKind::IrregularFile {
                    kind: "named pipe (fifo)",
                },
            )),
        ]);

        let (names, skipped) = available_dependency_names(&repo).unwrap();

        assert_eq!(names, vec!["alacritty"]);
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].contains("ghost.yml"), "got: {}", skipped[0]);
        assert!(
            skipped[0].contains("named pipe (fifo)"),
            "got: {}",
            skipped[0]
        );
    }

    // Creating the first package of all must still work, so the one listing
    // failure that means "nothing to depend on yet" is not an error.
    #[test]
    fn available_dependency_names_treats_a_missing_directory_as_no_candidates() {
        let mut repo = selfie::package::port::MockPackageRepository::new();
        repo.expect_list_packages().returning(|| {
            Err(PackageListError::new(
                PathBuf::from("/nowhere"),
                selfie::fs::DirectoryState::Absent(selfie::fs::AbsentReason::Empty),
            ))
        });

        let (names, skipped) = available_dependency_names(&repo).unwrap();

        assert!(names.is_empty());
        assert!(skipped.is_empty());
    }

    // A file at the package directory holds no candidates, but the spec about to be
    // created cannot be written under it, so it is not offered as an empty list.
    #[test]
    fn available_dependency_names_refuses_a_file_at_the_package_directory() {
        let mut repo = selfie::package::port::MockPackageRepository::new();
        repo.expect_list_packages().returning(|| {
            Err(PackageListError::new(
                PathBuf::from("/packages"),
                selfie::fs::DirectoryState::Absent(selfie::fs::AbsentReason::Occupied {
                    kind: "regular file",
                }),
            ))
        });

        let message = available_dependency_names(&repo).unwrap_err();

        assert!(
            message.contains("/packages is not a directory, it is a regular file"),
            "got: {message}"
        );
    }

    // Every other listing failure means selfie could not look, which must not be
    // offered to the user as an empty list of candidates.
    #[test]
    fn available_dependency_names_reports_a_listing_it_could_not_perform() {
        let mut repo = selfie::package::port::MockPackageRepository::new();
        repo.expect_list_packages().returning(|| {
            Err(PackageListError::new(
                PathBuf::from("/locked"),
                selfie::fs::DirectoryState::Unlistable(std::sync::Arc::new(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "denied",
                ))),
            ))
        });

        let message = available_dependency_names(&repo).unwrap_err();

        assert!(
            message.contains("Failed to list packages"),
            "got: {message}"
        );
    }

    // ── UI-concern tests (name validation uses repo directly for interactive flow) ──

    #[test]
    fn test_create_package_name_validation_with_mock_repo() {
        let mut mock_repo = MockPackageRepository::new();
        let package_dir = PathBuf::from("/test/packages");
        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));

        mock_repo
            .expect_get_package()
            .with(mockall::predicate::eq("new-package"))
            .times(1)
            .returning(|_| {
                Err(selfie::package::port::PackageError::PackageNotFound {
                    name: "new-package".to_string(),
                    packages_path: std::path::PathBuf::from("/test/packages"),
                    files_examined: 0,
                    search_patterns: vec!["new-package.yml".to_string()],
                    declared_by: Vec::new(),
                }
                .into())
            });

        let get_result = mock_repo.get_package("new-package");
        assert!(get_result.is_err());

        let package = create_basic_package("new-package", &config);
        assert_eq!(package.name(), "new-package");
    }
}
