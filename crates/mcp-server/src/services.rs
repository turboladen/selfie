//! The services the tools call, and the production set of them.

use std::sync::Arc;

use selfie::{
    commands::{ShellCommandRunner, ShellFallback},
    config::SelfieConfig,
    dotfile_service::{port::DotfileService, service::DotfileServiceImpl},
    fs::RealFileSystem,
    git::GixGitAdapter,
    package::{
        PackageService, SpecOrigin, SpecService, git_adapter::GixGitStatusProvider,
        repository::yaml::YamlPackageRepository, service::PackageServiceImpl,
    },
    privilege::{RealPrivilege, SudoPolicy},
    sync_service::{SyncService, service::SyncServiceImpl},
};
use tokio_util::sync::CancellationToken;

pub type ConcreteService = PackageServiceImpl<
    YamlPackageRepository<RealFileSystem>,
    ShellCommandRunner,
    GixGitStatusProvider,
>;

pub type ConcreteDotfileService = DotfileServiceImpl<
    YamlPackageRepository<RealFileSystem>,
    RealFileSystem,
    ShellCommandRunner,
    RealPrivilege,
>;

pub type ConcreteSyncService =
    SyncServiceImpl<GixGitAdapter, ConcreteDotfileService, RealPrivilege>;

/// The services one tool call runs on.
pub struct Scoped<S: ServiceSet + ?Sized> {
    pub package: Arc<S::Package>,
    pub dotfile: Arc<S::Dotfile>,
    pub sync: Arc<S::Sync>,
}

/// Supplies the services a tool call runs on, bound to that call's
/// cancellation token.
pub trait ServiceSet: Send + Sync + 'static {
    type Package: SpecService + PackageService + 'static;
    type Dotfile: DotfileService + 'static;
    type Sync: SyncService + 'static;

    /// The services for one request. Canceling `ct` stops the commands they
    /// start.
    fn for_request(&self, ct: CancellationToken) -> Scoped<Self>;

    /// The shell selfie replaced in `SHELL` for the commands these services
    /// run, if it replaced one.
    fn shell_fallback(&self) -> Option<&ShellFallback>;

    /// The configuration these services run under.
    fn config(&self) -> &SelfieConfig;
}

/// The services selfie-mcp runs on: the package, dotfile and sync services
/// over the configured directories, git and a shell.
pub struct LiveServices {
    package: ConcreteService,
    dotfile: ConcreteDotfileService,
    git: GixGitAdapter,
    config: SelfieConfig,
    shell_fallback: Option<ShellFallback>,
}

impl LiveServices {
    /// Builds the dotfile service from `config` and `runner`, beside `package`.
    /// Sync is built for each request over that request's dotfile service.
    pub fn new(
        package: ConcreteService,
        config: &SelfieConfig,
        runner: ShellCommandRunner,
        git: GixGitAdapter,
    ) -> Self {
        let repo = YamlPackageRepository::new(
            RealFileSystem,
            config.package_directory().to_path_buf(),
            SpecOrigin::PackageDirectory,
        );
        // Attached whether or not the directory exists. The dotfile service
        // reports a configured one that is missing on every call that reads it,
        // and a directory created after startup is read on the next call.
        let dotfiles_repo = dotfiles_repository(config);
        let shell_fallback = runner.shell_fallback().cloned();
        // The token given below is never canceled. A tool call runs on a copy of
        // the service that carries the request's own token (`for_request`), and
        // `main.rs` does the same for `PackageServiceImpl`.
        // No `allowing_sudo` call, and no tool parameter that could reach one: an
        // AI assistant has no reason to be driving selfie under sudo, so the
        // refusal here is unconditional.
        let dotfile = DotfileServiceImpl::new(
            repo,
            dotfiles_repo,
            RealFileSystem,
            // The package service's own runner, a login shell: a GUI-launched
            // MCP server does not inherit terminal PATH, and provider commands
            // (`op`, `teller`) live on the user's PATH.
            runner,
            config.clone(),
            CancellationToken::new(),
            SudoPolicy::new(RealPrivilege),
        );
        Self {
            package,
            dotfile,
            git,
            config: config.clone(),
            shell_fallback,
        }
    }
}

impl ServiceSet for LiveServices {
    type Package = ConcreteService;
    type Dotfile = ConcreteDotfileService;
    type Sync = ConcreteSyncService;

    fn for_request(&self, ct: CancellationToken) -> Scoped<Self> {
        let dotfile = self.dotfile.with_cancellation(ct.clone());
        // Built over the request's dotfile service, so the drift check sync
        // status runs stops with the request.
        let sync = SyncServiceImpl::new(
            self.git.clone(),
            dotfile.clone(),
            self.config.clone(),
            SudoPolicy::new(RealPrivilege),
        );
        Scoped {
            package: Arc::new(self.package.with_cancellation(ct)),
            dotfile: Arc::new(dotfile),
            sync: Arc::new(sync),
        }
    }

    fn shell_fallback(&self) -> Option<&ShellFallback> {
        self.shell_fallback.as_ref()
    }

    fn config(&self) -> &SelfieConfig {
        &self.config
    }
}

/// Returns the standalone dotfiles repository for `config`'s
/// `dotfiles_directory`. It is built whether or not the directory exists.
pub(crate) fn dotfiles_repository(config: &SelfieConfig) -> YamlPackageRepository<RealFileSystem> {
    YamlPackageRepository::new(
        RealFileSystem,
        config.dotfiles_directory(),
        SpecOrigin::DotfilesDirectory,
    )
}
