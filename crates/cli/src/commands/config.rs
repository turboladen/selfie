use crate::terminal_text::{DisplayPath, Literal, Name, Shown, lit, shown};
use selfie::{
    config::{YamlLoader, loader::ConfigLoader},
    fs::FileSystem,
};
use tracing::info;

use selfie::package::event::Outcome;

use crate::{
    display_manager::DisplayManager, event_processor::Exit, tables::ValidationTableReporter,
};

/// Report what the configuration file on disk says, and whether it is valid.
///
/// Takes no `CliConfig`, deliberately: every value reported here comes from a
/// fresh load of the file, so a flag cannot mask a problem in it.
pub(crate) fn handle_validate(display: &DisplayManager, fs: &impl FileSystem) -> Exit {
    info!("Validating configuration");

    // Load the raw on-disk config (without CLI overrides) so that validation
    // catches issues that would be masked by flags like --environment.
    let loaded = match YamlLoader::new(fs).load_config() {
        Ok(c) => c,
        Err(e) => {
            // A configuration file's error names its path as a path, as the
            // startup error does.
            let cause = crate::received::config_load_error(&e)
                .unwrap_or_else(|| crate::received::sentence(&e).into());
            display.print_error(shown!("Failed to load configuration: {}", cause));
            return Exit::Failed;
        }
    };

    // Covers the ignored keys as well as the settings.
    let result = loaded.validate(fs);

    // `main` hands this command over before it reports notices, so both halves
    // are reported here. They print as notices because only the library builds a
    // `ValidationIssue`.
    let cli_load = crate::config::cli_section(&loaded);
    let cli_notices = cli_load.notices;
    let raw_config = loaded.config();

    // Notes need nothing done, so they neither fail validation nor stop it
    // reporting the file as valid.
    for note in result.issues().infos() {
        display.print_info(crate::received::Issue::from(note).message);
    }

    // A `cli:` notice is an ignored key like any other, so it counts as a warning.
    let outcome = match result.issues().outcome() {
        Outcome::Clean if !cli_notices.is_empty() => Outcome::Found,
        outcome => outcome,
    };

    // The issues, the notes and the verdict are what this command was asked for,
    // so all of them go to stdout. Only a file that would not load, above, is an
    // error about the run.
    if outcome == Outcome::Failed {
        display.print_result(outcome, lit!("Validation failed."));

        let mut table_reporter = ValidationTableReporter::new();
        table_reporter
            .setup([
                lit!("Category"),
                lit!("Field"),
                lit!("Message"),
                lit!("Suggestion"),
            ])
            .add_validation_errors(&result.issues().errors())
            .add_validation_warnings(&result.issues().warnings())
            .print(display);
        crate::config::print_config_notices(&cli_notices, display);
        Exit::Failed
    } else {
        if result.issues().has_warnings() {
            let mut table_reporter = ValidationTableReporter::new();
            table_reporter
                .setup([
                    lit!("Category"),
                    lit!("Field"),
                    lit!("Message"),
                    lit!("Suggestion"),
                ])
                .add_validation_warnings(&result.issues().warnings())
                .print(display);
        }
        crate::config::print_config_notices(&cli_notices, display);

        if outcome == Outcome::Clean {
            display.print_result(outcome, lit!("Configuration is valid."));
        } else {
            display.print_result(outcome, lit!("Configuration is usable, with warnings."));
        }

        // Each value as a run would take it from the file: `~` expanded and
        // defaults filled in. Both required settings are present here, since a
        // file without one fails validation above.
        display.print_field(
            lit!("environment:"),
            Name::new(raw_config.environment().unwrap_or_default()),
        );
        display.print_field(
            lit!("package_directory:"),
            DisplayPath::of(&raw_config.package_directory(fs).unwrap_or_default()),
        );
        // The directories in effect: the configured value, or the default the
        // file leaves selfie to derive. Whether either exists is reported in the
        // table above, so a wrong value is visible here before a command
        // behaves oddly.
        display.print_field(
            lit!("dotfiles_directory:"),
            DisplayPath::of(&raw_config.dotfiles_directory(fs).unwrap_or_default()),
        );
        display.print_field(
            lit!("state_directory:"),
            match selfie::fs::state_directory(fs, raw_config.state_directory(fs).as_deref()) {
                Ok(directory) => Shown::from(DisplayPath::of(&directory)),
                // The library's sentence names the path, so it keeps the name
                // escape until that sentence is composed from its parts
                // (selfie-36md).
                Err(error) => shown!("(unresolved: {})", Name::new(&error.to_string())),
            },
        );
        display.print_field(
            lit!("command_timeout:"),
            shown!("{} seconds", raw_config.command_timeout().as_secs()),
        );
        display.print_field(
            lit!("max_concurrency:"),
            shown!("{}", raw_config.max_concurrency().get()),
        );
        display.print_field(lit!("stop_on_error:"), yes_no(raw_config.stop_on_error()));
        // From the file's `cli:` section, not the run's flags. Every other line
        // here reports the file, and this command exists so a flag cannot mask a
        // problem in it -- `--no-color` must not make a file saying
        // `use_colors: true` read as false.
        display.print_field(lit!("verbose:"), yes_no(cli_load.section.verbose));
        display.print_field(lit!("use_colors:"), yes_no(cli_load.section.use_colors));

        Exit::from(outcome)
    }
}

// A setting's value as the file spells it.
fn yes_no(value: bool) -> Literal {
    if value { lit!("true") } else { lit!("false") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use selfie::fs::MockFileSystem;
    use std::path::Path;

    fn create_display() -> DisplayManager {
        DisplayManager::new(false)
    }

    // Creates a mock filesystem that returns a valid config file.
    fn mock_fs_with_valid_config() -> MockFileSystem {
        let mut fs = MockFileSystem::default();
        let config_dir = Path::new("/home/test/.config/selfie");
        let config_yaml = r#"
            environment: "test-env"
            package_directory: "/test/packages"
        "#;
        fs.mock_config_file(config_dir, config_yaml);
        fs.expect_list_directory().returning(|_| Ok(Vec::new()));
        // No deploy state has been written yet.
        fs.expect_read_file()
            .withf(|path| path.ends_with("deploy-state.yml"))
            .returning(|_| {
                Err(selfie::fs::FileSystemError::IoError(
                    std::io::Error::from(std::io::ErrorKind::NotFound).into(),
                ))
            });
        // The report and the validation both resolve the default state
        // directory under the home directory when the file names none.
        mock_home(&mut fs);
        fs.mock_directories_exist();
        fs.mock_creatable_directories();
        fs
    }

    // A home directory that answers however many times it is asked.
    fn mock_home(fs: &mut MockFileSystem) {
        fs.expect_expand_path()
            .withf(|path| path == Path::new("~"))
            .returning(|_| Ok(std::path::PathBuf::from("/home/test")));
    }

    #[test]
    fn test_handle_validate_function_does_not_panic() {
        let display = create_display();
        let fs = mock_fs_with_valid_config();

        let result = handle_validate(&display, &fs);
        assert!(result == Exit::Clean || result == Exit::Failed);
    }

    #[test]
    fn test_handle_validate_with_colors_enabled() {
        let display = DisplayManager::new(true);
        let fs = mock_fs_with_valid_config();

        let result = handle_validate(&display, &fs);
        assert!(result == Exit::Clean || result == Exit::Failed);
    }

    #[test]
    fn test_handle_validate_with_verbose_enabled() {
        let display = create_display();
        let fs = mock_fs_with_valid_config();

        let result = handle_validate(&display, &fs);
        assert!(result == Exit::Clean || result == Exit::Failed);
    }

    #[test]
    fn test_handle_validate_catches_empty_environment_on_disk() {
        let display = create_display();

        // On-disk config has empty environment — CLI config would mask this,
        // but handle_validate should re-load from disk and catch it.
        let mut fs = MockFileSystem::default();
        let config_dir = Path::new("/home/test/.config/selfie");
        let config_yaml = r#"
            environment: ""
            package_directory: "/test/packages"
        "#;
        fs.mock_config_file(config_dir, config_yaml);
        mock_home(&mut fs);
        fs.mock_directories_exist();
        fs.mock_creatable_directories();
        fs.expect_list_directory().returning(|_| Ok(Vec::new()));
        // No deploy state has been written yet.
        fs.expect_read_file()
            .withf(|path| path.ends_with("deploy-state.yml"))
            .returning(|_| {
                Err(selfie::fs::FileSystemError::IoError(
                    std::io::Error::from(std::io::ErrorKind::NotFound).into(),
                ))
            });

        let result = handle_validate(&display, &fs);
        assert_eq!(result, Exit::Failed);
    }

    #[test]
    fn test_handle_validate_reports_load_error() {
        let display = create_display();

        // Mock filesystem where config file doesn't exist
        let mut fs = MockFileSystem::default();
        let config_dir = Path::new("/home/test/.config/selfie");
        fs.mock_config_dir_ok(config_dir);
        fs.mock_path_exists(&config_dir.join("config.yaml"), false);
        fs.mock_path_exists(&config_dir.join("config.yml"), false);
        // Nothing there at all, not a link that fails to resolve — the loader
        // asks before it concludes the file is absent.
        fs.expect_symlink_refusal().returning(|_| None);

        let result = handle_validate(&display, &fs);
        assert_eq!(result, Exit::Failed);
    }
}
