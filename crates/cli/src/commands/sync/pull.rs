//! Handler for `selfie sync pull`.

use console::style;

use selfie::{
    git::GixGitAdapter,
    package::event::{OperationResult, OperationSuccess, PackageEvent},
    sync_service::{SyncService, service::SyncServiceImpl},
};

use crate::{
    commands::common::create_dotfile_service,
    config::CliConfig,
    display_manager::{DisplayManager, INDENT},
    event_processor::EventProcessor,
};

pub(crate) async fn handle_pull(config: &CliConfig, display: &DisplayManager) -> i32 {
    let git = GixGitAdapter;
    let dotfile_service = create_dotfile_service(config);
    let service = SyncServiceImpl::new(git, dotfile_service, config.selfie_config().clone());

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
            let label = if *commits_pulled == 1 {
                "commit"
            } else {
                "commits"
            };
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
