mod event_collector;
mod server;
#[cfg(test)]
mod test_samples;

use std::io::Write as _;
use std::process::ExitCode;

use anyhow::Result;
use rmcp::{ServiceExt, transport::io::stdio};
use selfie::{
    commands::ShellCommandRunner,
    config::{Overrides, YamlLoader, loader::ConfigLoader},
    fs::RealFileSystem,
    git::GixGitAdapter,
    package::{
        SpecOrigin, git_adapter::GixGitStatusProvider, repository::yaml::YamlPackageRepository,
        service::PackageServiceImpl,
    },
};
use selfie_escape::LogEvent;
use tokio_util::sync::CancellationToken;

/// Recover HOME env var before the async runtime starts.
///
/// GUI-launched processes (like MCP servers started by Claude Desktop) may not
/// have HOME set, which breaks tilde expansion in config paths. We do this
/// before tokio starts so set_var is safe (truly single-threaded).
fn ensure_home_env() {
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

/// Returns the server's log subscriber. It logs WARN and above, never colored,
/// and writes each record on one line to a new sink from `sink`, with every
/// escape sequence, line break and invisible character in it escaped.
fn log_subscriber<W, F>(sink: F) -> impl tracing::Subscriber + Send + Sync + 'static
where
    W: std::io::Write + 'static,
    F: Fn() -> W + Send + Sync + 'static,
{
    // A logged value can come from a spec file, so it may hold a bidi control
    // or a line feed that would act on a terminal reading stderr or fake a log
    // line. Colors are off because `LogEvent` would escape their sequences.
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .with_writer(move || LogEvent::new(sink()))
        .finish()
}

/// Writes `panic` to `sink` as one log record, escaped as every record is.
fn write_panic(sink: impl std::io::Write, panic: &dyn std::fmt::Display) {
    let mut record = LogEvent::new(sink);
    // A panic hook has nowhere to report a failed write.
    let _ = writeln!(record, "selfie-mcp {panic}");
}

fn main() -> ExitCode {
    ensure_home_env();
    // Installed before anything can fail, so that an error returned below and a
    // panic both reach stderr escaped. Rust's own report of either writes the
    // message raw, and a config path or a spec's text can be in it.
    tracing_subscriber::util::SubscriberInitExt::init(log_subscriber(std::io::stderr));
    std::panic::set_hook(Box::new(|info| write_panic(std::io::stderr(), info)));

    let run = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(anyhow::Error::from)
        .and_then(|runtime| runtime.block_on(async_main()));
    match run {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!("{error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn async_main() -> Result<()> {
    let fs = RealFileSystem;
    let loaded = YamlLoader::new(&fs).load_config()?;

    // Also carried into `selfie_config_get` below. stderr from an MCP server is
    // often invisible to the assistant driving it, so the tool response is where
    // a warning actually gets read. Logged before resolving, since a misspelled
    // key may be the very setting that is missing.
    for ignored in loaded.ignored_keys() {
        tracing::warn!("{} {}", ignored.message(), ignored.suggestion());
    }

    // The server has no flags, so the file alone must name every required
    // setting.
    let config = loaded
        .config()
        .resolve(&fs, &Overrides::default())
        .map_err(|missing| {
            anyhow::anyhow!(
                "The configuration file {} does not set every required setting. {missing}",
                loaded.path().display()
            )
        })?;

    tracing::warn!(
        "Config loaded: environment={}, package_directory={}",
        config.environment(),
        config.package_directory().display()
    );

    let ignored_keys = loaded.ignored_keys().to_vec();

    let repo = YamlPackageRepository::new(
        fs,
        config.package_directory().clone(),
        SpecOrigin::PackageDirectory,
    );
    // Use a login shell so the user's PATH includes tools like ~/.cargo/bin,
    // homebrew, fnm, etc. GUI-launched processes (like MCP servers started by
    // Claude Desktop) don't inherit the terminal's environment.
    let runner = ShellCommandRunner::login_shell(config.command_timeout());
    let service = PackageServiceImpl::new(
        repo,
        server::dotfiles_repository(&config),
        runner,
        GixGitStatusProvider::default(),
        config.clone(),
        // Never canceled: each tool call runs on a copy of the service that
        // carries that request's own token, and rmcp cancels that one when the
        // client cancels the call or disconnects. `server.rs` does the same for the
        // dotfile service.
        CancellationToken::new(),
    );

    let mcp_server =
        server::SelfieServer::new(service, config, ignored_keys, GixGitAdapter::default());

    let in_flight = mcp_server.in_flight();
    let (stdin, stdout) = stdio();
    let service = mcp_server.serve((stdin, stdout)).await?;

    let stopped = service.waiting().await;
    // Waited on even when the server stopped with an error, because a call that
    // was canceled is still ending its command.
    server::wait_for_calls(&in_flight, server::SHUTDOWN_WAIT).await;
    stopped?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    // A sink every writer the subscriber makes appends to.
    #[derive(Clone, Default)]
    struct Logged(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Logged {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    // What the server's subscriber writes while `log` runs.
    fn logged(log: impl FnOnce()) -> String {
        let sink = Logged::default();
        let writer = sink.clone();
        tracing::subscriber::with_default(log_subscriber(move || writer.clone()), log);
        String::from_utf8(sink.0.lock().unwrap().clone()).unwrap()
    }

    // A value from a spec file can hold a bidi override, an invisible
    // character, a tag character or a line feed. Each reaches the log as an
    // escape, in a message and in a field alike, and the record stays one line.
    #[test]
    fn a_logged_value_is_escaped_on_one_line() {
        let value = "a\u{202e}b\u{200b}c\u{e0041}d\ne\u{1b}[2J";
        // tracing writes an ESC in a value as `\x1b` before the writer sees it,
        // so only its absence below is asserted.
        let shown = r"a\u{202e}b\u{200b}c\u{e0041}d\ne";
        let log = logged(|| {
            tracing::warn!("{value}");
            tracing::warn!(key = %value, "field");
        });
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 2, "{log}");
        for line in &lines {
            assert!(line.contains(shown), "{line}");
        }
        for raw in ['\u{202e}', '\u{200b}', '\u{e0041}', '\u{1b}'] {
            assert!(!log.contains(raw), "{raw:?} in {log}");
        }
    }

    // A panic's message reaches stderr as one escaped record, so a value in it
    // cannot reorder the line or start one of its own.
    #[test]
    fn a_panic_is_written_as_one_escaped_record() {
        let mut sink = Vec::new();
        write_panic(&mut sink, &"panicked at x:\nbad \u{202e}name");
        assert_eq!(
            String::from_utf8(sink).unwrap(),
            "selfie-mcp panicked at x:\\nbad \\u{202e}name\n"
        );
    }

    // A plain warning is written as it was logged, so the escape is not
    // passing by writing nothing, and nothing below WARN is written.
    #[test]
    fn a_plain_warning_is_written_as_it_is() {
        let log = logged(|| {
            tracing::info!("not logged");
            tracing::warn!("Config loaded: environment=macos");
        });
        assert_eq!(log.lines().count(), 1, "{log}");
        assert!(
            log.trim_end()
                .ends_with(" WARN selfie_mcp::tests: Config loaded: environment=macos"),
            "{log}"
        );
    }
}
