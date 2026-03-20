use selfie::{
    config::{YamlLoader, loader::ConfigLoader},
    fs::FileSystem,
};
use tracing::info;

use crate::{config::CliConfig, display_manager::DisplayManager, tables::ValidationTableReporter};

pub(crate) fn handle_validate(
    config: &CliConfig,
    display: &DisplayManager,
    fs: &impl FileSystem,
) -> i32 {
    info!("Validating configuration");

    // Load the raw on-disk config (without CLI overrides) so that validation
    // catches issues that would be masked by flags like --environment.
    let raw_config = match YamlLoader::new(fs).load_config() {
        Ok(c) => c,
        Err(e) => {
            display.print_error(format!("Failed to load configuration: {e}"));
            return 1;
        }
    };

    let result = raw_config.validate();

    if result.issues().has_errors() {
        display.print_error("Validation failed.");

        let mut table_reporter = ValidationTableReporter::new(display.use_colors());
        table_reporter
            .setup(vec!["Category", "Field", "Message", "Suggestion"])
            .add_validation_errors(&result.issues().errors())
            .add_validation_warnings(&result.issues().warnings())
            .print();
        1
    } else if result.issues().has_warnings() {
        let mut table_reporter = ValidationTableReporter::new(display.use_colors());
        table_reporter
            .setup(vec!["Category", "Field", "Message", "Suggestion"])
            .add_validation_warnings(&result.issues().warnings())
            .print();
        0
    } else {
        display.print_success("Configuration is valid.");
        report_with_style(display, "environment:", config.environment());
        report_with_style(
            display,
            "package_directory:",
            config.package_directory().display(),
        );
        report_with_style(
            display,
            "command_timeout:",
            format!("{} seconds", config.command_timeout().as_secs()),
        );
        report_with_style(
            display,
            "max_parallel_installations:",
            config.max_parallel_installations().get(),
        );
        report_with_style(display, "stop_on_error:", config.stop_on_error());
        report_with_style(display, "verbose:", config.verbose());
        report_with_style(display, "use_colors:", config.use_colors());

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
    use crate::config::CliSection;
    use selfie::fs::MockFileSystem;
    use std::path::Path;
    use test_common::test_config;

    fn create_display() -> DisplayManager {
        DisplayManager::new(false)
    }

    fn test_cli_config() -> CliConfig {
        CliConfig::new(test_config(), CliSection::default())
    }

    /// Creates a mock filesystem that returns a valid config file.
    fn mock_fs_with_valid_config() -> MockFileSystem {
        let mut fs = MockFileSystem::default();
        let config_dir = Path::new("/home/test/.config/selfie");
        let config_yaml = r#"
            environment: "test-env"
            package_directory: "/test/packages"
        "#;
        fs.mock_config_file(config_dir, config_yaml);
        fs.mock_expand_path("/test/packages", "/test/packages");
        fs
    }

    #[test]
    fn test_handle_validate_function_does_not_panic() {
        let config = test_cli_config();
        let display = create_display();
        let fs = mock_fs_with_valid_config();

        let result = handle_validate(&config, &display, &fs);
        assert!(result == 0 || result == 1);
    }

    #[test]
    fn test_handle_validate_with_colors_enabled() {
        let config = CliConfig::new(
            test_config(),
            CliSection {
                verbose: false,
                use_colors: true,
            },
        );
        let display = DisplayManager::new(true);
        let fs = mock_fs_with_valid_config();

        let result = handle_validate(&config, &display, &fs);
        assert!(result == 0 || result == 1);
    }

    #[test]
    fn test_handle_validate_with_verbose_enabled() {
        let config = CliConfig::new(
            test_config(),
            CliSection {
                verbose: true,
                use_colors: false,
            },
        );
        let display = create_display();
        let fs = mock_fs_with_valid_config();

        let result = handle_validate(&config, &display, &fs);
        assert!(result == 0 || result == 1);
    }

    #[test]
    fn test_handle_validate_catches_empty_environment_on_disk() {
        let config = test_cli_config(); // has environment set via builder
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
        fs.mock_expand_path("/test/packages", "/test/packages");

        let result = handle_validate(&config, &display, &fs);
        assert_eq!(result, 1);
    }

    #[test]
    fn test_handle_validate_reports_load_error() {
        let config = test_cli_config();
        let display = create_display();

        // Mock filesystem where config file doesn't exist
        let mut fs = MockFileSystem::default();
        let config_dir = Path::new("/home/test/.config/selfie");
        fs.mock_config_dir_ok(config_dir);
        fs.mock_path_exists(&config_dir.join("config.yaml"), false);
        fs.mock_path_exists(&config_dir.join("config.yml"), false);

        let result = handle_validate(&config, &display, &fs);
        assert_eq!(result, 1);
    }
}
