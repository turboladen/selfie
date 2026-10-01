use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{
    ErrorData as McpError,
    handler::server::ServerHandler,
    model::{
        CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig,
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
            dotfiles_repo,
            RealFileSystem,
            runner,
            config.clone(),
            CancellationToken::new(),
            SudoPolicy::new(RealPrivilege),
        );
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
        description = "Create a new package spec file. Requires name, environment, and install command. Use selfie_config_get to check the current environment. The spec is validated as selfie_spec_validate would before it is written: with errors nothing is written, the call fails, and the result carries every issue under issues; warnings, such as an environment other than the current one, are reported as a validation_result entry and the spec is still written. When the package or dotfiles directory cannot be read, or something other than a directory is at the package directory's path, the call is refused rather than reported as invalid params: the result carries status 'refused' with a reason and the directory's path under package_directory or dotfiles_directory. A package directory with nothing at its path is not refused; the first spec creates it. The name may be free and nothing could check it, so retrying with another name fails the same way."
    )]
    async fn spec_create(
        &self,
        Parameters(params): Parameters<CreateParam>,
    ) -> Result<CallToolResult, McpError> {
        let mut environments = selfie::package::Environments::new();
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

        // Check for namespace conflicts across packages/ and dotfiles/ directories
        let pkg_repo = YamlPackageRepository::new(
            RealFileSystem,
            self.config.package_directory().clone(),
            SpecOrigin::PackageDirectory,
        );
        // A dotfiles directory that is genuinely not there holds no names. One that
        // will not read is a different answer, and the check refuses on it rather
        // than reporting the name free.
        let dotfiles_repo = dotfiles_repository(&self.config);
        if let Err(e) = selfie::namespace::validate_unique_name(
            &params.package,
            &pkg_repo,
            Some(&dotfiles_repo),
        ) {
            return namespace_refusal(e, NameCheck::SpecCreate);
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
        description = "Get detailed definition info about a specific package including environments, dependencies, and commands. Does not check runtime installation status. `dotfiles` lists every dotfile entry with its content source (`kind` file, template with var names, command, or invalid, with the reason in `error`), and `apply_commands` counts the commands `selfie apply` would run in the current environment to produce them; nothing is run to report either. When 'selfie apply' would refuse the spec in the current environment, `refusal` carries the reason, `environments` and `dotfiles` are empty because the file cannot be trusted to say what it declares, and `apply_commands` is 0 because apply would run nothing; the call still succeeds. `refusal_elsewhere` carries the reason apply would refuse the spec in another environment, when it would there and not here."
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
        description = "Validate a single spec file for correctness. The package directory is searched first, then the dotfiles directory for a standalone dotfile spec. Returns validation issues at three levels: errors, warnings, and informational notices. Each issue carries a `level` field — do not filter on the word 'error' or 'warning' alone, or you will drop the notice reporting that 'selfie apply' executes commands for this package's dotfiles. A spec with warnings is a successful call with status 'found'; a notice alone leaves it 'success'. A spec with errors is an error result with status 'failed', whose message counts its errors and warnings; its issues are still in the data rows. Every result carries `outcome`: \"clean\", \"found\" or \"failed\", or \"cancelled\" for a cancelled call."
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
        description = "List all specs for the current environment with name, description, and environments. A spec that loaded but that 'selfie apply' would refuse in the current environment, such as one whose `environments:` a misspelled or anchor-named key shadows, is left out of the specs and listed in the summary's `refused` array with `package`, `path` and `reason`. A spec that could not be loaded is reported as structured fields — `kind` (\"yaml\", \"io\", \"unreadable\", \"irregular_file\", \"refused\" or \"invalid_name\"), `reason`, and `line`/`column` where the kind has a location. Branch on `kind`; `reason` is prose for display, not for matching. Fast — no commands executed."
    )]
    async fn spec_list(&self) -> Result<CallToolResult, McpError> {
        let stream = SpecService::list(&*self.service, false).await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    #[tool(
        name = "selfie_spec_validate_all",
        description = "Validate all spec files for correctness: the package specs and the standalone dotfile specs in the dotfiles directory, read as 'selfie apply' reads them. A name several files claim, or a dotfiles directory that cannot be listed, fails the run with a warning naming it. Returns per-spec validation issues at three levels: errors, warnings, and informational notices. Each issue carries a `level` field — do not filter on the word 'error' or 'warning' alone, or you will drop the notice reporting that 'selfie apply' executes commands for a package's dotfiles. A spec that could not be loaded is reported as structured fields — `kind` (\"yaml\", \"io\", \"unreadable\", \"irregular_file\", \"refused\" or \"invalid_name\"), `reason`, and `line`/`column` where the kind has a location. Branch on `kind`; `reason` is prose for display, not for matching. A run whose specs have warnings but no errors is a successful call with status 'found'. A run with a spec that has errors, a spec file that could not be loaded, or a name several files claim is an error result with status 'failed', whose message counts each. Fast — no commands executed."
    )]
    async fn spec_validate_all(&self) -> Result<CallToolResult, McpError> {
        let stream = SpecService::validate_all(&*self.service).await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    // ─── Package (runtime) tools ───────────────────────────────────────────

    #[tool(
        name = "selfie_package_check",
        description = "Check if a package is installed in the current environment by running its configured check command. A check command that exits non-zero for any reason, a kill included, means the package is not installed: a successful call with status 'found'. No check command, selfie's own timeout, or a command that could not be started comes back as an ERROR result. Every result carries `outcome`: \"clean\", \"found\" or \"failed\", or \"cancelled\" for a cancelled call."
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
        description = "Audit a package's installation sources and detect conflicts (e.g., installed via both npm and homebrew). A conflict, or a package nothing provides, is a successful call with status 'found'. An audit command that fails, or a package with no audit command, comes back as an ERROR result with status 'failed'. Every result carries `outcome`: \"clean\", \"found\" or \"failed\", or \"cancelled\" for a cancelled call."
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
        description = "Audit all packages for the current environment for installation source conflicts. Returns per-package audit results. A package with no audit command is skipped. A conflict, or a package nothing provides, anywhere in the run is a successful call with status 'found'. An audit that could not run comes back as an ERROR result with status 'failed', and a spec left out (one that could not be loaded, or that selfie will not read) as an ERROR result with status 'refused' and a `refused` count. Every result carries `outcome`: \"clean\", \"found\" or \"failed\", or \"cancelled\" for a cancelled call. A spec that could not be loaded is reported as structured fields — `kind` (\"yaml\", \"io\", \"unreadable\", \"irregular_file\", \"refused\" or \"invalid_name\"), `reason`, and `line`/`column` where the kind has a location. Branch on `kind`; `reason` is prose for display, not for matching."
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
A spec that could not be loaded is reported in the summary's invalid_packages, with its kind, reason and location. \
A spec that loaded but that selfie will not read is listed in the summary's `refused` array, with `package`, `path` and `reason`, \
and is not counted in total_packages. Without all=true, a spec is refused when 'selfie apply' would refuse it in the current environment; \
with all=true, when a key in any environment cannot be trusted. A result that completed carries `steps`, `{completed, total}`, the operation's step count; no message carries a count, and a result with status 'failure' carries none."
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
        description = "Check runtime installation status for a specific package in the current environment. Fails, naming the reason, when selfie will not read the package's spec. A dependency whose spec selfie will not read has an unknown status carrying that reason."
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
        description = "Get the current selfie configuration including environment, package directory, dotfiles directory, state directory, and settings. `dotfiles_directory` and `state_directory` are the paths in effect: the configured one, or the default when none is set (beside the package directory, and `~/.local/state/selfie`). Both are reported whether or not a directory is at them; the dotfile tools say what is at a configured one when it is not a directory. `state_directory` is selfie's own and is created on the first write that needs it, whether configured or defaulted. `state_directory` is null when neither a setting nor a home directory gives it a value, or when the configured value is not an absolute path."
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
        description = "Deploy dotfiles to their target locations. Omit name to deploy all. A name is matched against package file names, ignoring case, the way selfie_package_install resolves one; a name matching no package, or naming a spec that could not be loaded, comes back as an ERROR result with status 'failure' and nothing deployed, and a `reason` field of \"not_found\", \"maybe_in_unlistable_directory\", \"maybe_in_uncheckable_directory\", \"not_loaded\" or \"ambiguous\" (several spec files, such as bat.yml and bat.yaml, claim the name, so none is used, whatever they declare; a `conflicting_paths` field lists them); branch on `reason`, not on `error`. The two directory reasons differ in what is known: \"unlistable\" means a directory is there and its entries could not be read, \"uncheckable\" means the path could not be classified at all, so whether a directory is there is unknown. Conflicts (a target that exists, is untracked by selfie, and differs from the repo source — e.g. a second machine with its own edits) are skipped and reported with a diff, never overwritten, unless you pass auto_accept=true. Secret-bearing dotfiles — content from a `command`, or from a `source` with `vars` — are an exception: their conflicts are ALWAYS reported and skipped, auto_accept has no effect on them, and their content is never returned. dry_run=true previews without running any provider command, so it cannot say whether a secret-bearing entry would change. If selfie refuses any entry — an unrecognized key, two or more entries of one package that deploy to the same target in this environment (every one of them is refused and none is written), a target it will not write to or cannot read (a repository-file entry's symlinked target is always refused, even one whose content already matches; a secret-bearing entry's link is replaced instead, and reported in a `warning` row), a source it cannot read — or, when deploying all, cannot read a dotfiles directory that is there or cannot classify the path at all, or finds several spec files in one directory claiming one name where one failed to parse or declares dotfiles for this environment (none of them deploys) or a spec file it could not load, the call comes back as an ERROR result with status 'refused' and a non-zero `refused` count, even though the rest of the run carried on; a conflict is reported instead as a conflict and is not a refusal. Once a provider command fails, later entries running the same program (the first word of a command or of a template binding, after any leading NAME=value assignments) are refused without running, each in a `warning` row reading \"an earlier `<program>` command failed; no command was run\": they share one cause, such as a locked vault, and did not run, so fix that one failure rather than each entry. Entries running another program still run. If `dotfiles_directory` is set and no directory is at that path, a `warning` row says what is there instead — nothing, a file, a symlink whose destination is gone, or a path running through a non-directory — and the call carries on without standalone dotfiles. A spec that could not be loaded is reported as structured fields — `kind` (\"yaml\", \"io\", \"unreadable\", \"irregular_file\", \"refused\" or \"invalid_name\"), `reason`, and `line`/`column` where the kind has a location. Branch on `kind`; `reason` is prose for display, not for matching. A deploy state file that exists but cannot be read, is empty, or does not parse is refused: the call comes back as an ERROR result with status 'failure' whose message names the file and the remedy, and nothing is deployed; a dry run warns instead and previews against an empty state. A file selfie deployed that no entry deploys to any more in this environment (its target changed, or its entry or package was removed) is reported as a `dotfile_orphaned` row with `source`, `target` and `package` (null when the record does not name one) and counted in the result's `orphan_count`; selfie never deletes it, and it is not an error. With a name, only that package's orphans are reported. Once such a file is gone, a non-dry run drops selfie's record of it. The same file under another spelling (a symlinked directory, or a case-only change on a case-insensitive volume) is not an orphan. When selfie cannot see every entry (a spec it could not load or use, a package it refused, a dotfiles directory it could not read or that is missing, no home directory, or no package deploying anything in this environment), orphans are not judged and a `warning` row names the reason, so an `orphan_count` of 0 then means unjudged, not none. A result that completed carries `steps`, `{completed, total}`, the operation's step count; no message carries a count, and a result with status 'failure' carries none. A dotfile row's `source` is the file's full path, with a template's var names, or the command; `base` (\"packages\" or \"dotfiles\") and `relative_path` give the file relative to the directory it was read from, and `vars` lists a template's var names. An orphan's full path joins its recorded path to the directory configured now, so after `package_directory` or `dotfiles_directory` changes it names the new one; an orphan whose record names no base directory has a null `source` and gives `recorded_source`, the spelling the record holds."
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
        description = "List all dotfile mappings with package name, environment (null for shared entries, or the environment name for environment-specific ones), target, and where the content comes from. `kind` is one of \"file\" (a repository file, given in `source`), \"template\" (a repository file in `source` rendered by substituting the named values in `vars`), \"command\" (the whole file is the stdout of `command`), or \"invalid\". For template and command entries only the var names and the command string are returned — never a resolved value, and no command is executed. If `dotfiles_directory` is set and no directory is at that path, a `warning` row says what is there instead — nothing, a file, a symlink whose destination is gone, or a path running through a non-directory — and the call carries on without standalone dotfiles. A spec this tool could not load is reported as a `spec_skipped` row carrying its `kind`, `path` and `line`/`column`, the same shape every other tool uses. Branch on `kind`; `reason` is prose for display, not for matching. Fast — no commands executed."
    )]
    async fn selfie_dotfiles_list(&self) -> Result<CallToolResult, McpError> {
        use selfie::dotfile_service::port::DotfileService;

        let stream = self.dotfile_service.list().await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    #[tool(
        name = "selfie_dotfiles_drift",
        description = "Check deployed dotfiles for drift between repo sources and targets. Returns per-file drift status. A check that found drift or an orphan, or that warned it could not judge a recorded target for orphans, is a successful call with status 'found' and `outcome` \"found\", and `unjudged_count` counts the targets it could not judge. When no package deploys anything in this environment there is nothing to judge: the warning still names the recorded targets, and the status stays 'success'. A check that found none of these has status 'success'. If drift refuses to check something — a spec it could not load (reported as a `spec_skipped` row), a name several spec files in one directory claim where one of them failed to parse or has dotfiles for this environment (a name claimed only by install-only files is left to `selfie_package_install`, and a `dotfiles/` file under a name `packages/` claims only warns), a package apply would refuse whole, an entry apply would refuse, a source that escapes the package directory or cannot be read, a target that is a symlink (whether or not its content matches), a fifo, socket or device node, a directory, or that cannot be read (each reported as a `warning` row with no drift row, and left out of the total), or a dotfiles directory that cannot be read or cannot be classified — the call comes back as an ERROR result with status 'refused' and a non-zero `refused` count, even though the rest of the check ran. A secret-bearing entry (a `command`, or a `source` with `vars`) is reported as a `dotfile_skipped` row and counted in the result's `unverified_count` field; it is unverifiable by design, is not a refusal, and does not make the result an error. If `dotfiles_directory` is set and no directory is at that path, a `warning` row says what is there instead — nothing, a file, a symlink whose destination is gone, or a path running through a non-directory — and the call carries on without standalone dotfiles. A spec that could not be loaded is reported as structured fields — `kind` (\"yaml\", \"io\", \"unreadable\", \"irregular_file\", \"refused\" or \"invalid_name\"), `reason`, and `line`/`column` where the kind has a location. Branch on `kind`; `reason` is prose for display, not for matching. A deploy state file that exists but cannot be read, is empty, or does not parse is reported as a `warning` row and the check carries on as though nothing had been deployed, so every entry then shows as untracked. A file selfie deployed that no entry deploys to any more in this environment is reported as a `dotfile_orphaned` row with `source`, `target` and `package` (null when the record does not name one) and counted in the result's `orphan_count`; it is neither drift nor a refusal, and selfie never deletes it. The same file under another spelling (a symlinked directory, or a case-only change on a case-insensitive volume) is not an orphan. When selfie cannot see every entry (a spec it could not load or use, a package it refused, a dotfiles directory it could not read or that is missing, no home directory, or no package deploying anything in this environment), orphans are not judged and a `warning` row names the reason, so an `orphan_count` of 0 then means unjudged, not none. A result that completed carries `steps`, `{completed, total}`, the operation's step count; no message carries a count, and a result with status 'failure' carries none. A dotfile row's `source` is the file's full path, with a template's var names, or the command; `base` (\"packages\" or \"dotfiles\") and `relative_path` give the file relative to the directory it was read from, and `vars` lists a template's var names. An orphan's full path joins its recorded path to the directory configured now, so after `package_directory` or `dotfiles_directory` changes it names the new one; an orphan whose record names no base directory has a null `source` and gives `recorded_source`, the spelling the record holds."
    )]
    async fn selfie_dotfiles_drift(&self) -> Result<CallToolResult, McpError> {
        use selfie::dotfile_service::port::DotfileService;
        let stream = self.dotfile_service.check_drift().await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    #[tool(
        name = "selfie_dotfiles_track",
        description = "Track a file as a standalone dotfile. Copies it into the dotfiles directory and creates a YAML spec. Fails, writing nothing, when no readable directory is at the dotfiles directory path — nothing there, something that is not a directory, a symlink whose destination is gone, or a directory whose entries cannot be read, since a directory selfie cannot read may already hold the name — or when the package directory cannot be read or something other than a directory is at its path (status 'refused' with package_directory), or when a deploy state file exists that cannot be read, is empty, or does not parse; the message names the file and the remedy. A spec that cannot be saved also writes nothing: the copy is removed again, or, where that removal also fails, the failure names the file to delete. A deploy state that cannot be written at the end is the one partial outcome — the copy and the spec entry are in place and only the record is missing, and the failure names both files. Call selfie_apply_dotfiles to finish it; tracking again records nothing."
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
            return namespace_refusal(e, NameCheck::DotfilesTrack);
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
        description = "Get git repository status and dotfile drift summary. Returns uncommitted changes, remote tracking state, and drifted dotfiles. Everything the drift check warned about (`warning` rows) and every spec it could not load (`spec_skipped` rows) is included ahead of the summary, in the order drift reported them; those rows limit what the summary covers. Which targets drifted is included, but not the drift type of each; selfie_dotfiles_drift reports that per entry. The `sync_drift_summary` row carries `refused_count`, what the drift check refused to check, including each spec it could not load, and `warned`, the count of warnings that named work the check could not complete, including the \"Drift check failed\" warning status sends when the check fails outright; a non-zero value in either means the summary covers less than the repository. It also carries `unverified_count`, the secret-bearing entries drift reported without verifying, since checking one would run its commands; they are unverifiable by design and do not make the summary incomplete. `total_deployed` counts only entries that were compared. `orphan_count` counts orphaned targets, which are neither drift nor a gap in the check, `unjudged_count` counts recorded targets the check could not judge for orphans, and `drift_outcome` is the drift check's own verdict: \"clean\", \"found\" (drift, an orphan, or an unjudged target found) or \"failed\" (it refused something, failed, or reported no result)."
    )]
    async fn selfie_sync_status(&self) -> Result<CallToolResult, McpError> {
        let stream = self.sync_service.status().await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    #[tool(
        name = "selfie_sync_push",
        description = "Currently disabled for anything that would create a commit: with changes to commit it refuses before staging anything, because its commit could record every tracked file as deleted; commit with git instead. With nothing new to commit it still pushes commits that already exist. The parameters are accepted but have no effect while disabled."
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
    fn get_info(&self) -> ServerConfig {
        let mut capabilities = ServerCapabilities::default();
        capabilities.tools = Some(ToolsCapability::default());
        ServerConfig::new(capabilities)
            .with_server_info(Implementation::new("selfie-mcp", env!("CARGO_PKG_VERSION")))
    }
}

/// The MCP answer to a refused name check.
///
/// A conflict is about the name the caller sent, so it is `invalid_params`: the
/// fact, then what the calling tool can do about it. A package or dotfiles
/// directory selfie could not read is not about the name at all, so it comes back
/// as a refusal in the shape the apply and drift tools use: `status` and `reason`,
/// plus `package_directory` or `dotfiles_directory` naming the directory.
// Reporting an unreadable directory as invalid input is what sends an agent round a
// loop of names that all fail identically.
fn namespace_refusal(
    error: selfie::namespace::NamespaceValidationError,
    check: NameCheck,
) -> Result<CallToolResult, McpError> {
    use selfie::namespace::NamespaceValidationError as Invalid;

    let (directory_key, listing) = match &error {
        Invalid::PackageDirectoryUnreadable(listing) => ("package_directory", listing),
        Invalid::DotfilesDirectoryUnreadable(listing) => ("dotfiles_directory", listing),
        Invalid::Conflict(conflict) => {
            use selfie::namespace::NameLocation;

            let remedy = match (check, &conflict.found_in) {
                (NameCheck::SpecCreate, NameLocation::Packages) => {
                    "Use selfie_spec_update to change it, or choose a different name."
                }
                (NameCheck::DotfilesTrack, NameLocation::Packages) => {
                    "Use selfie_package_track_dotfile to track a file for it, or choose a \
                     different name."
                }
                (_, NameLocation::Dotfiles) => "Remove it first or choose a different name.",
            };
            return Err(McpError::invalid_params(
                format!("{conflict} {remedy}"),
                None,
            ));
        }
    };
    let mut result = serde_json::json!({
        "status": "refused",
        "reason": error.to_string(),
    });
    result[directory_key] = listing.path().display().to_string().into();
    let payload = serde_json::json!({ "result": result, "data": [] });
    Ok(CallToolResult::error(vec![ContentBlock::text(
        serde_json::to_string_pretty(&payload).unwrap_or_default(),
    )]))
}

/// Which tool's name check refused, for the remedy it offers.
#[derive(Clone, Copy)]
enum NameCheck {
    SpecCreate,
    DotfilesTrack,
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

    // A taken name reaches an assistant as the fact and the remedy for its tool.
    #[test]
    fn a_name_conflict_is_the_taken_name_sentence() {
        let error = selfie::namespace::NamespaceValidationError::Conflict(
            selfie::namespace::NamespaceConflict {
                name: "rc".to_string(),
                found_in: selfie::namespace::NameLocation::Dotfiles,
            },
        );

        let Err(refusal) = namespace_refusal(error, NameCheck::DotfilesTrack) else {
            panic!("a conflict is an error");
        };

        assert_eq!(
            refusal.message,
            "A dotfile spec named 'rc' already exists. Remove it first or choose a different name."
        );
    }

    // A package's name gets the remedy for the tool that asked: an update for
    // a create, the package's own track tool for a track.
    #[test]
    fn a_package_name_conflict_offers_the_calling_tools_remedy() {
        let conflict = || {
            selfie::namespace::NamespaceValidationError::Conflict(
                selfie::namespace::NamespaceConflict {
                    name: "bat".to_string(),
                    found_in: selfie::namespace::NameLocation::Packages,
                },
            )
        };

        let Err(create) = namespace_refusal(conflict(), NameCheck::SpecCreate) else {
            panic!("a conflict is an error");
        };
        let Err(track) = namespace_refusal(conflict(), NameCheck::DotfilesTrack) else {
            panic!("a conflict is an error");
        };

        assert!(create.message.starts_with("'bat' is already a package."));
        assert!(create.message.contains("selfie_spec_update"), "{create:?}");
        assert!(
            track.message.contains("selfie_package_track_dotfile"),
            "{track:?}"
        );
    }

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
            let json = crate::event_collector::dotfile_entry_json("creds", None, &entry(yaml));

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
            dotfiles_repository(&config),
            ShellCommandRunner::login_shell(config.command_timeout()),
            GixGitStatusProvider,
            config.clone(),
            CancellationToken::new(),
        );
        SelfieServer::new(service, config, Vec::new())
    }

    // `selfie_spec_create` for `name`, with an inert install command.
    async fn create(server: &SelfieServer, name: &str) -> serde_json::Value {
        let params: CreateParam = serde_json::from_value(serde_json::json!({
            "package": name,
            "install": "true",
            "environment": "test",
        }))
        .unwrap();
        tool_json(&server.spec_create(Parameters(params)).await.unwrap())
    }

    // The name rule is the library's, so the tool refuses what the loader would
    // refuse, a path that climbs out included, and admits what it would load.
    #[tokio::test]
    async fn spec_create_follows_the_spec_name_rule() {
        let temp = tempfile::TempDir::new().unwrap();
        let packages = temp.path().join("packages");
        std::fs::create_dir_all(&packages).unwrap();
        let server = server_over(&packages, None);

        // A file really is there outside the package directory, so a lookup
        // would answer "already taken"; the name rule must answer first.
        let outside = temp.path().join("outside.yml");
        std::fs::write(&outside, "keep").unwrap();

        for name in ["my tool", "../outside", ".hidden"] {
            let json = create(&server, name).await;
            assert!(
                json.to_string().contains("not a valid spec name"),
                "{name}: got {json}"
            );
            assert!(
                !json.to_string().contains("already taken"),
                "{name}: got {json}"
            );
        }
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep");
        assert_eq!(std::fs::read_dir(&packages).unwrap().count(), 0);

        let json = create(&server, "node@20").await;
        assert!(
            packages.join("node@20.yml").exists(),
            "a legal name must be created: {json}"
        );
    }

    // The first package on a fresh machine: the package directory is not there
    // yet, holds no names, and the save creates it.
    #[tokio::test]
    async fn spec_create_writes_the_first_package_before_the_package_directory_exists() {
        let temp = tempfile::TempDir::new().unwrap();
        let packages = temp.path().join("packages");
        let server = server_over(&packages, None);

        let json = create(&server, "brandnew").await;

        assert!(
            packages.join("brandnew.yml").is_file(),
            "the first package must be written: {json}"
        );
    }

    // A file at the package directory's path says nothing about the name, so the
    // tool refuses with the directory rather than calling the name invalid.
    #[tokio::test]
    async fn spec_create_refuses_when_the_package_directory_is_a_file() {
        let temp = tempfile::TempDir::new().unwrap();
        let packages = temp.path().join("packages");
        std::fs::write(&packages, "not a directory").unwrap();
        let server = server_over(&packages, None);

        let json = create(&server, "brandnew").await;

        assert_eq!(json["result"]["status"], "refused", "got {json}");
        assert_eq!(
            json["result"]["package_directory"],
            packages.display().to_string(),
            "got {json}"
        );
        let reason = json["result"]["reason"].as_str().unwrap_or_default();
        assert!(
            reason.contains("is not a directory, it is a regular file"),
            "got {json}"
        );
        assert_eq!(
            std::fs::read_to_string(&packages).unwrap(),
            "not a directory"
        );
    }

    // A spec `spec validate` would report an error for is not written, and the
    // failure carries each issue as fields.
    #[tokio::test]
    async fn spec_create_refuses_a_spec_that_would_not_validate() {
        let temp = tempfile::TempDir::new().unwrap();
        let packages = temp.path().join("packages");
        std::fs::create_dir_all(&packages).unwrap();
        let server = server_over(&packages, None);
        let params: CreateParam = serde_json::from_value(serde_json::json!({
            "package": "broken",
            "install": "",
            "environment": "test",
        }))
        .unwrap();

        let json = tool_json(&server.spec_create(Parameters(params)).await.unwrap());

        assert!(!packages.join("broken.yml").exists(), "got {json}");
        let issues = json["result"]["issues"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert!(
            issues
                .iter()
                .any(|issue| issue["field"] == "environments.test.install"
                    && issue["level"] == "error"),
            "got {json}"
        );
    }

    // A command that does not parse as POSIX sh may run in the user's own shell, so
    // it is written and reported as a warning, never refused.
    #[tokio::test]
    async fn spec_create_writes_a_command_that_is_not_posix_sh() {
        let temp = tempfile::TempDir::new().unwrap();
        let packages = temp.path().join("packages");
        std::fs::create_dir_all(&packages).unwrap();
        let server = server_over(&packages, None);
        let params: CreateParam = serde_json::from_value(serde_json::json!({
            "package": "fishy",
            "install": r"echo 'it\'s fish'",
            "environment": "test",
        }))
        .unwrap();

        let json = tool_json(&server.spec_create(Parameters(params)).await.unwrap());

        assert_eq!(json["result"]["status"], "success", "got {json}");
        assert!(packages.join("fishy.yml").is_file(), "got {json}");
        assert!(
            json.to_string().contains("does not parse as POSIX sh"),
            "got {json}"
        );
    }

    // A warning does not stop the create: the spec is written and the warning is
    // reported with it.
    #[tokio::test]
    async fn spec_create_reports_a_warning_and_still_writes() {
        let temp = tempfile::TempDir::new().unwrap();
        let packages = temp.path().join("packages");
        std::fs::create_dir_all(&packages).unwrap();
        let server = server_over(&packages, None);
        // The server's environment is `test`; this spec configures only `other`.
        let params: CreateParam = serde_json::from_value(serde_json::json!({
            "package": "elsewhere",
            "install": "true",
            "environment": "other",
        }))
        .unwrap();

        let json = tool_json(&server.spec_create(Parameters(params)).await.unwrap());

        assert!(packages.join("elsewhere.yml").is_file(), "got {json}");
        let data = json["data"].as_array().cloned().unwrap_or_default();
        assert!(
            data.iter().any(|entry| entry["type"] == "validation_result"
                && entry["issues"]
                    .as_array()
                    .is_some_and(
                        |issues| issues.iter().any(|issue| issue["level"] == "warning"
                            && issue["message"]
                                .as_str()
                                .is_some_and(|m| m.contains("Current environment 'test'")))
                    )),
            "got {json}"
        );
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
            warnings[0].starts_with("Dotfiles directory ")
                && warnings[0].contains(&format!("{} does not exist", dotfiles.display())),
            "got: {}",
            warnings[0]
        );
    }

    // A named apply's result carries no row about a name it was not asked for:
    // not another name's ambiguity, and not another spec it could not load.
    #[tokio::test]
    async fn a_named_apply_reports_nothing_about_other_names() {
        let temp = tempfile::TempDir::new().unwrap();
        let packages = temp.path().join("packages");
        std::fs::create_dir_all(&packages).unwrap();
        let target = temp.path().join("t.conf");
        std::fs::write(packages.join("t.conf"), "T").unwrap();
        let spec = format!(
            "name: X\nenvironments:\n  test:\n    install: \"true\"\ndotfiles:\n  \
             - source: \"t.conf\"\n    target: \"{}\"\n",
            target.display()
        );
        std::fs::write(packages.join("good.yml"), spec.replace("X", "good")).unwrap();
        std::fs::write(packages.join("nv.yml"), spec.replace("X", "nv")).unwrap();
        std::fs::write(packages.join("nv.yaml"), spec.replace("X", "nv")).unwrap();
        std::fs::write(packages.join("broken.yml"), "environments: {oops\n").unwrap();

        let server = server_over(&packages, None);
        let json = tool_json(
            &server
                .selfie_apply_dotfiles(Parameters(ApplyParam {
                    name: Some("good".to_string()),
                    dry_run: true,
                    auto_accept: false,
                }))
                .await
                .unwrap(),
        );

        // Matched on content, since a run may warn about its own deploy state.
        let rows = json["data"].as_array().unwrap();
        assert!(
            !rows.iter().any(|row| row["type"] == "spec_skipped"
                || row["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("'nv'") || m.contains("broken.yml"))),
            "got: {json}"
        );
    }

    // The listing's `error` for a refused entry keeps the full sentence, anchor
    // advice included: an assistant has room for it and needs the remedy.
    #[tokio::test]
    async fn the_list_tool_gives_a_shadowing_key_s_advice() {
        let temp = tempfile::TempDir::new().unwrap();
        let packages = temp.path().join("packages");
        std::fs::create_dir_all(&packages).unwrap();
        std::fs::write(
            packages.join("anchor.yml"),
            "name: anchor\nenvironments:\n  test:\n    install: \"true\"\ndotfiles:\n  \
             - source: \"a.conf\"\n    target: \"~/.a.conf\"\n    _target: \"x\"\n",
        )
        .unwrap();

        let server = server_over(&packages, None);
        let json = tool_json(&server.selfie_dotfiles_list().await.unwrap());

        assert!(
            json.to_string().contains("Anchors are legal here"),
            "got: {json}"
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
