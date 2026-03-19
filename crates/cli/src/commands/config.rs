use tracing::info;

use crate::{config::CliConfig, display_manager::DisplayManager, tables::ValidationTableReporter};

pub(crate) fn handle_validate(config: &CliConfig, display: &DisplayManager) -> i32 {
    info!("Validating configuration");

    let result = config.selfie_config().validate();

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
    use test_common::test_config;

    fn create_display() -> DisplayManager {
        DisplayManager::new(false)
    }

    fn test_cli_config() -> CliConfig {
        CliConfig::new(test_config(), CliSection::default())
    }

    #[test]
    fn test_handle_validate_function_does_not_panic() {
        let config = test_cli_config();
        let display = create_display();

        // Test that the function doesn't panic and returns a valid exit code
        let result = handle_validate(&config, &display);
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

        // Test that the function doesn't panic with colors enabled
        let result = handle_validate(&config, &display);
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

        // Test that the function doesn't panic with verbose enabled
        let result = handle_validate(&config, &display);
        assert!(result == 0 || result == 1);
    }
}
