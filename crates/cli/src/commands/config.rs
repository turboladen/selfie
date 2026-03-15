use tracing::info;

use crate::{
    commands::report_with_style, config::CliConfig, tables::ValidationTableReporter,
    terminal_progress_reporter::TerminalProgressReporter,
};

pub(crate) fn handle_validate(config: &CliConfig, reporter: TerminalProgressReporter) -> i32 {
    info!("Validating configuration");

    let result = config.selfie_config().validate();

    if result.issues().has_errors() {
        reporter.report_error("Validation failed.");

        let mut table_reporter = ValidationTableReporter::new();
        table_reporter
            .setup(vec!["Category", "Field", "Message", "Suggestion"])
            .add_validation_errors(&result.issues().errors(), reporter)
            .add_validation_warnings(&result.issues().warnings(), reporter)
            .print();
        1
    } else if result.issues().has_warnings() {
        let mut table_reporter = ValidationTableReporter::new();
        table_reporter
            .setup(vec!["Category", "Field", "Message", "Suggestion"])
            .add_validation_warnings(&result.issues().warnings(), reporter)
            .print();
        0
    } else {
        reporter.report_success("Configuration is valid.");
        report_with_style("environment:", config.environment());
        report_with_style("package_directory:", config.package_directory().display());
        report_with_style(
            "command_timeout:",
            format!("{} seconds", config.command_timeout().as_secs()),
        );
        report_with_style(
            "max_parallel_installations:",
            config.max_parallel_installations().get(),
        );
        report_with_style("stop_on_error:", config.stop_on_error());
        report_with_style("verbose:", config.verbose());
        report_with_style("use_colors:", config.use_colors());

        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CliSection;
    use test_common::test_config;

    fn create_mock_reporter() -> TerminalProgressReporter {
        TerminalProgressReporter::new(false)
    }

    fn test_cli_config() -> CliConfig {
        CliConfig::new(test_config(), CliSection::default())
    }

    #[test]
    fn test_handle_validate_function_does_not_panic() {
        let config = test_cli_config();
        let reporter = create_mock_reporter();

        // Test that the function doesn't panic and returns a valid exit code
        let result = handle_validate(&config, reporter);
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
        let reporter = TerminalProgressReporter::new(true);

        // Test that the function doesn't panic with colors enabled
        let result = handle_validate(&config, reporter);
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
        let reporter = create_mock_reporter();

        // Test that the function doesn't panic with verbose enabled
        let result = handle_validate(&config, reporter);
        assert!(result == 0 || result == 1);
    }
}
