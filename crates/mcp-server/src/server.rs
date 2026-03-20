use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{
    ErrorData as McpError,
    handler::server::{ServerHandler, tool::ToolRouter},
    model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use selfie::{
    commands::ShellCommandRunner,
    config::SelfieConfig,
    fs::RealFileSystem,
    package::{
        EnvironmentConfig, Package, PackageService, SpecService, event::PackageUpdateFields,
        git_adapter::GixGitStatusProvider, repository::yaml::YamlPackageRepository,
        service::PackageServiceImpl,
    },
};
use serde::Deserialize;

use crate::event_collector;

type ConcreteService = PackageServiceImpl<
    YamlPackageRepository<RealFileSystem>,
    ShellCommandRunner,
    GixGitStatusProvider,
>;

#[derive(Clone)]
pub struct SelfieServer {
    service: Arc<ConcreteService>,
    config: SelfieConfig,
    tool_router: ToolRouter<Self>,
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

// ─── Spec (definition) tools ───────────────────────────────────────────────

#[tool_router]
impl SelfieServer {
    pub fn new(service: ConcreteService, config: SelfieConfig) -> Self {
        Self {
            service: Arc::new(service),
            config,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        name = "selfie_spec_create",
        description = "Create a new spec file. Requires name, environment, and install command at minimum. The environment should match the user's current selfie environment (use selfie_config_get to check). Command conventions: check commands should use 'command -v X' (POSIX portable) not 'which X'. Multi-line install scripts should start with 'set -e' to fail fast."
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

        let file_path = self
            .config
            .package_directory()
            .join(format!("{}.yml", &params.package));

        let package = Package::new(
            params.package,
            "0.1.0".to_string(),
            params.homepage,
            params.description,
            environments,
            file_path,
        );

        let stream = self.service.create(package).await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    #[tool(
        name = "selfie_spec_update",
        description = "Update fields of an existing spec. Environment-scoped fields (install, check, audit, dependencies) require the environment parameter. Command conventions: check commands should use 'command -v X' (POSIX portable) not 'which X'. Multi-line install scripts should start with 'set -e'."
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
            recommends: None,
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
        description = "Update multiple specs in a single call. Each entry in the updates array has the same fields as selfie_spec_update. Use this instead of calling selfie_spec_update repeatedly to avoid hitting tool call limits. Command conventions: check commands should use 'command -v X' not 'which X'. Multi-line install scripts should start with 'set -e'."
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
                recommends: None,
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
        description = "List all specs for the current environment with name, version, description, and environments. Fast — no commands are executed. Use this instead of calling selfie_spec_info repeatedly."
    )]
    async fn spec_list(&self) -> Result<CallToolResult, McpError> {
        let stream = SpecService::list(&*self.service, false).await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    #[tool(
        name = "selfie_spec_validate_all",
        description = "Validate all spec files in the current environment for correctness. Returns validation issues (errors and warnings) per spec. Fast — no commands are executed."
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
        description = "Install a package using its configured installation method for the current environment"
    )]
    async fn package_install(
        &self,
        Parameters(params): Parameters<InstallParam>,
    ) -> Result<CallToolResult, McpError> {
        let stream = self
            .service
            .install(&params.package, selfie::package::InstallOptions::default())
            .await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    #[tool(
        name = "selfie_package_list",
        description = "List packages relevant to the current environment with their installation status (installed/not installed). Set all=true to include packages from other environments too."
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
}

#[tool_handler]
impl ServerHandler for SelfieServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::default())
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
