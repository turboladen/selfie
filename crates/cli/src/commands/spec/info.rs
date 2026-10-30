use crate::tables::{Cell, SafeTable};
use crate::terminal_text::{Literal, Name, Prose, Shown, lit, shown};
use selfie::package::{
    event::{
        DependencyStatus, EnvironmentStatus, EnvironmentStatusData, OperationResult, PackageEvent,
        PackageInfoData,
    },
    service::SpecService,
};

use crate::{
    config::CliConfig,
    display_manager::{DisplayManager, shorten_path},
    event_processor::{EventProcessor, Exit},
    formatters::format_key,
    git_style, status_style,
};

use crate::commands::presentation;

pub(crate) async fn handle_info(
    service: &impl SpecService,
    package_name: &str,
    config: &CliConfig,
    display: &DisplayManager,
) -> Exit {
    tracing::debug!("Getting package definition for: {package_name}");

    // Status message:
    display.print_status(crate::terminal_text::Prose::message(format!(
        "Getting info for {package_name}..."
    )));

    // Call the service's spec_info method to get an event stream
    let event_stream = service.spec_info(package_name).await;

    // Process the event stream with custom handling for structured data
    let processor = EventProcessor::new(display.clone());
    let result = processor
        .process_events(event_stream, |event| {
            match event {
                PackageEvent::PackageInfoLoaded { package_info, .. } => {
                    let table = create_package_info_table(package_info, config);
                    display.println(shown!("{}", table));
                    print_dotfiles(package_info, display);
                    if let Some(reason) = &package_info.refusal_elsewhere {
                        display.print_info(crate::terminal_text::Prose::message(format!(
                            "selfie apply would refuse this spec in another environment: {reason}"
                        )));
                    }
                    true // Handled
                }
                PackageEvent::EnvironmentStatusChecked {
                    environment_status, ..
                } => {
                    let table = create_environment_table(environment_status, config);
                    display.println(shown!("\n{}", table));
                    true // Handled
                }
                PackageEvent::Completed {
                    result: OperationResult::Success(_),
                    ..
                } => true,
                PackageEvent::Completed { .. } => false,
                _ => false, // Use default handling for other events
            }
        })
        .await;
    result.exit
}

fn create_package_info_table(package_info: &PackageInfoData, config: &CliConfig) -> SafeTable {
    let mut table = SafeTable::new();
    let use_colors = config.use_colors();
    let key = |name: Literal| Cell::from(format_key(name, use_colors));

    table.add_row([key(lit!("Name")), Name::new(&package_info.name).into()]);
    if let Some(desc) = &package_info.description {
        table.add_row([
            key(lit!("Description")),
            Cell::from(crate::terminal_text::Prose::new(desc)),
        ]);
    }

    if let Some(homepage) = &package_info.homepage {
        table.add_row([
            key(lit!("Homepage")),
            Cell::painted(crate::terminal_text::Name::new(homepage), use_colors, |s| {
                s.underlined().blue()
            }),
        ]);
    }

    // A refused spec's environments cannot be trusted, so the reason stands in
    // for them rather than an empty list that reads as "declares none".
    if let Some(refusal) = &package_info.refusal {
        let text = format!("selfie apply would refuse this spec here: {refusal}");
        table.add_row([
            key(lit!("Refused")),
            status_style::styled_text(Prose::new(&text), use_colors, |s| s.red()).into(),
        ]);
    } else {
        let env_names = presentation::format_environment_names(
            &package_info.environments,
            &package_info.current_environment,
            config,
        );
        table.add_row([key(lit!("Environments")), env_names.into()]);
    }

    if let Some(git_status) = &package_info.git_status {
        table.add_row([
            key(lit!("Git Status")),
            git_style::format_git_status_long(git_status, use_colors).into(),
        ]);
    }

    table
}

/// The spec's dotfile entries and where each one's content comes from, then
/// how many commands apply would run for them here.
fn print_dotfiles(package_info: &PackageInfoData, display: &DisplayManager) {
    if package_info.dotfiles.is_empty() {
        return;
    }

    let mut table = SafeTable::new();
    table.set_header([lit!("Environment"), lit!("Content"), lit!("Target")]);
    for dotfile in &package_info.dotfiles {
        table.add_row([
            match (&dotfile.environment, dotfile.refused) {
                (Some(environment), true) => Name::new(&format!("{environment} (refused)")).into(),
                (Some(environment), false) => Name::new(environment).into(),
                (None, _) => Shown::from(lit!("(shared)")),
            }
            .into(),
            Cell::from(crate::commands::dotfiles::list::source_cell(&dotfile.entry)),
            shorten_path(dotfile.entry.target()).into(),
        ]);
    }
    display.println(shown!("\n{}", table));

    // A reader may take a package file for data, and applying this one runs
    // commands.
    if package_info.apply_commands > 0 {
        display.print_warning(crate::terminal_text::Prose::message(format!(
            "selfie apply runs {} {} in environment '{}' to produce this package's dotfiles",
            package_info.apply_commands,
            selfie::pluralize(package_info.apply_commands, "command", "commands"),
            package_info.current_environment,
        )));
    }
}

pub(crate) fn create_environment_table(
    env_status: &EnvironmentStatusData,
    config: &CliConfig,
) -> SafeTable {
    let mut env_table = SafeTable::new();

    // Create a header for the environment table
    let use_colors = config.use_colors();
    let name = &env_status.environment_name;
    let env_header = if env_status.is_current {
        Shown::from(Name::new(&format!("Environment: *{name}")))
            .paint(use_colors, |s| s.bold().green())
    } else {
        Shown::from(Name::new(&format!("Environment: {name}"))).paint(use_colors, |s| s.bold())
    };
    env_table.set_header([Cell::from(env_header), Name::new("").into()]);

    let format_env_key = |key: Literal| Cell::from(format_key(key, use_colors));
    // Commands, as prose: a block-scalar command keeps its line breaks inside
    // the cell.
    let format_env_value = |text: &str| Cell::from(Prose::new(text));

    // Add installation status if this is the current environment and we have status
    if env_status.is_current {
        let status_text = format_status(&env_status.status, config.use_colors());
        env_table.add_row([format_env_key(lit!("Status")), status_text.into()]);
    }

    // Add environment detail rows
    // A blank install still gets its row, so the user sees why install refuses.
    let install = env_status.install_command.as_deref().unwrap_or(
        "none (blank: `package install` refuses it unless the check says it is installed)",
    );
    env_table.add_row([format_env_key(lit!("Install")), format_env_value(install)]);

    if let Some(check) = &env_status.check_command {
        env_table.add_row([format_env_key(lit!("Check")), format_env_value(check)]);
    }

    if !env_status.dependency_statuses.is_empty() {
        let dep_display = env_status
            .dependency_statuses
            .iter()
            .map(|dep| format_dependency_with_status(dep, use_colors));
        env_table.add_row([
            format_env_key(lit!("Dependencies")),
            Shown::join(dep_display, lit!(", ")).into(),
        ]);
    } else if !env_status.dependencies.is_empty() {
        env_table.add_row([
            format_env_key(lit!("Dependencies")),
            crate::formatters::name_list(&env_status.dependencies).into(),
        ]);
    }

    if !env_status.recommend_statuses.is_empty() {
        let rec_display = env_status
            .recommend_statuses
            .iter()
            .map(|dep| format_dependency_with_status(dep, use_colors));
        env_table.add_row([
            format_env_key(lit!("Recommends")),
            Shown::join(rec_display, lit!(", ")).into(),
        ]);
    } else if !env_status.recommends.is_empty() {
        env_table.add_row([
            format_env_key(lit!("Recommends")),
            crate::formatters::name_list(&env_status.recommends).into(),
        ]);
    }

    env_table
}

pub(crate) fn format_status(status: &EnvironmentStatus, use_colors: bool) -> Shown {
    match status {
        EnvironmentStatus::Installed => status_style::format_installed(use_colors),
        EnvironmentStatus::NotInstalled => status_style::format_not_installed(use_colors),
        EnvironmentStatus::Unknown(reason) => status_style::styled_text(
            Prose::new(&format!("Unknown ({reason})")),
            use_colors,
            |s| s.yellow().italic(),
        ),
    }
}

fn format_dependency_with_status(dep: &DependencyStatus, use_colors: bool) -> Shown {
    shown!(
        "{}: {}",
        Name::new(&dep.name),
        format_status(&dep.status, use_colors)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use selfie::package::event::EnvironmentStatus;
    use test_common::TEST_ENV;

    // Dependencies and recommends whose statuses were not checked are listed
    // by name, so an invisible character in one shows as an escape.
    #[test]
    fn unchecked_dependencies_are_listed_as_names() {
        let config = CliConfig::wrap_for_test(test_common::test_config());
        let env_status = EnvironmentStatusData {
            environment_name: TEST_ENV.to_string(),
            is_current: false,
            install_command: None,
            check_command: None,
            dependencies: vec!["z\u{200B}w".to_string(), "git".to_string()],
            dependency_statuses: Vec::new(),
            recommends: vec!["r\u{200B}s".to_string()],
            recommend_statuses: Vec::new(),
            status: EnvironmentStatus::NotInstalled,
        };

        let table = create_environment_table(&env_status, &config).to_string();

        assert!(table.contains(r"z\u{200b}w, git"), "{table}");
        assert!(table.contains(r"r\u{200b}s"), "{table}");
        assert!(!table.contains('\u{200B}'), "{table:?}");
    }

    #[test]
    fn test_format_status_functions() {
        let use_colors = false;

        let status = EnvironmentStatus::Installed;
        let result = format_status(&status, use_colors);
        assert!(result.contains("Installed"));

        let status = EnvironmentStatus::NotInstalled;
        let result = format_status(&status, use_colors);
        assert!(result.contains("Not installed"));
    }

    #[test]
    fn test_format_environment_names() {
        let config = CliConfig::wrap_for_test(test_common::test_config());
        let environments = vec![TEST_ENV.to_string(), "alt-test-env".to_string()];
        let result = presentation::format_environment_names(&environments, TEST_ENV, &config);
        assert!(!result.is_empty());
    }

    #[test]
    fn test_format_dependency_with_status_installed() {
        let dep = DependencyStatus {
            name: "git".to_string(),
            status: EnvironmentStatus::Installed,
        };
        let result = format_dependency_with_status(&dep, false);
        assert!(result.starts_with("git: "));
        assert!(result.contains("Installed"));
    }

    #[test]
    fn test_format_dependency_with_status_not_installed() {
        let dep = DependencyStatus {
            name: "curl".to_string(),
            status: EnvironmentStatus::NotInstalled,
        };
        let result = format_dependency_with_status(&dep, false);
        assert!(result.starts_with("curl: "));
        assert!(result.contains("Not installed"));
    }

    // A dependency no spec answers to, and one whose spec answered and could
    // not be loaded, each print the error that says which.
    #[test]
    fn test_format_dependency_with_status_unknown() {
        use selfie::package::event::UnknownStatus;
        use selfie::package::port::{PackageError, PackageRepoError};

        let missing = DependencyStatus {
            name: "missing".to_string(),
            status: EnvironmentStatus::Unknown(UnknownStatus::LookupFailed(
                PackageRepoError::from(PackageError::PackageNotFound {
                    name: "missing".to_string(),
                    packages_path: "/packages".into(),
                    files_examined: 0,
                    search_patterns: Vec::new(),
                    declared_by: Vec::new(),
                })
                .into_lookup_failure()
                .expect("a lookup returns this error"),
            )),
        };
        let result = format_dependency_with_status(&missing, false);
        assert!(result.starts_with("missing: "), "{result}");
        assert!(
            result.contains("Unknown (Package `missing` not found in path /packages)"),
            "{result}"
        );

        let source = selfie::yaml::parse::<selfie::package::Package>("name: broken\nx: {oops\n")
            .expect_err("the fixture must not parse");
        let broken = DependencyStatus {
            name: "broken".to_string(),
            status: EnvironmentStatus::Unknown(UnknownStatus::LookupFailed(
                PackageRepoError::from(PackageError::ParseError {
                    name: "broken".to_string(),
                    packages_path: "/packages".into(),
                    failed_file: "/packages/broken.yml".into(),
                    cause: selfie::package::port::PackageParseError::new(
                        "/packages/broken.yml",
                        selfie::package::port::PackageParseKind::Yaml { cause: source },
                    ),
                })
                .into_lookup_failure()
                .expect("a lookup returns this error"),
            )),
        };
        let result = format_dependency_with_status(&broken, false);
        assert!(result.starts_with("broken: Unknown ("), "{result}");
        assert!(result.contains("/packages/broken.yml"), "{result}");
    }
}
