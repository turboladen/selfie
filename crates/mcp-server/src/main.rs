mod event_collector;
mod server;

use anyhow::Result;
use rmcp::{ServiceExt, transport::io::stdio};
use selfie::{
    commands::ShellCommandRunner,
    config::{YamlLoader, loader::ConfigLoader},
    fs::RealFileSystem,
    package::{repository::yaml::YamlPackageRepository, service::PackageServiceImpl},
};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::WARN)
        .init();

    let fs = RealFileSystem;
    let config = YamlLoader::new(&fs).load_config()?;

    let repo = YamlPackageRepository::new(fs, config.package_directory().clone());
    let runner = ShellCommandRunner::new(
        ShellCommandRunner::default_shell(),
        config.command_timeout(),
    );
    let service = PackageServiceImpl::new(repo, runner, config.clone(), CancellationToken::new());

    let mcp_server = server::SelfieServer::new(service, config);

    let (stdin, stdout) = stdio();
    let service = mcp_server.serve((stdin, stdout)).await?;

    tracing::info!("selfie-mcp server started");

    service.waiting().await?;

    Ok(())
}
