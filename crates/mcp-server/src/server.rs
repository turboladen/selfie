use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{
    ErrorData as McpError,
    handler::server::ServerHandler,
    model::{
        CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo,
        ToolsCapability,
    },
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use selfie::{
    commands::ShellCommandRunner,
    config::{IgnoredKey, SelfieConfig},
    dotfile_service::{port::ApplyOptions, service::DotfileServiceImpl},
    fs::RealFileSystem,
    git::GixGitAdapter,
    package::{
        EnvironmentConfig, Package, PackageService, SpecOrigin, SpecService,
        event::PackageUpdateFields, git_adapter::GixGitStatusProvider,
        repository::yaml::YamlPackageRepository, service::PackageServiceImpl,
    },
    privilege::{RealPrivilege, SudoPolicy},
    sync_service::{ConfirmedCommit, PushOptions, SyncService, service::SyncServiceImpl},
};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::event_collector;

type ConcreteService = PackageServiceImpl<
    YamlPackageRepository<RealFileSystem>,
    ShellCommandRunner,
    GixGitStatusProvider,
>;

type ConcreteDotfileService = DotfileServiceImpl<
    YamlPackageRepository<RealFileSystem>,
    RealFileSystem,
    ShellCommandRunner,
    RealPrivilege,
>;

type ConcreteSyncService = SyncServiceImpl<GixGitAdapter, ConcreteDotfileService, RealPrivilege>;

#[derive(Clone)]
pub struct SelfieServer {
    service: Arc<ConcreteService>,
    dotfile_service: Arc<ConcreteDotfileService>,
    sync_service: Arc<ConcreteSyncService>,
    config: SelfieConfig,
    /// Keys the configuration file carried that selfie did not use.
    ///
    /// Reported through `selfie_config_get` rather than only logged: an
    /// assistant driving this server usually cannot see stderr.
    ignored_config_keys: Vec<IgnoredKey>,
}

#[derive(Deserialize, JsonSchema)]
pub struct PackageNameParam {
    /// Name of the package
    pub package: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct InstallParam {
    /// Name of the package to install
    pub package: String,
    /// Skip installing recommended (soft) dependencies
    #[serde(default)]
    pub skip_recommends: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct CreateParam {
    /// Name of the new package
    pub package: String,
    /// Install command
    pub install: String,
    /// Environment to configure
    pub environment: String,
    /// Optional check command
    #[serde(default)]
    pub check: Option<String>,
    /// Optional audit command
    #[serde(default)]
    pub audit: Option<String>,
    /// Optional dependencies
    #[serde(default)]
    pub dependencies: Vec<String>,
    /// Optional description
    #[serde(default)]
    pub description: Option<String>,
    /// Optional homepage URL
    #[serde(default)]
    pub homepage: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct UpdateParam {
    /// Name of the package to update
    pub package: String,
    /// Target environment for environment-scoped fields
    #[serde(default)]
    pub environment: Option<String>,
    /// Update package description
    #[serde(default)]
    pub description: Option<String>,
    /// Update package homepage
    #[serde(default)]
    pub homepage: Option<String>,
    /// Update install command (requires environment)
    #[serde(default)]
    pub install: Option<String>,
    /// Update check command (requires environment). Set to empty string to remove.
    #[serde(default)]
    pub check: Option<String>,
    /// Update audit command (requires environment). Set to empty string to remove.
    #[serde(default)]
    pub audit: Option<String>,
    /// Update dependencies (requires environment)
    #[serde(default)]
    pub dependencies: Option<Vec<String>>,
    /// Update recommended (soft) dependencies (requires environment)
    #[serde(default)]
    pub recommends: Option<Vec<String>>,
    /// Add a new environment configuration
    #[serde(default)]
    pub add_environment: Option<AddEnvironmentParam>,
    /// Remove an environment by name
    #[serde(default)]
    pub remove_environment: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct AddEnvironmentParam {
    /// Environment name
    pub name: String,
    /// Install command
    pub install: String,
    /// Optional check command
    #[serde(default)]
    pub check: Option<String>,
    /// Optional audit command
    #[serde(default)]
    pub audit: Option<String>,
    /// Optional dependencies
    #[serde(default)]
    pub dependencies: Vec<String>,
    /// Optional soft dependencies (installed after package, failures don't cascade)
    #[serde(default)]
    pub recommends: Vec<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct BatchUpdateParam {
    /// List of package updates to apply
    pub updates: Vec<UpdateParam>,
}

#[derive(Deserialize, JsonSchema)]
pub struct RemoveParam {
    /// Name of the package to remove
    pub package: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ListParam {
    /// Show all packages regardless of environment
    #[serde(default)]
    pub all: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct ApplyParam {
    /// Specific package or config name (deploys all if omitted)
    #[serde(default)]
    pub name: Option<String>,
    /// Show what would change without writing files
    #[serde(default)]
    pub dry_run: bool,
    /// Overwrite a conflicting target (one that exists, is untracked by selfie,
    /// and differs from the repo source). Defaults to `false`: conflicts are
    /// skipped and reported with a diff rather than silently overwritten, since
    /// the MCP path has no interactive prompt. Set `true` to force overwrite.
    ///
    /// Does NOT apply to secret-bearing dotfiles — those whose content comes from
    /// a `command` or from a `source` with `vars`. Their conflicts are always
    /// reported and skipped here, whatever this is set to, because their content
    /// is a credential that was never recorded and so could not be recovered
    /// after being overwritten.
    #[serde(default)]
    pub auto_accept: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct TrackDotfileParam {
    /// Name for the new standalone dotfile spec
    pub name: String,
    /// Path to the file to track (the deploy target). Either `~/…` or absolute;
    /// the `~user/…` form is not supported. A path under the home directory is
    /// recorded as `~/…` either way, so the spec means the same file on every
    /// machine.
    pub file: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct PackageTrackDotfileParam {
    /// Name of the existing package to add the dotfile to
    pub package: String,
    /// Path to the file to track (the deploy target). Either `~/…` or absolute;
    /// the `~user/…` form is not supported. A path under the home directory is
    /// recorded as `~/…` either way, so the spec means the same file on every
    /// machine.
    pub file: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct SyncPushParam {
    /// Create a single commit for all changes instead of per-package
    #[serde(default)]
    pub batch: bool,
    /// Override commit message (only meaningful with batch=true)
    #[serde(default)]
    pub message: Option<String>,
    /// Per-package custom commit messages (package name → message).
    /// Packages not in this map use the auto-generated default.
    #[serde(default)]
    pub messages: std::collections::HashMap<String, String>,
    /// Include non-package files in a housekeeping commit
    #[serde(default)]
    pub include_ungrouped: bool,
}

// ─── Spec (definition) tools ───────────────────────────────────────────────

#[tool_router]
impl SelfieServer {
    pub fn new(
        service: ConcreteService,
        config: SelfieConfig,
        ignored_config_keys: Vec<IgnoredKey>,
    ) -> Self {
        let repo = YamlPackageRepository::new(
            RealFileSystem,
            config.package_directory().to_path_buf(),
            SpecOrigin::PackageDirectory,
        );
        // Attached whether or not the directory exists. The dotfile service
        // reports a configured one that is missing on every call that reads it,
        // and a directory created after startup is read on the next call.
        let dotfiles_repo = dotfiles_repository(&config);
        // Login shell: a GUI-launched MCP server does not inherit terminal PATH,
        // and provider commands (`op`, `teller`) live on the user's PATH.
        let runner = ShellCommandRunner::login_shell(config.command_timeout());
        // A fresh token, deliberately: an MCP server has no signal handler and no
        // interactive user to press Ctrl+C, so there is nothing to cancel with.
        // Stated here rather than defaulted inside the service, so this stays a
        // visible property of *this* adapter — `main.rs` says the same about
        // `PackageServiceImpl`. `command_timeout` remains the bound on a provider
        // command that blocks.
        // No `allowing_sudo` call, and no tool parameter that could reach one: an
        // AI assistant has no reason to be driving selfie under sudo, so the
        // refusal here is unconditional.
        let dotfile_service = DotfileServiceImpl::new(
            repo,
            RealFileSystem,
            runner,
            config.clone(),
            CancellationToken::new(),
            SudoPolicy::new(RealPrivilege),
        )
        .with_dotfiles_repository(dotfiles_repo);
        let sync_service = SyncServiceImpl::new(
            GixGitAdapter,
            dotfile_service.clone(),
            config.clone(),
            SudoPolicy::new(RealPrivilege),
        );
        Self {
            service: Arc::new(service),
            dotfile_service: Arc::new(dotfile_service),
            sync_service: Arc::new(sync_service),
            config,
            ignored_config_keys,
        }
    }

    #[tool(
        name = "selfie_spec_create",
        description = "Create a new package spec file. Requires name, environment, and install command. Use selfie_config_get to check the current environment."
    )]
    async fn spec_create(
        &self,
        Parameters(params): Parameters<CreateParam>,
    ) -> Result<CallToolResult, McpError> {
        let mut environments = std::collections::HashMap::new();
        environments.insert(
            params.environment,
            EnvironmentConfig::new(
                params.install,
                params.check,
                params.audit,
                params.dependencies,
                Vec::new(),
            ),
        );

        // Validate package name to prevent path traversal (e.g. "../outside")
        if params.package.contains('/')
            || params.package.contains('\\')
            || params.package.contains("..")
            || params.package.is_empty()
        {
            return Err(McpError::invalid_params(
                format!("Invalid package name: '{}'", params.package),
                None,
            ));
        }

        // Check for namespace conflicts across packages/ and dotfiles/ directories
        let pkg_repo = YamlPackageRepository::new(
            RealFileSystem,
            self.config.package_directory().clone(),
            SpecOrigin::PackageDirectory,
        );
        // A dotfiles directory that is not there holds no names, so the check
        // below is complete without it.
        let dotfiles_repo = dotfiles_repository(&self.config);
        if let Err(e) = selfie::namespace::validate_unique_name(
            &params.package,
            &pkg_repo,
            Some(&dotfiles_repo),
        ) {
            return Err(McpError::invalid_params(
                format!("Namespace conflict: {e}"),
                None,
            ));
        }

        let file_path = self
            .config
            .package_directory()
            .join(format!("{}.yml", params.package));

        let package = Package::new(
            params.package,
            params.homepage,
            params.description,
            Vec::new(),
            None,
            environments,
            file_path,
        );

        let stream = self.service.create(package).await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    #[tool(
        name = "selfie_spec_update",
        description = "Update fields of an existing spec. Environment-scoped fields (install, check, audit, dependencies) require the environment parameter."
    )]
    async fn spec_update(
        &self,
        Parameters(params): Parameters<UpdateParam>,
    ) -> Result<CallToolResult, McpError> {
        // Map check/audit: empty string means "remove", non-empty means "set"
        let check = params
            .check
            .map(|v| if v.is_empty() { None } else { Some(v) });
        let audit = params
            .audit
            .map(|v| if v.is_empty() { None } else { Some(v) });

        let add_environment =
            params
                .add_environment
                .map(|ae| selfie::package::event::AddEnvironment {
                    name: ae.name,
                    install: ae.install,
                    check: ae.check,
                    audit: ae.audit,
                    dependencies: ae.dependencies,
                    recommends: ae.recommends,
                });

        let fields = PackageUpdateFields {
            description: params.description,
            homepage: params.homepage,
            install: params.install,
            check,
            audit,
            dependencies: params.dependencies,
            recommends: params.recommends,
            environment: params.environment,
            add_environment,
            remove_environment: params.remove_environment,
        };

        let stream = self.service.update(&params.package, fields).await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    #[tool(
        name = "selfie_spec_update_batch",
        description = "Update multiple specs in a single call. Each entry has the same fields as selfie_spec_update. Prefer this over calling selfie_spec_update repeatedly."
    )]
    async fn spec_update_batch(
        &self,
        Parameters(params): Parameters<BatchUpdateParam>,
    ) -> Result<CallToolResult, McpError> {
        let mut results: Vec<serde_json::Value> = Vec::new();

        for update in params.updates {
            let package_name = update.package.clone();
            let check = update
                .check
                .map(|v| if v.is_empty() { None } else { Some(v) });
            let audit = update
                .audit
                .map(|v| if v.is_empty() { None } else { Some(v) });
            let add_environment =
                update
                    .add_environment
                    .map(|ae| selfie::package::event::AddEnvironment {
                        name: ae.name,
                        install: ae.install,
                        check: ae.check,
                        audit: ae.audit,
                        dependencies: ae.dependencies,
                        recommends: ae.recommends,
                    });

            let fields = PackageUpdateFields {
                description: update.description,
                homepage: update.homepage,
                install: update.install,
                check,
                audit,
                dependencies: update.dependencies,
                recommends: update.recommends,
                environment: update.environment,
                add_environment,
                remove_environment: update.remove_environment,
            };

            let stream = self.service.update(&package_name, fields).await;
            let result = event_collector::collect_events(stream).await;

            results.push(serde_json::json!({
                "package": package_name,
                "success": result.success,
                "result": result.data["result"],
            }));
        }

        let succeeded = results.iter().filter(|r| r["success"] == true).count();
        let failed = results.len() - succeeded;

        let output = serde_json::json!({
            "total": results.len(),
            "succeeded": succeeded,
            "failed": failed,
            "results": results,
        });

        Ok(CallToolResult::success(vec![ContentBlock::text(
            serde_json::to_string_pretty(&output).unwrap_or_default(),
        )]))
    }

    #[tool(
        name = "selfie_spec_remove",
        description = "Remove a spec file. Warning: this is permanent and may break dependent packages."
    )]
    async fn spec_remove(
        &self,
        Parameters(params): Parameters<RemoveParam>,
    ) -> Result<CallToolResult, McpError> {
        let stream = self.service.remove(&params.package).await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    #[tool(
        name = "selfie_spec_info",
        description = "Get detailed definition info about a specific package including environments, dependencies, and commands. Does not check runtime installation status."
    )]
    async fn spec_info(
        &self,
        Parameters(params): Parameters<PackageNameParam>,
    ) -> Result<CallToolResult, McpError> {
        let stream = self.service.spec_info(&params.package).await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    #[tool(
        name = "selfie_spec_validate",
        description = "Validate a single spec file for correctness. Returns validation issues at three levels: errors, warnings, and informational notices. Each issue carries a `level` field — do not filter on the word 'error' or 'warning' alone, or you will drop the notice reporting that 'selfie apply' executes commands for this package's dotfiles."
    )]
    async fn spec_validate(
        &self,
        Parameters(params): Parameters<PackageNameParam>,
    ) -> Result<CallToolResult, McpError> {
        let stream = self.service.validate(&params.package, None).await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    #[tool(
        name = "selfie_spec_list",
        description = "List all specs for the current environment with name, description, and environments. A spec that could not be loaded is reported as structured fields — `kind` (\"yaml\", \"io\", \"unreadable\", \"irregular_file\" or \"refused\"), `reason`, and `line`/`column` where the kind has a location. Branch on `kind`; `reason` is prose for display, not for matching. Fast — no commands executed."
    )]
    async fn spec_list(&self) -> Result<CallToolResult, McpError> {
        let stream = SpecService::list(&*self.service, false).await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    #[tool(
        name = "selfie_spec_validate_all",
        description = "Validate all spec files for correctness. Returns per-spec validation issues at three levels: errors, warnings, and informational notices. Each issue carries a `level` field — do not filter on the word 'error' or 'warning' alone, or you will drop the notice reporting that 'selfie apply' executes commands for a package's dotfiles. A spec that could not be loaded is reported as structured fields — `kind` (\"yaml\", \"io\", \"unreadable\", \"irregular_file\" or \"refused\"), `reason`, and `line`/`column` where the kind has a location. Branch on `kind`; `reason` is prose for display, not for matching. Fast — no commands executed."
    )]
    async fn spec_validate_all(&self) -> Result<CallToolResult, McpError> {
        let stream = SpecService::validate_all(&*self.service).await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    // ─── Package (runtime) tools ───────────────────────────────────────────

    #[tool(
        name = "selfie_package_check",
        description = "Check if a package is installed in the current environment by running its configured check command"
    )]
    async fn package_check(
        &self,
        Parameters(params): Parameters<PackageNameParam>,
    ) -> Result<CallToolResult, McpError> {
        let stream = self.service.check(&params.package).await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    #[tool(
        name = "selfie_package_audit",
        description = "Audit a package's installation sources and detect conflicts (e.g., installed via both npm and homebrew)"
    )]
    async fn package_audit(
        &self,
        Parameters(params): Parameters<PackageNameParam>,
    ) -> Result<CallToolResult, McpError> {
        let stream = self.service.audit(&params.package).await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    #[tool(
        name = "selfie_package_audit_all",
        description = "Audit all packages for the current environment for installation source conflicts. Returns per-package audit results. A spec that could not be loaded is reported as structured fields — `kind` (\"yaml\", \"io\", \"unreadable\", \"irregular_file\" or \"refused\"), `reason`, and `line`/`column` where the kind has a location. Branch on `kind`; `reason` is prose for display, not for matching."
    )]
    async fn package_audit_all(&self) -> Result<CallToolResult, McpError> {
        let stream = self.service.audit_all().await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    #[tool(
        name = "selfie_package_install",
        description = "Install a package using its configured method for the current environment."
    )]
    async fn package_install(
        &self,
        Parameters(params): Parameters<InstallParam>,
    ) -> Result<CallToolResult, McpError> {
        let options = selfie::package::InstallOptions {
            skip_recommends: params.skip_recommends,
        };
        let stream = self.service.install(&params.package, options).await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    #[tool(
        name = "selfie_package_list",
        description = "List packages with installation status. Set all=true to include packages from other environments. \
A spec that could not be loaded is reported in the summary's invalid_packages, with its kind, reason and location."
    )]
    async fn package_list(
        &self,
        Parameters(params): Parameters<ListParam>,
    ) -> Result<CallToolResult, McpError> {
        let stream = PackageService::list(&*self.service, params.all).await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    #[tool(
        name = "selfie_package_status",
        description = "Check runtime installation status for a specific package in the current environment"
    )]
    async fn package_status(
        &self,
        Parameters(params): Parameters<PackageNameParam>,
    ) -> Result<CallToolResult, McpError> {
        let stream = self.service.status(&params.package).await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    // ─── Config tools ──────────────────────────────────────────────────────

    #[tool(
        name = "selfie_config_get",
        description = "Get the current selfie configuration including environment, package directory, dotfiles directory, state directory, and settings. `dotfiles_directory` and `state_directory` are the paths in effect: the configured one, or the default when none is set (beside the package directory, and `~/.local/state/selfie`). Both are reported whether or not they exist; the dotfile tools say when a configured one is missing. `state_directory` is null when neither a setting nor a home directory gives it a value, or when the configured value is not an absolute path."
    )]
    async fn config_get(&self) -> Result<CallToolResult, McpError> {
        let config_data = serde_json::json!({
            "environment": self.config.environment(),
            "package_directory": self.config.package_directory().display().to_string(),
            // The path in effect rather than the configured one, so a consumer
            // reads the same shape whether or not the default applies. Whether
            // it exists is the dotfile tools' answer to give, and they do.
            "dotfiles_directory": self.config.dotfiles_directory().display().to_string(),
            "state_directory": selfie::fs::state_directory(
                &RealFileSystem,
                self.config.state_directory().map(|p| p.as_path()),
            )
            .ok()
            .map(|directory| directory.display().to_string()),
            "command_timeout_secs": self.config.command_timeout().as_secs(),
            // Always present, empty when the file is clean, so a consumer can
            // read the same shape every time.
            "ignored_config_keys": self
                .ignored_config_keys
                .iter()
                .map(|ignored| serde_json::json!({
                    "key": ignored.key(),
                    "message": ignored.message(),
                    "suggestion": ignored.suggestion(),
                }))
                .collect::<Vec<_>>(),
        });
        Ok(CallToolResult::success(vec![ContentBlock::text(
            serde_json::to_string_pretty(&config_data).unwrap_or_default(),
        )]))
    }

    // ─── Config deploy tools ──────────────────────────────────────────────

    #[tool(
        name = "selfie_apply_dotfiles",
        description = "Deploy dotfiles to their target locations. Omit name to deploy all. A name is matched against package file names, ignoring case, the way selfie_package_install resolves one; a name matching no package, or naming a spec that could not be loaded, comes back as an ERROR result with status 'failure' and nothing deployed, and a `reason` field of \"not_found\", \"maybe_in_unlistable_directory\" or \"not_loaded\"; branch on `reason`, not on `error`. Conflicts (a target that exists, is untracked by selfie, and differs from the repo source — e.g. a second machine with its own edits) are skipped and reported with a diff, never overwritten, unless you pass auto_accept=true. Secret-bearing dotfiles — content from a `command`, or from a `source` with `vars` — are an exception: their conflicts are ALWAYS reported and skipped, auto_accept has no effect on them, and their content is never returned. dry_run=true previews without running any provider command, so it cannot say whether a secret-bearing entry would change. If selfie refuses any entry — an unrecognized key, a target it will not write to or cannot read, a source it cannot read — or, when deploying all, cannot list a dotfiles directory that exists, the call comes back as an ERROR result with status 'refused' and a non-zero `refused` count, even though the rest of the run succeeded; a conflict is reported instead as a conflict and is not a refusal. If `dotfiles_directory` is set and does not exist, a `warning` row says so and the call carries on without standalone dotfiles. A spec that could not be loaded is reported as structured fields — `kind` (\"yaml\", \"io\", \"unreadable\", \"irregular_file\" or \"refused\"), `reason`, and `line`/`column` where the kind has a location. Branch on `kind`; `reason` is prose for display, not for matching. A deploy state file that exists but cannot be read, is empty, or does not parse is refused: the call comes back as an ERROR result with status 'failure' whose message names the file and the remedy, and nothing is deployed; a dry run warns instead and previews against an empty state."
    )]
    async fn selfie_apply_dotfiles(
        &self,
        Parameters(params): Parameters<ApplyParam>,
    ) -> Result<CallToolResult, McpError> {
        let options = ApplyOptions {
            dry_run: params.dry_run,
            auto_accept: params.auto_accept,
            conflict_resolver: None,
        };

        use selfie::dotfile_service::port::DotfileService;
        let stream = if let Some(name) = &params.name {
            self.dotfile_service.apply(name, options).await
        } else {
            self.dotfile_service.apply_all(options).await
        };

        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    #[tool(
        name = "selfie_dotfiles_list",
        description = "List all dotfile mappings with package name, environment (null for shared entries, or the environment name for environment-specific ones), target, and where the content comes from. `kind` is one of \"file\" (a repository file, given in `source`), \"template\" (a repository file in `source` rendered by substituting the named values in `vars`), \"command\" (the whole file is the stdout of `command`), or \"invalid\". For template and command entries only the var names and the command string are returned — never a resolved value, and no command is executed. If `dotfiles_directory` is set and does not exist, a `warning` row says so and the call carries on without standalone dotfiles. A spec this tool could not load is reported as a `spec_skipped` row carrying its `kind`, `path` and `line`/`column`, the same shape every other tool uses. Branch on `kind`; `reason` is prose for display, not for matching. Fast — no commands executed."
    )]
    async fn selfie_dotfiles_list(&self) -> Result<CallToolResult, McpError> {
        use selfie::dotfile_service::port::DotfileService;

        let stream = self.dotfile_service.list().await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    #[tool(
        name = "selfie_dotfiles_drift",
        description = "Check deployed dotfiles for drift between repo sources and targets. Returns per-file drift status. If drift cannot check something — a package apply would refuse whole, a target that exists but cannot be read (reported as a `warning` row with no drift row), or a dotfiles directory that exists and cannot be listed — the call comes back as an ERROR result with status 'refused' and a non-zero `refused` count, even though the rest of the check ran. If `dotfiles_directory` is set and does not exist, a `warning` row says so and the call carries on without standalone dotfiles. A spec that could not be loaded is reported as structured fields — `kind` (\"yaml\", \"io\", \"unreadable\", \"irregular_file\" or \"refused\"), `reason`, and `line`/`column` where the kind has a location. Branch on `kind`; `reason` is prose for display, not for matching. A deploy state file that exists but cannot be read, is empty, or does not parse is reported as a `warning` row and the check carries on as though nothing had been deployed, so every entry then shows as untracked."
    )]
    async fn selfie_dotfiles_drift(&self) -> Result<CallToolResult, McpError> {
        use selfie::dotfile_service::port::DotfileService;
        let stream = self.dotfile_service.check_drift().await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    #[tool(
        name = "selfie_dotfiles_track",
        description = "Track a file as a standalone dotfile. Copies it into the dotfiles directory and creates a YAML spec. Fails, writing nothing, when the dotfiles directory does not exist, or when a deploy state file exists that cannot be read, is empty, or does not parse; the message names the file and the remedy. A spec that cannot be saved also writes nothing: the copy is removed again, or, where that removal also fails, the failure names the file to delete. A deploy state that cannot be written at the end is the one partial outcome — the copy and the spec entry are in place and only the record is missing, and the failure names both files. Call selfie_apply_dotfiles to finish it; tracking again records nothing."
    )]
    async fn selfie_dotfiles_track(
        &self,
        Parameters(params): Parameters<TrackDotfileParam>,
    ) -> Result<CallToolResult, McpError> {
        use selfie::dotfile_service::port::DotfileService;

        // Namespace validation — prevent conflicts with existing packages
        let pkg_repo = YamlPackageRepository::new(
            RealFileSystem,
            self.config.package_directory().to_owned(),
            SpecOrigin::PackageDirectory,
        );
        let dotfiles_repo = dotfiles_repository(&self.config);
        if let Err(e) =
            selfie::namespace::validate_unique_name(&params.name, &pkg_repo, Some(&dotfiles_repo))
        {
            return Err(McpError::invalid_params(
                format!("Namespace conflict: {e}"),
                None,
            ));
        }

        let stream = self
            .dotfile_service
            .track_standalone(&params.name, &params.file)
            .await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    #[tool(
        name = "selfie_package_track_dotfile",
        description = "Add a file to an existing package's dotfiles section. The package must already exist. Fails, writing nothing, when a deploy state file exists that cannot be read, is empty, or does not parse; the message names the file and the remedy. A spec that cannot be saved also writes nothing: the copy is removed again, or, where that removal also fails, the failure names the file to delete. A deploy state that cannot be written at the end is the one partial outcome — the copy and the spec entry are in place and only the record is missing, and the failure names both files. Call selfie_apply_dotfiles to finish it; tracking again records nothing."
    )]
    async fn selfie_package_track_dotfile(
        &self,
        Parameters(params): Parameters<PackageTrackDotfileParam>,
    ) -> Result<CallToolResult, McpError> {
        use selfie::dotfile_service::port::DotfileService;
        let stream = self
            .dotfile_service
            .track_for_package(&params.package, &params.file)
            .await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    // ─── Sync tools ────────────────────────────────────────────────────────

    #[tool(
        name = "selfie_sync_status",
        description = "Get git repository status and dotfile drift summary. Returns uncommitted changes, remote tracking state, and drifted dotfiles. Everything the drift check warned about (`warning` rows) and every spec it could not load (`spec_skipped` rows) is included ahead of the summary, in the order drift reported them; those rows limit what the summary covers. Why an individual target drifted is not included; selfie_dotfiles_drift reports that beside each entry. The `sync_drift_summary` row carries `unloaded_specs`, the count of specs the drift check could not load, and `warned`, the count of warnings it relayed; a non-zero value in either means the summary covers less than the repository."
    )]
    async fn selfie_sync_status(&self) -> Result<CallToolResult, McpError> {
        let stream = self.sync_service.status().await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    #[tool(
        name = "selfie_sync_push",
        description = "Commit and push changes to remote. Creates one commit per changed package by default. Use batch=true for a single commit, or 'messages' for custom per-package messages."
    )]
    async fn selfie_sync_push(
        &self,
        Parameters(params): Parameters<SyncPushParam>,
    ) -> Result<CallToolResult, McpError> {
        let options = PushOptions {
            batch: params.batch,
            message: params.message.clone(),
            auto_accept: true, // MCP never prompts
            include_ungrouped: params.include_ungrouped,
        };

        // Phase 1: Prepare commits
        let prepare_result = match self.sync_service.prepare_push(&options).await {
            Ok(result) => result,
            Err(e) => {
                let data = serde_json::json!({
                    "status": "error",
                    "message": e.to_string(),
                });
                return Ok(CallToolResult::error(vec![ContentBlock::text(
                    serde_json::to_string_pretty(&data).unwrap_or_default(),
                )]));
            }
        };

        if prepare_result.pending_commits.is_empty() && prepare_result.ahead == 0 {
            let data = serde_json::json!({
                "status": "nothing_to_push",
                "message": "Working tree is clean — nothing to push",
                "warnings": prepare_result.warnings,
            });
            return Ok(CallToolResult::success(vec![ContentBlock::text(
                serde_json::to_string_pretty(&data).unwrap_or_default(),
            )]));
        }

        // Apply custom messages from the `messages` parameter
        let confirmed_commits: Vec<ConfirmedCommit> = prepare_result
            .pending_commits
            .into_iter()
            .map(|c| {
                let message = params.messages.get(&c.name).cloned().unwrap_or(c.message);
                ConfirmedCommit {
                    files: c.files,
                    message,
                }
            })
            .collect();

        // Phase 2: Execute commits and push (also pushes existing ahead commits)
        let warnings = prepare_result.warnings;
        let stream = self.sync_service.execute_push(confirmed_commits).await;
        let mut result = event_collector::collect_events(stream).await;

        // Include warnings from prepare phase in the result
        if !warnings.is_empty()
            && let serde_json::Value::Object(ref mut map) = result.data
        {
            map.insert("warnings".to_string(), serde_json::json!(warnings));
        }

        Ok(tool_result(result))
    }

    #[tool(
        name = "selfie_sync_pull",
        description = "Fetch and fast-forward merge from remote. Refuses if working tree has uncommitted changes."
    )]
    async fn selfie_sync_pull(&self) -> Result<CallToolResult, McpError> {
        let stream = self.sync_service.pull().await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }
}

#[tool_handler]
impl ServerHandler for SelfieServer {
    fn get_info(&self) -> ServerInfo {
        let mut capabilities = ServerCapabilities::default();
        capabilities.tools = Some(ToolsCapability::default());
        ServerInfo::new(capabilities)
            .with_server_info(Implementation::new("selfie-mcp", env!("CARGO_PKG_VERSION")))
    }
}

/// Returns the standalone dotfiles repository for `config`'s
/// `dotfiles_directory`. It is built whether or not the directory exists.
fn dotfiles_repository(config: &SelfieConfig) -> YamlPackageRepository<RealFileSystem> {
    YamlPackageRepository::new(
        RealFileSystem,
        config.dotfiles_directory(),
        SpecOrigin::DotfilesDirectory,
    )
}

fn tool_result(result: event_collector::EventCollectorResult) -> CallToolResult {
    let json = serde_json::to_string_pretty(&result.data).unwrap_or_default();
    if result.success {
        CallToolResult::success(vec![ContentBlock::text(json)])
    } else {
        CallToolResult::error(vec![ContentBlock::text(json)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_param_defaults_auto_accept_to_false() {
        // Data-loss guard (selfie-45h): the MCP apply path has no interactive
        // prompt, so an omitted `auto_accept` must deserialize to `false` — a
        // conflicting target (exists, untracked, different content) is then
        // skipped and reported with a diff rather than silently overwritten.
        // Overwriting must require an explicit `auto_accept: true`.
        let params: ApplyParam =
            serde_json::from_str("{}").expect("empty params object should deserialize");
        assert!(
            !params.auto_accept,
            "auto_accept must default to false to prevent silent overwrites of divergent configs"
        );
    }

    // Parses a fixture rather than building one: a *malformed* entry cannot be
    // constructed programmatically — `DotfileEntry::new` only produces valid
    // ones — so testing how a refused entry is reported means parsing YAML, as
    // the repository does.
    fn entry(yaml: &str) -> selfie::package::DotfileEntry {
        selfie::yaml::parse(yaml).expect("fixture must parse")
    }

    #[test]
    fn a_refused_entry_is_reported_as_invalid_with_the_reason() {
        // `content_source()` returns a `Result`, and this is the consumer where a
        // silent drop is least visible: an assistant that never sees the entry
        // cannot tell the user why their dotfile does not deploy. It has to be
        // listed, and the reason has to name the offending var or key rather than
        // reciting every way an entry can be malformed.
        for (yaml, needle) in [
            (
                "source: creds.tpl\ntarget: ~/.creds\nvars:\n  not-a-name: op read x\n",
                "not-a-name",
            ),
            (
                "source: creds.tpl\ntarget: ~/.creds\n_vars:\n  api_key: op read x\n",
                "_vars",
            ),
            (
                "source: a.tpl\ncommand: op read x\ntarget: ~/.creds\n",
                "exactly one of",
            ),
        ] {
            let json =
                crate::event_collector::dotfile_entry_json("creds", None, &entry(yaml), "packages");

            assert_eq!(json["kind"], "invalid", "for {yaml}");
            assert_eq!(json["target"], "~/.creds", "for {yaml}");
            assert!(
                json["error"].as_str().unwrap().contains(needle),
                "the reason must name what is wrong, got: {}",
                json["error"]
            );
        }
    }

    #[test]
    fn a_deployable_entry_is_still_described_by_its_source() {
        // The control: without it the test above could pass on a change that
        // reported every entry as invalid.
        let json = crate::event_collector::dotfile_entry_json(
            "creds",
            Some("macos"),
            &entry("source: creds.tpl\ntarget: ~/.creds\nvars:\n  api_key: op read x\n"),
            "packages",
        );

        assert_eq!(json["kind"], "template");
        assert_eq!(json["source"], "creds.tpl");
        assert_eq!(json["vars"][0], "api_key");
        assert!(json.get("error").is_none());
    }

    // A server over `packages` and the given `dotfiles_directory`, or the
    // sibling default when `None`. Listing runs no command, so the login-shell
    // runner is never used.
    fn server_over(packages: &std::path::Path, dotfiles: Option<&std::path::Path>) -> SelfieServer {
        server_with(packages, dotfiles, None)
    }

    // As `server_over`, with a configured `state_directory` as well.
    fn server_with(
        packages: &std::path::Path,
        dotfiles: Option<&std::path::Path>,
        state: Option<&std::path::Path>,
    ) -> SelfieServer {
        let mut builder = selfie::config::SelfieConfigBuilder::default()
            .environment("test")
            .package_directory(packages);
        if let Some(dotfiles) = dotfiles {
            builder = builder.dotfiles_directory(dotfiles.to_path_buf());
        }
        if let Some(state) = state {
            builder = builder.state_directory(state.to_path_buf());
        }
        let config = builder.build();
        let repo = YamlPackageRepository::new(
            RealFileSystem,
            config.package_directory().to_path_buf(),
            SpecOrigin::PackageDirectory,
        );
        let service = PackageServiceImpl::new(
            repo,
            ShellCommandRunner::login_shell(config.command_timeout()),
            GixGitStatusProvider,
            config.clone(),
            CancellationToken::new(),
        );
        SelfieServer::new(service, config, Vec::new())
    }

    // The JSON a tool returned.
    fn tool_json(result: &CallToolResult) -> serde_json::Value {
        let text = &result.content[0]
            .as_text()
            .expect("tool results are text")
            .text;
        serde_json::from_str(text).unwrap()
    }

    // An assistant that asks where dotfiles live gets an answer. Every dotfile
    // tool resolves paths against this directory, and until it was reported the
    // only way to learn it was to infer it from a path in some other result.
    #[tokio::test]
    async fn config_get_reports_a_configured_dotfiles_directory() {
        let temp = tempfile::TempDir::new().unwrap();
        let packages = temp.path().join("packages");
        std::fs::create_dir_all(&packages).unwrap();
        let dotfiles = temp.path().join("elsewhere");

        let server = server_over(&packages, Some(&dotfiles));
        let json = tool_json(&server.config_get().await.unwrap());

        assert_eq!(
            json["dotfiles_directory"].as_str(),
            Some(dotfiles.display().to_string().as_str()),
            "got: {json}"
        );
    }

    // Unset, the sibling default applies, and the field carries that rather
    // than going absent -- one shape either way.
    #[tokio::test]
    async fn config_get_reports_the_default_dotfiles_directory_when_none_is_set() {
        let temp = tempfile::TempDir::new().unwrap();
        let packages = temp.path().join("packages");
        std::fs::create_dir_all(&packages).unwrap();

        let server = server_over(&packages, None);
        let json = tool_json(&server.config_get().await.unwrap());

        assert_eq!(
            json["dotfiles_directory"].as_str(),
            Some(temp.path().join("dotfiles").display().to_string().as_str()),
            "got: {json}"
        );
    }

    // A configured state directory is echoed as given; it is where the deploy
    // state the apply and drift tools read lives.
    #[tokio::test]
    async fn config_get_reports_a_configured_state_directory() {
        let temp = tempfile::TempDir::new().unwrap();
        let packages = temp.path().join("packages");
        std::fs::create_dir_all(&packages).unwrap();
        let state = temp.path().join("state");

        let server = server_with(&packages, None, Some(&state));
        let json = tool_json(&server.config_get().await.unwrap());

        assert_eq!(
            json["state_directory"].as_str(),
            Some(state.display().to_string().as_str()),
            "got: {json}"
        );
    }

    // Unset, the field carries the default under the home directory rather
    // than going absent, so a consumer reads one shape either way.
    #[tokio::test]
    async fn config_get_reports_the_default_state_directory_when_none_is_set() {
        let temp = tempfile::TempDir::new().unwrap();
        let packages = temp.path().join("packages");
        std::fs::create_dir_all(&packages).unwrap();

        let server = server_with(&packages, None, None);
        let json = tool_json(&server.config_get().await.unwrap());

        let reported = json["state_directory"]
            .as_str()
            .unwrap_or_else(|| panic!("the default must be reported, got: {json}"));
        assert!(
            std::path::Path::new(reported).is_absolute()
                && reported.ends_with("/.local/state/selfie"),
            "the default is the XDG state home under the home directory, got: {reported}"
        );
    }

    // A configured value with no usable resolution is null rather than echoed:
    // a relative path is refused by every command that would read the state,
    // and reporting it as the directory in effect would say otherwise.
    #[tokio::test]
    async fn config_get_reports_null_for_an_unresolvable_state_directory() {
        let temp = tempfile::TempDir::new().unwrap();
        let packages = temp.path().join("packages");
        std::fs::create_dir_all(&packages).unwrap();

        let server = server_with(
            &packages,
            None,
            Some(std::path::Path::new("relative/state")),
        );
        let json = tool_json(&server.config_get().await.unwrap());

        assert!(
            json["state_directory"].is_null(),
            "an unresolvable state directory must be null, got: {json}"
        );
        assert!(
            json.get("state_directory").is_some(),
            "the field must be present even when null, got: {json}"
        );
    }

    // An assistant reading the list result has no stderr to look at, so the
    // missing directory has to be a row in the result itself.
    #[tokio::test]
    async fn the_list_tool_reports_a_configured_dotfiles_directory_that_is_missing() {
        let temp = tempfile::TempDir::new().unwrap();
        let packages = temp.path().join("packages");
        std::fs::create_dir_all(&packages).unwrap();
        let dotfiles = temp.path().join("missing-dotfiles");

        let server = server_over(&packages, Some(&dotfiles));
        let json = tool_json(&server.selfie_dotfiles_list().await.unwrap());

        let warnings: Vec<&str> = json["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["type"] == "warning")
            .filter_map(|row| row["message"].as_str())
            .collect();
        assert_eq!(warnings.len(), 1, "got: {json}");
        assert!(
            warnings[0].starts_with("Dotfiles directory does not exist: ")
                && warnings[0].contains(&dotfiles.display().to_string()),
            "got: {}",
            warnings[0]
        );
    }

    // The server reads the dotfiles directory on each call, not once at startup.
    #[tokio::test]
    async fn the_list_tool_reads_a_dotfiles_directory_created_after_startup() {
        let temp = tempfile::TempDir::new().unwrap();
        let packages = temp.path().join("packages");
        std::fs::create_dir_all(&packages).unwrap();

        let server = server_over(&packages, None);
        let dotfiles = temp.path().join("dotfiles");
        std::fs::create_dir_all(&dotfiles).unwrap();
        std::fs::write(
            dotfiles.join("vim.yml"),
            "name: vim\ndotfiles:\n  - source: vim/vimrc\n    target: ~/.vimrc\n",
        )
        .unwrap();

        let json = tool_json(&server.selfie_dotfiles_list().await.unwrap());

        let list = json["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["type"] == "dotfile_list")
            .unwrap_or_else(|| panic!("no dotfile_list row in {json}"));
        assert!(
            list["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["package"] == "vim"),
            "got: {list}"
        );
    }
}
