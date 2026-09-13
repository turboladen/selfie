//! List command handler for dotfiles
//!
//! This module handles the `selfie dotfiles list` CLI command, which shows
//! all dotfile mappings defined across packages and the standalone dotfiles
//! directory. This is a fast, file-only operation — no commands are executed.

use selfie::dotfile_service::port::DotfileService;
use selfie::package::{
    Package, SpecOrigin,
    event::{DotfileListData, OperationResult, PackageEvent},
};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::{
    commands::common::{create_dotfile_service, create_formatted_table},
    config::CliConfig,
    display_manager::{DisplayManager, shorten_path},
    event_processor::EventProcessor,
};

/// Handle the `selfie dotfiles list` command
///
/// Drives `DotfileService::list`, which reads both directories and reports every
/// spec it could not read. This command runs nothing: it renders var names and
/// command strings, never a resolved value, so it cannot leak a secret or raise
/// an authentication prompt.
pub(crate) async fn handle_list(
    config: &CliConfig,
    display: &DisplayManager,
    cancellation_token: CancellationToken,
) -> i32 {
    info!("Listing dotfiles");

    let service = create_dotfile_service(config, display, cancellation_token);
    let event_stream = service.list().await;

    let config_for_handler = config.clone();
    let display_for_handler = display.clone();
    let processor = EventProcessor::new(display.clone());
    let result = processor
        .process_events(event_stream, |event| match event {
            PackageEvent::DotfileListLoaded { dotfile_list, .. } => {
                render_listing(dotfile_list, &config_for_handler, &display_for_handler);
                true
            }
            // The envelope every service-driven command prints is noise on a
            // listing: the header repeats the environment the table is about,
            // and the completion line repeats the count the table just gave.
            // `spec search` suppresses its own for the same reason.
            //
            // Only a SUCCESSFUL completion. `process_events` skips its default
            // handler for anything a custom handler claims, and that default
            // handler is the only thing that writes the exit code -- so claiming
            // a failure here would print nothing and exit 0, which is the bug
            // PR #155 fixed for this very command.
            PackageEvent::Started { .. }
            | PackageEvent::Completed {
                result: OperationResult::Success(_),
                ..
            } => true,
            _ => false,
        })
        .await;

    result.exit_code
}

/// Render the table, or say there is nothing to put in one.
fn render_listing(data: &DotfileListData, config: &CliConfig, display: &DisplayManager) {
    // Before the table, and before the empty-listing line: a refused package may
    // be the only reason the table is short, and saying "no dotfiles found" over
    // one is the answer this reporting exists to stop.
    for refused in &data.refused {
        display.print_warning(format!(
            "Cannot read the dotfiles in '{}' ({}): {}",
            refused.package_name, refused.path, refused.reason
        ));
    }

    if data.packages.is_empty() {
        if data.refused.is_empty() {
            display.print_info("No dotfiles found in any packages.");
        }
        return;
    }

    print_base_directories(config, display, &data.packages);

    let mut table = create_formatted_table();
    table.set_header(vec!["Package", "Environment", "Source", "Target"]);

    let mut total = 0;
    for pkg in &data.packages {
        for (scope, entry) in pkg.dotfiles_with_scope() {
            table.add_row(vec![
                pkg.name().to_string(),
                scope.unwrap_or("(shared)").to_string(),
                // Renders var names and command strings, never a resolved value.
                //
                // A refused entry is shown as the reason it was refused rather
                // than omitted: it is in the package file, `selfie apply` will
                // report skipping it, and a listing that hid it would leave the
                // user looking for a dotfile the table says does not exist.
                entry
                    .content_source()
                    .map_or_else(|invalid| invalid.to_string(), |source| source.to_string()),
                shorten_path(entry.target()),
            ]);
            total += 1;
        }
    }

    display.println(table.to_string());
    display.print_info(format!(
        "{total} {} across {} {}",
        selfie::pluralize(total, "dotfile", "dotfiles"),
        data.packages.len(),
        selfie::pluralize(data.packages.len(), "package", "packages"),
    ));
}

/// Print the base directories above the table so relative source paths have context.
fn print_base_directories(config: &CliConfig, display: &DisplayManager, packages: &[Package]) {
    let has_packages = packages
        .iter()
        .any(|p| p.origin() == SpecOrigin::PackageDirectory);
    let has_dotfiles = packages
        .iter()
        .any(|p| p.origin() == SpecOrigin::DotfilesDirectory);

    if has_packages {
        display.print_info(format!(
            "Packages: {}",
            shorten_path(&config.package_directory().display().to_string()),
        ));
    }
    if has_dotfiles {
        display.print_info(format!(
            "Dotfiles: {}",
            shorten_path(
                &config
                    .selfie_config()
                    .dotfiles_directory()
                    .display()
                    .to_string()
            ),
        ));
    }
}
