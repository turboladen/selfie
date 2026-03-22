//! Apply command handler for deploying config files
//!
//! This module handles the `selfie apply` CLI command, which deploys
//! configuration files defined in package YAML files to their target
//! locations on the system.

use selfie::{
    config_service::{
        port::{ApplyOptions, ConfigService},
        service::ConfigServiceImpl,
    },
    fs::real::RealFileSystem,
};
use tracing::info;

use crate::{
    cli::ApplyArgs, commands::common::create_package_repository, config::CliConfig,
    display_manager::DisplayManager, event_processor::EventProcessor,
};

/// Handle the apply command
///
/// Creates a `ConfigServiceImpl` and delegates to `apply_all` or `apply`
/// based on whether a package name was given. Events are processed through
/// the standard `EventProcessor`.
pub(crate) async fn handle_apply(
    args: &ApplyArgs,
    config: &CliConfig,
    display: &DisplayManager,
) -> i32 {
    let options = ApplyOptions {
        dry_run: args.dry_run,
        auto_accept: args.yes,
    };

    let repo = create_package_repository(config);
    let fs = RealFileSystem;
    let service = ConfigServiceImpl::new(repo, fs, config.selfie_config().clone());

    let event_stream = if let Some(name) = &args.name {
        info!("Applying config for package: {}", name);
        service.apply(name, options).await
    } else {
        info!("Applying all config files");
        service.apply_all(options).await
    };

    let processor = EventProcessor::new(display.clone());
    let result = processor.process_events(event_stream, |_event| false).await;

    result.exit_code
}
