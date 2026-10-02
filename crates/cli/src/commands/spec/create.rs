use dialoguer::{Confirm, Input, MultiSelect, Select, theme::SimpleTheme};
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

use crate::formatters::name_argument;

use crate::{
    config::CliConfig,
    display_manager::{DisplayManager, PromptFailure},
    event_processor::{EventProcessor, Exit},
};

use crate::commands::common;

const MAX_NAME_RETRIES: usize = 3;

enum PackageNameResult {
    CreateNew(String),     // Use this name to create a new package
    EditExisting(PathBuf), // User wants to edit the existing package at this path
    Cancelled,             // User cancelled the operation
}

pub(crate) async fn handle_create(
    service: &impl SpecService,
    package_name: &str,
    config: &CliConfig,
    display: &DisplayManager,
    interactive: bool,
) -> i32 {
    info!("Creating package: {}", package_name);

    // Refused before the name check, so a run that cannot ask prints only why.
    if interactive && !display.can_prompt() {
        return refuse_interactive(display, &PromptFailure::NoTerminal);
    }

    // Create repository for name validation (UI flow decisions)
    let repo = common::create_package_repository(config);

    // Get a valid package name or handle existing package scenarios
    let package_name = match get_valid_package_name(package_name, &repo, config, display) {
        Ok(PackageNameResult::CreateNew(name)) => name,
        Ok(PackageNameResult::EditExisting(path)) => {
            display.print_run_note(format!(
                "Opening existing package for editing at {}",
                path.display()
            ));
            let success_message = format!("Package editing completed at {}", path.display());
            return common::open_editor(&path, display, Some(success_message));
        }
        // A create that wrote nothing did not do what it was asked, so a script
        // must not read it as success.
        Ok(PackageNameResult::Cancelled) => {
            display.print_info("Package creation cancelled.");
            return Exit::Failed.code();
        }
        Err(exit_code) => return exit_code,
    };

    // Build the Package (CLI handles interactive prompting)
    let package = if interactive {
        match create_package_interactive(&package_name, config, display) {
            Ok(pkg) => pkg,
            Err(exit_code) => return exit_code,
        }
    } else {
        create_basic_package(&package_name, config)
    };

    // Use PackageService::create to persist (hexagonal pattern)
    let event_stream = service.create(package).await;

    // Process the event stream with custom handling for create-specific events
    // The name and path the library created the spec under. The name is the file's,
    // which the interactive prompts may have made different from the argument.
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

    if result.exit_code != 0 {
        return result.exit_code;
    }

    // Ask if user wants to edit the file (only in interactive mode)
    if interactive {
        if let Some((ref created_name, ref file_path)) = created {
            let edit_now = display.prompt(
                Confirm::with_theme(&SimpleTheme)
                    .with_prompt("Would you like to open the package file for editing now?")
                    .default(true),
            );

            match edit_now {
                Ok(true) => {
                    let success_message = format!(
                        "Package '{}' created and saved at {}",
                        created_name,
                        file_path.display()
                    );
                    common::open_editor(file_path, display, Some(success_message))
                }
                Ok(false) => {
                    display.print_info(
                        "Package created. You can edit it later with 'selfie spec edit'.",
                    );
                    Exit::Clean.code()
                }
                Err(failure) => display
                    .refuse_prompt(
                        &failure,
                        "Asking whether to open the editor",
                        format!("Open {} with 'selfie spec edit'.", file_path.display()).as_str(),
                    )
                    .code(),
            }
        } else {
            Exit::Clean.code()
        }
    } else {
        display.print_info("Package created. Use 'selfie spec edit' to customize it.");
        Exit::Clean.code()
    }
}

/// Report a prompt of `spec create --interactive` that got no answer, and return
/// the exit code.
fn refuse_interactive(display: &DisplayManager, failure: &PromptFailure) -> i32 {
    display
        .refuse_prompt(
            failure,
            "spec create --interactive",
            "Leave off --interactive to write a template, then edit it with 'selfie spec edit'.",
        )
        .code()
}

fn get_valid_package_name(
    initial_name: &str,
    repo: &impl PackageRepository,
    config: &CliConfig,
    display: &DisplayManager,
) -> Result<PackageNameResult, i32> {
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
                display.print_error(format!("Cannot create '{current_name}': {error}"));
                return Err(Exit::Failed.code());
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
                    display
                        .print_warning(format!("{conflict} Edit it, or choose a different name."));

                    let action = display.prompt(
                        Select::with_theme(&SimpleTheme)
                            .with_prompt("What would you like to do?")
                            .items([
                                "Edit the existing package",
                                "Create a new package with a different name",
                                "Cancel",
                            ])
                            .default(0),
                    );

                    match action {
                        Ok(0) => {
                            return Ok(PackageNameResult::EditExisting(
                                existing_package.file_path().to_path_buf(),
                            ));
                        }
                        Ok(1) => {
                            // Fall through to prompt for new name below
                        }
                        Ok(_) => return Ok(PackageNameResult::Cancelled),
                        Err(failure) => {
                            return Err(display
                                .refuse_prompt(
                                    &failure,
                                    "Choosing what to do about an existing package",
                                    format!(
                                        "Edit it with `selfie spec edit {}`, or run `selfie spec \
                                         create` with a different name.",
                                        name_argument(&current_name)
                                    )
                                    .as_str(),
                                )
                                .code());
                        }
                    }
                } else {
                    // Not a package this command can edit, so a different name is
                    // the way out.
                    let why = match conflict.found_in {
                        namespace::NameLocation::Packages => {
                            " It could not be loaded, so it cannot be edited here."
                        }
                        namespace::NameLocation::Dotfiles => "",
                    };
                    display.print_warning(format!("{conflict}{why} Choose a different name."));
                }

                // Prompt for a new name
                retry_count += 1;
                if retry_count > MAX_NAME_RETRIES {
                    display.print_error(format!(
                        "Too many retry attempts ({MAX_NAME_RETRIES}). Please try again later."
                    ));
                    return Err(Exit::Failed.code());
                }

                let new_name: String =
                    match display.prompt(Input::with_theme(&SimpleTheme).with_prompt(format!(
                        "Enter a new package name (attempt {retry_count}/{MAX_NAME_RETRIES})"
                    ))) {
                        Ok(name) => name,
                        Err(failure) => {
                            return Err(display
                                .refuse_prompt(
                                    &failure,
                                    "Choosing a different name",
                                    "Run 'selfie spec create' again with a different name.",
                                )
                                .code());
                        }
                    };
                current_name = new_name;
                continue;
            }
            Ok(()) => {
                // Name is unique — proceed with creation
                return Ok(PackageNameResult::CreateNew(current_name));
            }
        }
    }
}

fn create_basic_package(package_name: &str, config: &CliConfig) -> selfie::package::Package {
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
) -> Result<selfie::package::Package, i32> {
    display.print_info("Creating package interactively...");

    let name = prompt_package_name(package_name, display)?;
    let homepage = prompt_package_homepage(display)?;
    let description = prompt_package_description(display)?;
    let environments = prompt_environments(&name, config, display)?;
    let file_name = prompt_file_name(&name, display)?;

    Ok(selfie::package::Package::new(
        name,
        homepage,
        description,
        Vec::new(),
        None,
        environments,
        config.package_directory().join(format!("{file_name}.yml")),
    ))
}

fn prompt_package_name(default_name: &str, display: &DisplayManager) -> Result<String, i32> {
    display
        .prompt(
            Input::with_theme(&SimpleTheme)
                .with_prompt("Package name")
                .default(default_name.to_string()),
        )
        .map_err(|failure| refuse_interactive(display, &failure))
}

fn prompt_package_homepage(display: &DisplayManager) -> Result<Option<String>, i32> {
    let homepage: String = display
        .prompt(
            Input::with_theme(&SimpleTheme)
                .with_prompt("Homepage URL (optional)")
                .allow_empty(true),
        )
        .map_err(|failure| refuse_interactive(display, &failure))?;

    Ok(if homepage.trim().is_empty() {
        None
    } else {
        Some(homepage)
    })
}

fn prompt_package_description(display: &DisplayManager) -> Result<Option<String>, i32> {
    let description: String = display
        .prompt(
            Input::with_theme(&SimpleTheme)
                .with_prompt("Description (optional)")
                .allow_empty(true),
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
) -> Result<Environments, i32> {
    let mut environments = Environments::new();

    loop {
        display.print_info("Adding environment configuration...");

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

fn prompt_environment_name(
    existing_environments: &Environments,
    config: &CliConfig,
    display: &DisplayManager,
) -> Result<String, i32> {
    let default_env = if existing_environments.is_empty() {
        config.environment().to_string()
    } else {
        "production".to_string()
    };

    display
        .prompt(
            Input::with_theme(&SimpleTheme)
                .with_prompt("Environment name")
                .default(default_env),
        )
        .map_err(|failure| refuse_interactive(display, &failure))
}

fn prompt_install_command(display: &DisplayManager) -> Result<String, i32> {
    loop {
        let cmd: String = display
            .prompt(Input::with_theme(&SimpleTheme).with_prompt("Install command (required)"))
            .map_err(|failure| refuse_interactive(display, &failure))?;

        if !cmd.trim().is_empty() {
            break Ok(cmd);
        }

        display.print_error("Install command cannot be empty.");
    }
}

fn prompt_check_command(
    package_name: &str,
    display: &DisplayManager,
) -> Result<Option<String>, i32> {
    let default_check = format!("command -v {package_name}");
    let check_cmd: String = display
        .prompt(
            Input::with_theme(&SimpleTheme)
                .with_prompt("Check command (optional)")
                .default(default_check)
                .allow_empty(true),
        )
        .map_err(|failure| refuse_interactive(display, &failure))?;

    Ok(if check_cmd.trim().is_empty() {
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
) -> Result<(Vec<String>, Vec<String>), String> {
    match common::package_names_and_skipped(repo) {
        Ok(loaded) => Ok(loaded),
        // The first package anyone writes has no directory to list yet, and
        // having nothing to depend on is the right answer for it. Any other
        // failure is selfie unable to look, or a path the spec cannot be written
        // under, and neither may be offered as "there is nothing there".
        Err(listing) if listing.may_be_created() => Ok((Vec::new(), Vec::new())),
        Err(e) => Err(format!("Failed to list packages: {e}")),
    }
}

fn prompt_dependencies(config: &CliConfig, display: &DisplayManager) -> Result<Vec<String>, i32> {
    let repo = common::create_package_repository(config);

    let (available_packages, skipped) = available_dependency_names(&repo).map_err(|msg| {
        display.print_error(msg);
        Exit::Failed.code()
    })?;

    // A dependency is resolved by name, and selfie does not know the name inside
    // a spec it could not read, so these cannot be offered. Naming them stops
    // the picker reading as every package the user has.
    for warning in skipped {
        display.print_warning(warning);
    }

    if available_packages.is_empty() {
        return Ok(Vec::new());
    }

    let selected = display
        .prompt(
            MultiSelect::with_theme(&SimpleTheme)
                .with_prompt("Dependencies (select with space, confirm with enter)")
                .items(&available_packages),
        )
        .map_err(|failure| refuse_interactive(display, &failure))?;

    Ok(selected
        .into_iter()
        .map(|i| available_packages[i].clone())
        .collect())
}

fn prompt_add_another_environment(display: &DisplayManager) -> Result<bool, i32> {
    display
        .prompt(
            Confirm::with_theme(&SimpleTheme)
                .with_prompt("Add another environment?")
                .default(false),
        )
        .map_err(|failure| refuse_interactive(display, &failure))
}

fn prompt_file_name(default_name: &str, display: &DisplayManager) -> Result<String, i32> {
    display
        .prompt(
            Input::with_theme(&SimpleTheme)
                .with_prompt("File name (without .yml extension)")
                .default(default_name.to_string()),
        )
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

    // Ctrl+C at the "already exists" menu ends the run canceled.
    #[tokio::test]
    async fn ctrl_c_at_the_taken_name_menu_is_canceled() {
        let (_temp, packages, config, service) = packages_holding(&["tool"]);
        let display = DisplayManager::new(false).answering(vec![crate::display_manager::ctrl_c()]);

        let code = handle_create(&service, "tool", &config, &display, false).await;

        assert_eq!(code, 130);
        assert_eq!(std::fs::read_dir(&packages).unwrap().count(), 1);
    }

    // Ctrl+C at the first interactive prompt ends the run canceled, writing
    // nothing.
    #[tokio::test]
    async fn ctrl_c_at_an_interactive_prompt_is_canceled() {
        let (_temp, packages, config, service) = packages_holding(&[]);
        let display = DisplayManager::new(false).answering(vec![crate::display_manager::ctrl_c()]);

        let code = handle_create(&service, "fresh", &config, &display, true).await;

        assert_eq!(code, 130);
        assert_eq!(std::fs::read_dir(&packages).unwrap().count(), 0);
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
        assert!(env.install().contains("template-test"));
        assert!(env.check().unwrap().contains("template-test"));
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

        assert_eq!(env_config.install(), "brew install test");
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
        assert!(default_env.install().starts_with("# TODO:"));
        assert!(default_env.check().is_some());
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
        assert!(staging_env.install().contains("test-staging"));
        assert!(staging_env.check().unwrap().contains("test-staging"));
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
            .build()
    }

    #[test]
    fn available_dependency_names_are_sorted_and_report_nothing_skipped() {
        let repo = dependency_repo(vec![Ok(named("zsh")), Ok(named("alacritty"))]);

        let (names, skipped) = available_dependency_names(&repo).unwrap();

        assert_eq!(names, vec!["alacritty", "zsh"]);
        assert!(skipped.is_empty());
    }

    // The picker resolves a dependency by the name inside the spec, which selfie
    // does not have for a file it could not read, so the caller is handed
    // something to say about it.
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
                }
                .into())
            });

        let get_result = mock_repo.get_package("new-package");
        assert!(get_result.is_err());

        let package = create_basic_package("new-package", &config);
        assert_eq!(package.name(), "new-package");
    }
}
