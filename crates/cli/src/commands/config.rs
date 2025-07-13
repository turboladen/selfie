use selfie::config::AppConfig;
use tracing::info;

use crate::{
    commands::report_with_style, tables::ValidationTableReporter,
    terminal_progress_reporter::TerminalProgressReporter,
};

pub(crate) fn handle_validate(
    original_config: &AppConfig,
    reporter: TerminalProgressReporter,
) -> i32 {
    info!("Validating configuration");

    let result = original_config.validate();

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
        report_with_style("environment:", original_config.environment());
        report_with_style(
            "package_directory:",
            original_config.package_directory().display(),
        );
        report_with_style(
            "command_timeout:",
            format!("{} seconds", original_config.command_timeout().as_secs()),
        );
        report_with_style(
            "max_parallel_installations:",
            original_config.max_parallel_installations().get(),
        );
        report_with_style("stop_on_error:", original_config.stop_on_error());
        report_with_style("verbose:", original_config.verbose());
        report_with_style("use_colors:", original_config.use_colors());

        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_common::{test_config, test_config_verbose, test_config_with_colors};

    fn create_mock_reporter() -> TerminalProgressReporter {
        TerminalProgressReporter::new(false)
    }

    #[test]
    fn test_handle_validate_function_does_not_panic() {
        let config = test_config();
        let reporter = create_mock_reporter();

        // Test that the function doesn't panic and returns a valid exit code
        let result = handle_validate(&config, reporter);
        assert!(result == 0 || result == 1);
    }

    #[test]
    fn test_handle_validate_with_colors_enabled() {
        let config = test_config_with_colors();
        let reporter = TerminalProgressReporter::new(true);

        // Test that the function doesn't panic with colors enabled
        let result = handle_validate(&config, reporter);
        assert!(result == 0 || result == 1);
    }

    #[test]
    fn test_handle_validate_with_verbose_enabled() {
        let config = test_config_verbose();
        let reporter = create_mock_reporter();

        // Test that the function doesn't panic with verbose enabled
        let result = handle_validate(&config, reporter);
        assert!(result == 0 || result == 1);
    }
}
