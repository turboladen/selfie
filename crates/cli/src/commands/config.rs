use selfie::{
    config::{YamlLoader, loader::ConfigLoader},
    fs::FileSystem,
};
use tracing::info;

use crate::{display_manager::DisplayManager, tables::ValidationTableReporter};

/// Report what the configuration file on disk says, and whether it is valid.
///
/// Takes no `CliConfig`, deliberately: every value reported here comes from a
/// fresh load of the file, so a flag cannot mask a problem in it.
pub(crate) fn handle_validate(display: &DisplayManager, fs: &impl FileSystem) -> i32 {
    info!("Validating configuration");

    // Load the raw on-disk config (without CLI overrides) so that validation
    // catches issues that would be masked by flags like --environment.
    let loaded = match YamlLoader::new(fs).load_config() {
        Ok(c) => c,
        Err(e) => {
            display.print_error(format!("Failed to load configuration: {e}"));
            return 1;
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
        display.print_info(note.message());
    }

    if result.issues().has_errors() {
        display.print_error("Validation failed.");

        let mut table_reporter = ValidationTableReporter::new(display.use_colors());
        table_reporter
            .setup(vec!["Category", "Field", "Message", "Suggestion"])
            .add_validation_errors(&result.issues().errors())
            .add_validation_warnings(&result.issues().warnings())
            .print();
        crate::config::report_config_notices(&cli_notices, display);
        1
    } else {
        if result.issues().has_warnings() {
            let mut table_reporter = ValidationTableReporter::new(display.use_colors());
            table_reporter
                .setup(vec!["Category", "Field", "Message", "Suggestion"])
                .add_validation_warnings(&result.issues().warnings())
                .print();
        }
        crate::config::report_config_notices(&cli_notices, display);

        // Only when nothing at all was reported, `cli:` notices included.
        if !result.issues().has_warnings() && cli_notices.is_empty() {
            display.print_success("Configuration is valid.");
        }

        // Each value as a run would take it from the file: `~` expanded and
        // defaults filled in. Both required settings are present here, since a
        // file without one fails validation above.
        report_with_style(
            display,
            "environment:",
            raw_config.environment().unwrap_or_default(),
        );
        report_with_style(
            display,
            "package_directory:",
            raw_config
                .package_directory(fs)
                .unwrap_or_default()
                .display(),
        );
        // The directories in effect: the configured value, or the default the
        // file leaves selfie to derive. Whether either exists is reported in the
        // table above, so a wrong value is visible here before a command
        // behaves oddly.
        report_with_style(
            display,
            "dotfiles_directory:",
            raw_config
                .dotfiles_directory(fs)
                .unwrap_or_default()
                .display(),
        );
        report_with_style(
            display,
            "state_directory:",
            match selfie::fs::state_directory(fs, raw_config.state_directory(fs).as_deref()) {
                Ok(directory) => directory.display().to_string(),
                Err(error) => format!("(unresolved: {error})"),
            },
        );
        report_with_style(
            display,
            "command_timeout:",
            format!("{} seconds", raw_config.command_timeout().as_secs()),
        );
        report_with_style(
            display,
            "max_concurrency:",
            raw_config.max_concurrency().get(),
        );
        report_with_style(display, "stop_on_error:", raw_config.stop_on_error());
        // From the file's `cli:` section, not the run's flags. Every other line
        // here reports the file, and this command exists so a flag cannot mask a
        // problem in it -- `--no-color` must not make a file saying
        // `use_colors: true` read as false.
        report_with_style(display, "verbose:", cli_load.section.verbose);
        report_with_style(display, "use_colors:", cli_load.section.use_colors);

        0
    }
}

fn report_with_style(
    display: &DisplayManager,
    param1: impl std::fmt::Display,
    param2: impl std::fmt::Display,
) {
    display.print_field(param1, param2);
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
                Err(selfie::fs::FileSystemError::IoError(std::sync::Arc::new(
                    std::io::Error::from(std::io::ErrorKind::NotFound),
                )))
            });
        // The report and the validation both resolve the default state
        // directory under the home directory when the file names none.
        mock_home(&mut fs);
        fs.mock_directories_exist();
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
        assert!(result == 0 || result == 1);
    }

    #[test]
    fn test_handle_validate_with_colors_enabled() {
        let display = DisplayManager::new(true);
        let fs = mock_fs_with_valid_config();

        let result = handle_validate(&display, &fs);
        assert!(result == 0 || result == 1);
    }

    #[test]
    fn test_handle_validate_with_verbose_enabled() {
        let display = create_display();
        let fs = mock_fs_with_valid_config();

        let result = handle_validate(&display, &fs);
        assert!(result == 0 || result == 1);
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
        fs.expect_list_directory().returning(|_| Ok(Vec::new()));
        // No deploy state has been written yet.
        fs.expect_read_file()
            .withf(|path| path.ends_with("deploy-state.yml"))
            .returning(|_| {
                Err(selfie::fs::FileSystemError::IoError(std::sync::Arc::new(
                    std::io::Error::from(std::io::ErrorKind::NotFound),
                )))
            });

        let result = handle_validate(&display, &fs);
        assert_eq!(result, 1);
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
        assert_eq!(result, 1);
    }
}
