//! List command handler for dotfiles
//!
//! This module handles the `selfie dotfiles list` CLI command, which shows
//! all dotfile mappings defined across packages and the standalone dotfiles
//! directory. This is a fast, file-only operation — no commands are executed.

use crate::terminal_text::shown;
use selfie::dotfile_service::port::DotfileService;
use selfie::package::{
    ContentSource, Package, SpecOrigin,
    event::{BaseKind, DotfileListData, OperationResult, PackageEvent},
};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::{
    commands::common::create_dotfile_service,
    config::CliConfig,
    display_manager::{DisplayManager, DisplayPath, shorten_path},
    event_processor::{EventProcessor, Exit},
    source_paths::base_directory_line,
    tables::{Cell, SafeTable},
    terminal_text::{Name, Shown, lit},
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
) -> Exit {
    info!("Listing dotfiles");

    let service = create_dotfile_service(config, cancellation_token);
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
            // The completion line repeats the count the table just gave. `spec
            // search` suppresses its own for the same reason.
            //
            // Only a SUCCESSFUL completion: a failure must still print, and only
            // the default handler renders one.
            PackageEvent::Completed {
                result: OperationResult::Success(_),
                ..
            } => true,
            _ => false,
        })
        .await;

    result.exit
}

/// Render the table, or say there is nothing to put in one.
fn render_listing(data: &DotfileListData, config: &CliConfig, display: &DisplayManager) {
    // Before the table, and before the empty-listing line: a refused package may
    // be the only reason the table is short, and saying "no dotfiles found" over
    // one is the answer this reporting exists to stop.
    for refused in &data.refused {
        display.print_warning(shown!(
            "Cannot read the dotfiles in '{}' ({}): {}",
            Name::new(&refused.package_name),
            DisplayPath::of(&refused.path),
            crate::received::refusal_reason(refused)
        ));
    }

    if data.packages.is_empty() {
        if data.refused.is_empty() {
            display.print_info(lit!("No dotfiles found in any packages."));
        }
        return;
    }

    print_base_directories(config, display, &data.packages);

    let mut table = SafeTable::new();
    table.set_header([
        lit!("Package"),
        lit!("Environment"),
        lit!("Content"),
        lit!("Target"),
    ]);

    let mut total = 0;
    for pkg in &data.packages {
        for (scope, entry) in pkg.dotfiles_with_scope() {
            table.add_row([
                Cell::from(Name::new(pkg.name())),
                scope.map_or_else(|| lit!("(shared)").into(), |scope| Name::new(scope).into()),
                source_cell(entry).into(),
                shorten_path(entry.target()).into(),
            ]);
            total += 1;
        }
    }

    display.println(shown!("{}", table));
    display.print_info(shown!(
        "{} {} across {} {}",
        total,
        crate::formatters::plural(total, lit!("dotfile"), lit!("dotfiles")),
        data.packages.len(),
        crate::formatters::plural(data.packages.len(), lit!("package"), lit!("packages"))
    ));
}

/// Where `entry`'s content comes from, as a table cell: var names and command
/// strings, never a resolved value.
pub(crate) fn source_cell(entry: &selfie::package::DotfileEntry) -> Shown {
    // A refused entry is shown as the reason it was refused rather than omitted:
    // it is in the package file, `selfie apply` will report skipping it, and a
    // listing that hid it would leave the user looking for a dotfile the table
    // says does not exist. The brief form, since a cell cannot hold the anchor
    // advice.
    match entry.content_source() {
        Ok(source @ ContentSource::Provider(_)) => {
            crate::terminal_text::interim_one_line(&source.to_string())
        }
        Ok(source) => DisplayPath::new(&source.to_string()).into(),
        Err(invalid) => DisplayPath::new(&invalid.brief()).into(),
    }
}

/// Print the base directories above the table so relative source paths have context.
fn print_base_directories(config: &CliConfig, display: &DisplayManager, packages: &[Package]) {
    let has = |origin| packages.iter().any(|p| p.origin() == origin);

    if has(SpecOrigin::PackageDirectory) {
        display.print_info(base_directory_line(
            BaseKind::PackageDirectory,
            config.package_directory(),
        ));
    }
    if has(SpecOrigin::DotfilesDirectory) {
        display.print_info(base_directory_line(
            BaseKind::DotfilesDirectory,
            &config.selfie_config().dotfiles_directory(),
        ));
    }
}
