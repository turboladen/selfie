//! Handler for `selfie sync pull`.

use console::style;
use tokio_util::sync::CancellationToken;

use selfie::{
    package::event::{OperationResult, OperationSuccess, PackageEvent},
    sync_service::SyncService,
};

use crate::{
    commands::common::create_sync_service,
    config::CliConfig,
    display_manager::{DisplayManager, INDENT},
    event_processor::EventProcessor,
};

pub(crate) async fn handle_pull(
    config: &CliConfig,
    display: &DisplayManager,
    cancellation_token: CancellationToken,
) -> i32 {
    let service = create_sync_service(config, display, cancellation_token);

    let event_stream = service.pull().await;

    let display_for_handler = display.clone();
    let use_colors = config.use_colors();
    let processor = EventProcessor::new(display.clone());
    let result = processor
        .process_events(event_stream, move |event| {
            handle_pull_event(event, &display_for_handler, use_colors)
        })
        .await;

    result.exit_code
}

fn handle_pull_event(event: &PackageEvent, display: &DisplayManager, use_colors: bool) -> bool {
    match event {
        PackageEvent::Completed {
            result:
                OperationResult::Success(OperationSuccess::SyncPullComplete {
                    commits_pulled,
                    packages_updated,
                    packages_added,
                    packages_removed,
                    ..
                }),
            ..
        } => {
            let label = selfie::pluralize(*commits_pulled, "commit", "commits");
            display.print_success(format!("Pulled {commits_pulled} {label} from remote"));

            if !packages_updated.is_empty() {
                let names = format_package_list(packages_updated, use_colors);
                display.println(format!("{INDENT}updated: {names}"));
            }
            if !packages_added.is_empty() {
                let names = format_package_list(packages_added, use_colors);
                display.println(format!("{INDENT}added: {names}"));
            }
            if !packages_removed.is_empty() {
                let names = format_package_list(packages_removed, use_colors);
                display.println(format!("{INDENT}removed: {names}"));
            }
            true
        }

        PackageEvent::Completed {
            result: OperationResult::Success(OperationSuccess::SyncPullUpToDate { .. }),
            ..
        } => {
            display.print_success("Already up to date");
            true
        }

        // Suppress started — but show progress (fetch/merge steps)
        PackageEvent::Started { .. } => true,

        _ => false,
    }
}

fn format_package_list(names: &[String], use_colors: bool) -> String {
    if use_colors {
        names
            .iter()
            .map(|n| style(n).bold().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        names.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use selfie::package::event::{OperationContext, OperationInfo, OperationType, StepCount};

    fn make_operation_info() -> OperationInfo {
        OperationInfo {
            id: uuid::Uuid::new_v4(),
            operation_type: OperationType::SyncPull,
            package_name: String::new(),
            environment: "test".to_string(),
            context: OperationContext::default(),
            timestamp: std::time::Instant::now(),
        }
    }

    #[test]
    fn handles_pull_complete_with_changes() {
        let display = DisplayManager::new(false);
        let event = PackageEvent::Completed {
            operation_info: make_operation_info(),
            result: OperationResult::Success(OperationSuccess::SyncPullComplete {
                commits_pulled: 2,
                packages_updated: vec!["starship".to_string()],
                packages_added: vec!["fnm".to_string()],
                packages_removed: vec![],
                steps_completed: StepCount::new(3, 3),
            }),
        };

        assert!(handle_pull_event(&event, &display, false));
    }

    #[test]
    fn handles_already_up_to_date() {
        let display = DisplayManager::new(false);
        let event = PackageEvent::Completed {
            operation_info: make_operation_info(),
            result: OperationResult::Success(OperationSuccess::SyncPullUpToDate {
                steps_completed: StepCount::new(3, 3),
            }),
        };

        assert!(handle_pull_event(&event, &display, false));
    }

    #[test]
    fn suppresses_started() {
        let display = DisplayManager::new(false);
        let started = PackageEvent::Started {
            operation_info: make_operation_info(),
        };
        assert!(handle_pull_event(&started, &display, false));
    }

    #[test]
    fn format_package_list_no_colors() {
        let names = vec!["starship".to_string(), "fnm".to_string()];
        assert_eq!(format_package_list(&names, false), "starship, fnm");
    }
}
