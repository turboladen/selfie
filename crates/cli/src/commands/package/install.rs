use selfie::package::{
    event::{OperationFailure, OperationResult, PackageEvent},
    port::PackageError,
    service::{InstallOptions, PackageService},
};
use tracing::info;

use crate::{
    commands::common, config::CliConfig, display_manager::DisplayManager,
    event_processor::EventProcessor,
};

pub(crate) async fn handle_install(
    service: &impl PackageService,
    package_name: &str,
    options: InstallOptions,
    config: &CliConfig,
    display: &DisplayManager,
) -> i32 {
    info!("Installing package: {}", package_name);

    let event_stream = service.install(package_name, options).await;

    let processor = EventProcessor::new(display.clone());
    let result = processor
        .process_events(event_stream, |event| {
            // Check for environment errors in Completed events
            if let PackageEvent::Completed {
                result: OperationResult::Failure(failure),
                ..
            } = event
                && failure.is_environment_error()
            {
                display_environment_error(package_name, failure, config, display);
                return true;
            }

            match event {
                // What the pre-install check found, before the install runs: detail
                // about the run, shown under `--verbose`.
                PackageEvent::CheckResultCompleted { check_result, .. } => {
                    let message = match &check_result.result {
                        selfie::package::event::CheckResult::Success { .. } => {
                            "Package is already installed".to_string()
                        }
                        selfie::package::event::CheckResult::Failed { .. } => {
                            "Package not currently installed, proceeding with installation"
                                .to_string()
                        }
                        selfie::package::event::CheckResult::NoCheckCommand => {
                            "No check command defined, proceeding with installation".to_string()
                        }
                        selfie::package::event::CheckResult::Error(err) => {
                            format!("Check error ({err}), proceeding with installation")
                        }
                        selfie::package::event::CheckResult::TimedOut(timed_out) => {
                            format!("Check {timed_out}; proceeding with installation")
                        }
                    };
                    display.print_status(message);
                    true
                }
                _ => false,
            }
        })
        .await;

    result.exit_code
}

/// Display environment error with helpful suggestions from the typed failure data
///
/// The failure names the package it is about, which may be a dependency or a
/// recommend of `package_name`, the package the user asked for.
fn display_environment_error(
    package_name: &str,
    failure: &OperationFailure,
    config: &CliConfig,
    display: &DisplayManager,
) {
    display.print_note("");

    match failure {
        OperationFailure::Package(PackageError::EnvironmentNotFound {
            package_name: failing,
            available_environments,
            required_by,
            ..
        }) => {
            common::display_environment_summary(
                failing,
                config.environment(),
                available_environments,
                config,
                display,
                "install",
            );
            display_required_by(display, required_by.as_deref());
        }
        OperationFailure::Package(PackageError::NoInstallCommand {
            package_name: failing,
            environment,
            other_envs_with_install,
            required_by,
            ..
        }) => {
            common::display_missing_command(
                display,
                "install",
                failing,
                environment,
                other_envs_with_install,
            );
            display_required_by(display, required_by.as_deref());
        }
        _ => {
            common::display_generic_environment_suggestion(
                package_name,
                config.environment(),
                config,
                display,
                "install",
            );
        }
    }
}

/// Say which package needs the one the failure is about, when it is a dependency
/// or a recommend.
fn display_required_by(display: &DisplayManager, required_by: Option<&str>) {
    if let Some(required_by) = required_by {
        display.print_note(format!(
            "{}'{required_by}' requires it.",
            crate::display_manager::INDENT
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_display() -> DisplayManager {
        DisplayManager::new(false)
    }

    #[tokio::test]
    async fn test_handle_install_basic() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config = CliConfig::wrap_for_test(test_common::test_config_with_dir(temp_dir.path()));
        let service = test_common::create_test_service(&temp_dir);
        let display = create_display();

        // This will fail without proper setup, but tests that the function can be called
        let _result = handle_install(
            &service,
            "test-package",
            InstallOptions::default(),
            &config,
            &display,
        )
        .await;
    }

    #[test]
    fn test_installation_display() {
        let display = create_display();

        // Test that DisplayManager output methods don't panic
        display.print_progress("test progress");
        display.println("test output line");
        display.print_info("test info");
        display.print_success("test success");
        display.print_error("test error");

        // Test that clone shares state
        let display2 = display.clone();
        display2.print_progress("cloned display output");
    }
}
