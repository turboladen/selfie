mod event_collector;
mod server;

use anyhow::Result;
use rmcp::{ServiceExt, transport::io::stdio};
use selfie::{
    commands::ShellCommandRunner,
    config::{YamlLoader, loader::ConfigLoader},
    fs::RealFileSystem,
    package::{
        git_adapter::GixGitStatusProvider, repository::yaml::YamlPackageRepository,
        service::PackageServiceImpl,
    },
};
use tokio_util::sync::CancellationToken;

/// Recover HOME env var before the async runtime starts.
///
/// GUI-launched processes (like MCP servers started by Claude Desktop) may not
/// have HOME set, which breaks tilde expansion in config paths. We do this
/// before tokio starts so set_var is safe (truly single-threaded).
fn ensure_home_env() {
    #[cfg(unix)]
    if std::env::var("HOME").is_err() {
        use std::ffi::CStr;
        let pw = unsafe { libc::getpwuid(libc::getuid()) };
        if !pw.is_null() {
            let home = unsafe { CStr::from_ptr((*pw).pw_dir) };
            if let Ok(home_str) = home.to_str() {
                // SAFETY: called before tokio runtime starts — truly single-threaded
                unsafe { std::env::set_var("HOME", home_str) };
            }
        }
    }
}

fn main() -> Result<()> {
    ensure_home_env();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async_main())
}

async fn async_main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::WARN)
        .init();

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
    let service = PackageServiceImpl::new(
        repo,
        runner,
        GixGitStatusProvider,
        config.clone(),
        // A fresh token, deliberately: an MCP server has no signal handler and no
        // interactive user to press Ctrl+C, so there is nothing to cancel with.
        // `command_timeout` remains the bound on a command that blocks.
        // `server.rs` says the same about the dotfile service.
        CancellationToken::new(),
    );

    let mcp_server = server::SelfieServer::new(service, config);

    let (stdin, stdout) = stdio();
    let service = mcp_server.serve((stdin, stdout)).await?;

    service.waiting().await?;

    Ok(())
}
