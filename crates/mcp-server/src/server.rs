use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{
    ErrorData as McpError,
    handler::server::ServerHandler,
    model::{
        CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo, ToolsCapability,
    },
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use selfie::{
    commands::ShellCommandRunner,
    config::SelfieConfig,
    dotfile_service::{port::ApplyOptions, service::DotfileServiceImpl},
    fs::RealFileSystem,
    git::GixGitAdapter,
    package::{
        EnvironmentConfig, Package, PackageService, SpecService, event::PackageUpdateFields,
        git_adapter::GixGitStatusProvider, repository::yaml::YamlPackageRepository,
        service::PackageServiceImpl,
    },
    sync_service::{ConfirmedCommit, PushOptions, SyncService, service::SyncServiceImpl},
};
use serde::Deserialize;

use crate::event_collector;

type ConcreteService = PackageServiceImpl<
    YamlPackageRepository<RealFileSystem>,
    ShellCommandRunner,
    GixGitStatusProvider,
>;

type ConcreteDotfileService =
    DotfileServiceImpl<YamlPackageRepository<RealFileSystem>, RealFileSystem, ShellCommandRunner>;

type ConcreteSyncService = SyncServiceImpl<GixGitAdapter, ConcreteDotfileService>;

#[derive(Clone)]
pub struct SelfieServer {
    service: Arc<ConcreteService>,
    dotfile_service: Arc<ConcreteDotfileService>,
    sync_service: Arc<ConcreteSyncService>,
    config: SelfieConfig,
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
    /// Absolute path to the file to track (the deploy target)
    pub file: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct PackageTrackDotfileParam {
    /// Name of the existing package to add the dotfile to
    pub package: String,
    /// Absolute path to the file to track (the deploy target)
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
    pub fn new(service: ConcreteService, config: SelfieConfig) -> Self {
        let repo =
            YamlPackageRepository::new(RealFileSystem, config.package_directory().to_path_buf());
        // Login shell: a GUI-launched MCP server does not inherit terminal PATH,
        // and provider commands (`op`, `teller`) live on the user's PATH.
        let runner = ShellCommandRunner::login_shell(config.command_timeout());
        let mut dotfile_service =
            DotfileServiceImpl::new(repo, RealFileSystem, runner, config.clone());

        // Add standalone dotfiles repository if the directory exists
        let dotfiles_dir = config.dotfiles_directory();
        if dotfiles_dir.is_dir() {
            let dotfiles_repo = YamlPackageRepository::new(RealFileSystem, dotfiles_dir);
            dotfile_service = dotfile_service.with_dotfiles_repository(dotfiles_repo);
        }
        let sync_service =
            SyncServiceImpl::new(GixGitAdapter, dotfile_service.clone(), config.clone());
        Self {
            service: Arc::new(service),
            dotfile_service: Arc::new(dotfile_service),
            sync_service: Arc::new(sync_service),
            config,
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
        let pkg_repo =
            YamlPackageRepository::new(RealFileSystem, self.config.package_directory().clone());
        let dotfiles_dir = self.config.dotfiles_directory();
        let dotfiles_repo = if dotfiles_dir.is_dir() {
            Some(YamlPackageRepository::new(RealFileSystem, dotfiles_dir))
        } else {
            None
        };
        if let Err(e) = selfie::namespace::validate_unique_name(
            &params.package,
            &pkg_repo,
            dotfiles_repo.as_ref(),
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

        Ok(CallToolResult::success(vec![Content::text(
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
        description = "Validate a single spec file for correctness. Returns validation issues (errors and warnings)."
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
        description = "List all specs for the current environment with name, description, and environments. Fast — no commands executed."
    )]
    async fn spec_list(&self) -> Result<CallToolResult, McpError> {
        let stream = SpecService::list(&*self.service, false).await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    #[tool(
        name = "selfie_spec_validate_all",
        description = "Validate all spec files for correctness. Returns per-spec validation issues (errors and warnings). Fast — no commands executed."
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
        description = "Audit all packages for the current environment for installation source conflicts. Returns per-package audit results."
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
        description = "List packages with installation status. Set all=true to include packages from other environments."
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
        description = "Get the current selfie configuration including environment, package directory, and settings"
    )]
    async fn config_get(&self) -> Result<CallToolResult, McpError> {
        let config_data = serde_json::json!({
            "environment": self.config.environment(),
            "package_directory": self.config.package_directory().display().to_string(),
            "command_timeout_secs": self.config.command_timeout().as_secs(),
        });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&config_data).unwrap_or_default(),
        )]))
    }

    // ─── Config deploy tools ──────────────────────────────────────────────

    #[tool(
        name = "selfie_apply_dotfiles",
        description = "Deploy dotfiles to their target locations. Omit name to deploy all. Conflicts (a target that exists, is untracked by selfie, and differs from the repo source — e.g. a second machine with its own edits) are skipped and reported with a diff, never overwritten, unless you pass auto_accept=true. Secret-bearing dotfiles — content from a `command`, or from a `source` with `vars` — are an exception: their conflicts are ALWAYS reported and skipped, auto_accept has no effect on them, and their content is never returned. dry_run=true previews without running any provider command, so it cannot say whether a secret-bearing entry would change."
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
        description = "List all dotfile mappings with package name, environment (null for shared entries, or the environment name for environment-specific ones), target, and where the content comes from. `kind` is one of \"file\" (a repository file, given in `source`), \"template\" (a repository file in `source` rendered by substituting the named values in `vars`), \"command\" (the whole file is the stdout of `command`), or \"invalid\". For template and command entries only the var names and the command string are returned — never a resolved value, and no command is executed. Fast — no commands executed."
    )]
    async fn selfie_dotfiles_list(&self) -> Result<CallToolResult, McpError> {
        use selfie::package::port::PackageRepository;

        let repo = YamlPackageRepository::new(
            RealFileSystem,
            self.config.package_directory().to_path_buf(),
        );
        let mut entries: Vec<serde_json::Value> = Vec::new();

        if let Ok(output) = repo.list_packages() {
            for pkg in output
                .valid_packages()
                .filter(|p| !p.dotfiles_with_scope().is_empty())
            {
                for (scope, entry) in pkg.dotfiles_with_scope() {
                    entries.push(dotfile_entry_json(pkg.name(), scope, entry, "packages"));
                }
            }
        }

        let dotfiles_dir = self.config.dotfiles_directory();
        if dotfiles_dir.is_dir() {
            let dotfiles_repo = YamlPackageRepository::new(RealFileSystem, dotfiles_dir);
            if let Ok(output) = dotfiles_repo.list_packages() {
                for pkg in output
                    .valid_packages()
                    .filter(|p| !p.dotfiles_with_scope().is_empty())
                {
                    for (scope, entry) in pkg.dotfiles_with_scope() {
                        entries.push(dotfile_entry_json(pkg.name(), scope, entry, "dotfiles"));
                    }
                }
            }
        }

        let data = serde_json::json!({
            "status": "success",
            "total": entries.len(),
            "dotfiles": entries,
        });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&data).unwrap_or_default(),
        )]))
    }

    #[tool(
        name = "selfie_dotfiles_drift",
        description = "Check deployed dotfiles for drift between repo sources and targets. Returns per-file drift status."
    )]
    async fn selfie_dotfiles_drift(&self) -> Result<CallToolResult, McpError> {
        use selfie::dotfile_service::port::DotfileService;
        let stream = self.dotfile_service.check_drift().await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    #[tool(
        name = "selfie_dotfiles_track",
        description = "Track a file as a standalone dotfile. Copies it into the dotfiles directory and creates a YAML spec."
    )]
    async fn selfie_dotfiles_track(
        &self,
        Parameters(params): Parameters<TrackDotfileParam>,
    ) -> Result<CallToolResult, McpError> {
        use selfie::dotfile_service::port::DotfileService;

        // Namespace validation — prevent conflicts with existing packages
        let pkg_repo =
            YamlPackageRepository::new(RealFileSystem, self.config.package_directory().to_owned());
        let dotfiles_dir = self.config.dotfiles_directory().to_owned();
        let dotfiles_repo = if dotfiles_dir.is_dir() {
            Some(YamlPackageRepository::new(RealFileSystem, dotfiles_dir))
        } else {
            None
        };
        if let Err(e) =
            selfie::namespace::validate_unique_name(&params.name, &pkg_repo, dotfiles_repo.as_ref())
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
        description = "Add a file to an existing package's dotfiles section. The package must already exist."
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
        description = "Get git repository status and dotfile drift summary. Returns uncommitted changes, remote tracking state, and drifted dotfiles."
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
                return Ok(CallToolResult::error(vec![Content::text(
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
            return Ok(CallToolResult::success(vec![Content::text(
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

fn tool_result(result: event_collector::EventCollectorResult) -> CallToolResult {
    let json = serde_json::to_string_pretty(&result.data).unwrap_or_default();
    if result.success {
        CallToolResult::success(vec![Content::text(json)])
    } else {
        CallToolResult::error(vec![Content::text(json)])
    }
}

/// Render one dotfile entry as JSON for `selfie_dotfiles_list`.
///
/// Reports where content comes from without producing any of it: var names and
/// the command string come from the package file and are references, not values.
/// Nothing here runs a command or renders a template, so enumeration cannot leak
/// a secret or trigger an authentication prompt.
fn dotfile_entry_json(
    package: &str,
    scope: Option<&str>,
    entry: &selfie::package::DotfileEntry,
    origin: &str,
) -> serde_json::Value {
    use selfie::package::ContentSource;

    let mut value = serde_json::json!({
        "package": package,
        "environment": scope,
        "target": entry.target(),
        "origin": origin,
    });
    let map = value.as_object_mut().expect("constructed as an object");

    match entry.content_source() {
        ContentSource::RepoFile(source) => {
            map.insert("kind".into(), "file".into());
            map.insert("source".into(), source.into());
        }
        ContentSource::Template { source, vars } => {
            map.insert("kind".into(), "template".into());
            map.insert("source".into(), source.into());
            map.insert(
                "vars".into(),
                vars.keys().map(String::as_str).collect::<Vec<_>>().into(),
            );
        }
        ContentSource::Provider(command) => {
            map.insert("kind".into(), "command".into());
            map.insert("command".into(), command.into());
        }
        ContentSource::Invalid => {
            map.insert("kind".into(), "invalid".into());
            map.insert(
                "error".into(),
                selfie::package::INVALID_CONTENT_SOURCE.into(),
            );
        }
    }

    value
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
}
