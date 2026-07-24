use std::{collections::HashMap, path::PathBuf};

use super::{DotfileEntry, EnvironmentConfig, Package};

#[derive(Default)]
pub struct PackageBuilder {
    name: String,
    homepage: Option<String>,
    description: Option<String>,
    dotfiles: Vec<DotfileEntry>,
    post_install_note: Option<String>,
    environments: HashMap<String, EnvironmentConfig>,
    path: PathBuf,
}

impl PackageBuilder {
    /// Create a new package builder with the specified name
    ///
    /// # Arguments
    ///
    /// * `name` - The package name (required)
    #[must_use]
    pub fn name(mut self, name: &str) -> Self {
        self.name = name.to_string();
        self
    }

    /// Set the package homepage URL
    ///
    /// # Arguments
    ///
    /// * `homepage` - The homepage URL for the package
    #[must_use]
    pub fn homepage(mut self, homepage: &str) -> Self {
        self.homepage = Some(homepage.to_string());
        self
    }

    /// Set the package description
    ///
    /// # Arguments
    ///
    /// * `description` - Optional description of the package
    #[must_use]
    pub fn description(mut self, description: &str) -> Self {
        self.description = Some(description.to_string());
        self
    }

    /// Set the dotfile mappings for this package
    #[must_use]
    pub fn dotfiles(mut self, dotfiles: Vec<DotfileEntry>) -> Self {
        self.dotfiles = dotfiles;
        self
    }

    /// Set the post-install note for this package
    #[must_use]
    pub fn post_install_note(mut self, note: &str) -> Self {
        self.post_install_note = Some(note.to_string());
        self
    }

    #[must_use]
    pub fn environment<T, F>(mut self, name: T, env_builder: F) -> Self
    where
        T: AsRef<str>,
        F: Fn(EnvironmentConfigBuilder) -> EnvironmentConfigBuilder,
    {
        self.environments.insert(
            name.as_ref().to_string(),
            env_builder(EnvironmentConfigBuilder::default()).build(),
        );
        self
    }

    #[must_use]
    pub fn path<T>(mut self, path: T) -> Self
    where
        PathBuf: From<T>,
    {
        self.path = path.into();
        self
    }

    /// Build the final Package instance
    ///
    /// Constructs a `Package` with all the configured values. Uses sensible
    /// defaults for any fields that weren't explicitly set.
    #[must_use]
    pub fn build(self) -> Package {
        Package::new(
            self.name,
            self.homepage,
            self.description,
            self.dotfiles,
            self.post_install_note,
            self.environments,
            self.path,
        )
    }
}

#[derive(Default)]
pub struct EnvironmentConfigBuilder {
    install: String,
    check: Option<String>,
    audit: Option<String>,
    dependencies: Vec<String>,
    recommends: Vec<String>,
    dotfiles: Vec<DotfileEntry>,
}
impl EnvironmentConfigBuilder {
    /// Create a new environment configuration builder
    ///
    /// # Arguments
    ///
    /// * `install_command` - The command to install the package
    #[must_use]
    pub fn install<T: AsRef<str>>(mut self, install: T) -> Self {
        self.install = install.as_ref().to_string();
        self
    }

    #[must_use]
    pub fn check<T: AsRef<str>>(mut self, check: Option<T>) -> Self {
        self.check = check.map(|c| c.as_ref().to_string());
        self
    }

    #[must_use]
    pub fn check_some<T: AsRef<str>>(mut self, check: T) -> Self {
        self.check = Some(check.as_ref().to_string());
        self
    }

    #[must_use]
    pub fn audit<T: AsRef<str>>(mut self, audit: Option<T>) -> Self {
        self.audit = audit.map(|a| a.as_ref().to_string());
        self
    }

    #[must_use]
    pub fn audit_some<T: AsRef<str>>(mut self, audit: T) -> Self {
        self.audit = Some(audit.as_ref().to_string());
        self
    }

    #[must_use]
    pub fn dependencies<T: AsRef<str>>(mut self, dependencies: Vec<T>) -> Self {
        self.dependencies = dependencies
            .into_iter()
            .map(|d| d.as_ref().to_string())
            .collect();
        self
    }

    #[must_use]
    pub fn recommends<T: AsRef<str>>(mut self, recommends: Vec<T>) -> Self {
        self.recommends = recommends
            .into_iter()
            .map(|r| r.as_ref().to_string())
            .collect();
        self
    }

    /// Set the environment-specific dotfile mappings
    #[must_use]
    pub fn dotfiles(mut self, dotfiles: Vec<DotfileEntry>) -> Self {
        self.dotfiles = dotfiles;
        self
    }

    /// Build the final `EnvironmentConfig` instance
    ///
    /// Constructs an `EnvironmentConfig` with all the configured values.
    #[must_use]
    pub fn build(self) -> EnvironmentConfig {
        EnvironmentConfig {
            install: self.install,
            check: self.check,
            audit: self.audit,
            dependencies: self.dependencies,
            recommends: self.recommends,
            dotfiles: self.dotfiles,
        }
    }
}
