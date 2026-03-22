//! Apply command handler for deploying dotfiles
//!
//! This module handles the `selfie apply` CLI command, which deploys
//! dotfiles defined in package YAML files to their target
//! locations on the system.

use selfie::{
    dotfile_service::{
        port::{ApplyOptions, DotfileService},
        service::DotfileServiceImpl,
    },
    fs::real::RealFileSystem,
    package::repository::yaml::YamlPackageRepository,
};
use tracing::info;

use crate::{
    cli::ApplyArgs, commands::common::create_package_repository, config::CliConfig,
    display_manager::DisplayManager, event_processor::EventProcessor,
};

/// Handle the apply command
///
/// Creates a `DotfileServiceImpl` and delegates to `apply_all` or `apply`
/// based on whether a package name was given. When a `dotfiles/` directory
/// exists (sibling of `package_directory`), it's added as a second source
/// so standalone dotfiles are included in the apply. Events are processed
/// through the standard `EventProcessor`.
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
    let mut service = DotfileServiceImpl::new(repo, fs, config.selfie_config().clone());

    // Add standalone dotfiles repository if the directory exists
    let dotfiles_dir = config.selfie_config().dotfiles_directory();
    if dotfiles_dir.is_dir() {
        let dotfiles_repo = YamlPackageRepository::new(RealFileSystem, dotfiles_dir);
        service = service.with_dotfiles_repository(dotfiles_repo);
    }

    let event_stream = if let Some(name) = &args.name {
        info!("Applying dotfiles for package: {}", name);
        service.apply(name, options).await
    } else {
        info!("Applying all dotfiles");
        service.apply_all(options).await
    };

    let processor = EventProcessor::new(display.clone());
    let result = processor.process_events(event_stream, |_event| false).await;

    result.exit_code
}
