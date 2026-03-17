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

    // Ensure HOME is set — GUI-launched processes (like MCP servers started by
    // Claude Desktop) may not have it, which breaks tilde expansion in config paths.
    #[cfg(unix)]
    if std::env::var("HOME").is_err() {
        use std::ffi::CStr;
        let pw = unsafe { libc::getpwuid(libc::getuid()) };
        if !pw.is_null() {
            let home = unsafe { CStr::from_ptr((*pw).pw_dir) };
            if let Ok(home_str) = home.to_str() {
                // SAFETY: single-threaded at startup, before tokio runtime spawns tasks
                unsafe { std::env::set_var("HOME", home_str) };
            }
        }
    }

    let fs = RealFileSystem;
    let config = YamlLoader::new(&fs).load_config()?;

    tracing::warn!(
        "Config loaded: environment={}, package_directory={}",
        config.environment(),
        config.package_directory().display()
    );

    let repo = YamlPackageRepository::new(fs, config.package_directory().clone());
    // Use a login shell so the user's PATH includes tools like ~/.cargo/bin,
    // homebrew, fnm, etc. GUI-launched processes (like MCP servers started by
    // Claude Desktop) don't inherit the terminal's environment.
    let runner = ShellCommandRunner::login_shell(config.command_timeout());
    let service = PackageServiceImpl::new(repo, runner, config.clone(), CancellationToken::new());

    let mcp_server = server::SelfieServer::new(service, config);

    let (stdin, stdout) = stdio();
    let service = mcp_server.serve((stdin, stdout)).await?;

    service.waiting().await?;

    Ok(())
}
