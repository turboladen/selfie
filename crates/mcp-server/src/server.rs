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
        EnvironmentConfig, Package, PackageService, event::PackageUpdateFields,
        port::PackageRepository, repository::yaml::YamlPackageRepository,
        service::PackageServiceImpl,
    },
};
use serde::Deserialize;

use crate::event_collector;

type ConcreteService =
    PackageServiceImpl<YamlPackageRepository<RealFileSystem>, ShellCommandRunner>;

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
        name = "selfie_list_packages",
        description = "List packages relevant to the current environment with their installation status (installed/not installed). Set all=true to include packages from other environments too."
    )]
    async fn list_packages(
        &self,
        Parameters(params): Parameters<ListParam>,
    ) -> Result<CallToolResult, McpError> {
        let stream = self.service.list(params.all).await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    #[tool(
        name = "selfie_get_package",
        description = "Get detailed information about a specific package including environments, dependencies, and status"
    )]
    async fn get_package(
        &self,
        Parameters(params): Parameters<PackageNameParam>,
    ) -> Result<CallToolResult, McpError> {
        let stream = self.service.info(&params.package).await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    #[tool(
        name = "selfie_check_package",
        description = "Check if a package is installed in the current environment"
    )]
    async fn check_package(
        &self,
        Parameters(params): Parameters<PackageNameParam>,
    ) -> Result<CallToolResult, McpError> {
        let stream = self.service.check(&params.package).await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    #[tool(
        name = "selfie_validate_package",
        description = "Validate a package definition file for correctness"
    )]
    async fn validate_package(
        &self,
        Parameters(params): Parameters<PackageNameParam>,
    ) -> Result<CallToolResult, McpError> {
        let stream = self.service.validate(&params.package, None).await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    #[tool(
        name = "selfie_audit_package",
        description = "Audit a package's installation sources and detect conflicts (e.g., installed via both npm and homebrew)"
    )]
    async fn audit_package(
        &self,
        Parameters(params): Parameters<PackageNameParam>,
    ) -> Result<CallToolResult, McpError> {
        let stream = self.service.audit(&params.package).await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    #[tool(
        name = "selfie_audit_all",
        description = "Audit all packages for the current environment for installation source conflicts. Returns per-package audit results."
    )]
    async fn audit_all(&self) -> Result<CallToolResult, McpError> {
        let stream = self.service.audit_all().await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    #[tool(
        name = "selfie_install_package",
        description = "Install a package using its configured installation method for the current environment"
    )]
    async fn install_package(
        &self,
        Parameters(params): Parameters<InstallParam>,
    ) -> Result<CallToolResult, McpError> {
        let stream = self.service.install(&params.package).await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    #[tool(
        name = "selfie_create_package",
        description = "Create a new package definition file. Requires name, environment, and install command at minimum. The environment should match the user's current selfie environment (use selfie_get_config to check)."
    )]
    async fn create_package(
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
            ),
        );

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
        name = "selfie_update_package",
        description = "Update fields of an existing package. Environment-scoped fields (install, check, audit, dependencies) require the environment parameter."
    )]
    async fn update_package(
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
                });

        let fields = PackageUpdateFields {
            description: params.description,
            homepage: params.homepage,
            install: params.install,
            check,
            audit,
            dependencies: params.dependencies,
            environment: params.environment,
            add_environment,
            remove_environment: params.remove_environment,
        };

        let stream = self.service.update(&params.package, fields).await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    #[tool(
        name = "selfie_remove_package",
        description = "Remove a package definition file. Warning: this is permanent and may break dependent packages."
    )]
    async fn remove_package(
        &self,
        Parameters(params): Parameters<RemoveParam>,
    ) -> Result<CallToolResult, McpError> {
        let stream = self.service.remove(&params.package).await;
        let result = event_collector::collect_events(stream).await;
        Ok(tool_result(result))
    }

    #[tool(
        name = "selfie_get_all_specs",
        description = "Get the full definition (name, version, description, homepage, environments with install/check/audit commands and dependencies) for all packages in the current environment. Fast bulk read — no commands are executed. Use this instead of calling selfie_get_package repeatedly."
    )]
    async fn get_all_specs(&self) -> Result<CallToolResult, McpError> {
        let repo =
            YamlPackageRepository::new(RealFileSystem, self.config.package_directory().clone());
        let packages = repo
            .list_packages()
            .map_err(|e| McpError::internal_error(format!("Failed to list packages: {e}"), None))?;

        let current_env = self.config.environment();
        let specs: Vec<serde_json::Value> = packages
            .valid_packages()
            .filter(|p| p.environments().contains_key(current_env))
            .map(|p| {
                let envs: serde_json::Map<String, serde_json::Value> = p
                    .environments()
                    .iter()
                    .map(|(env_name, env_config)| {
                        (
                            env_name.clone(),
                            serde_json::json!({
                                "install": env_config.install(),
                                "check": env_config.check(),
                                "audit": env_config.audit(),
                                "dependencies": env_config.dependencies(),
                            }),
                        )
                    })
                    .collect();

                serde_json::json!({
                    "name": p.name(),
                    "version": p.version(),
                    "description": p.description(),
                    "homepage": p.homepage(),
                    "environments": envs,
                })
            })
            .collect();

        let result = serde_json::json!({
            "environment": current_env,
            "package_count": specs.len(),
            "packages": specs,
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    #[tool(
        name = "selfie_validate_all",
        description = "Validate all package definition files in the current environment for correctness. Returns validation issues (errors and warnings) per package. Fast — no commands are executed."
    )]
    async fn validate_all(&self) -> Result<CallToolResult, McpError> {
        let repo =
            YamlPackageRepository::new(RealFileSystem, self.config.package_directory().clone());
        let packages = repo
            .list_packages()
            .map_err(|e| McpError::internal_error(format!("Failed to list packages: {e}"), None))?;

        let current_env = self.config.environment();
        let mut results: Vec<serde_json::Value> = Vec::new();
        let mut packages_with_errors = 0usize;
        let mut packages_with_warnings = 0usize;

        for package in packages
            .valid_packages()
            .filter(|p| p.environments().contains_key(current_env))
        {
            let validation = package.validate(current_env);
            let issues: Vec<serde_json::Value> = validation
                .issues()
                .all_issues()
                .iter()
                .map(|i| {
                    serde_json::json!({
                        "field": i.field(),
                        "message": i.message(),
                        "level": format!("{:?}", i.level()),
                        "suggestion": i.suggestion(),
                    })
                })
                .collect();

            if validation.issues().has_errors() {
                packages_with_errors += 1;
            }
            if validation.issues().has_warnings() {
                packages_with_warnings += 1;
            }

            if validation.issues().has_issues() {
                results.push(serde_json::json!({
                    "package": package.name(),
                    "valid": !validation.issues().has_errors(),
                    "issues": issues,
                }));
            }
        }

        let result = serde_json::json!({
            "environment": current_env,
            "packages_with_errors": packages_with_errors,
            "packages_with_warnings": packages_with_warnings,
            "results": results,
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    #[tool(
        name = "selfie_get_config",
        description = "Get the current selfie configuration including environment, package directory, and settings"
    )]
    async fn get_config(&self) -> Result<CallToolResult, McpError> {
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
