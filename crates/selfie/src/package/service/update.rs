//!
//! Helps break down the pieces of running the `package update` command.
//!

use crate::{
    config::SelfieConfig,
    package::{
        EnvironmentConfig,
        event::{EventSender, OperationResult, OperationSuccess, PackageUpdateFields},
        port::PackageRepository,
        service::ProgressTracker,
    },
};

pub(super) async fn handle_update<PR>(
    package_name: &str,
    fields: PackageUpdateFields,
    repo: &PR,
    config: &SelfieConfig,
    sender: &EventSender,
    progress: &mut ProgressTracker,
) -> OperationResult
where
    PR: PackageRepository,
{
    // Step 1: Load the package
    progress.next(sender, "Loading package").await;

    let get_package = match repo.get_package(package_name) {
        Ok(pkg) => pkg,
        Err(err) => return OperationResult::Failure(err.into()),
    };

    let file_path = get_package.file_path().to_path_buf();
    let mut package = get_package.into_package();

    // Step 2: Apply changes
    progress.next(sender, "Applying updates").await;

    // Apply top-level fields
    if let Some(description) = fields.description {
        package.description = Some(description);
    }

    if let Some(homepage) = fields.homepage {
        package.homepage = Some(homepage);
    }

    // Check if environment-scoped fields are present without an environment target
    let has_env_scoped_fields = fields.install.is_some()
        || fields.check.is_some()
        || fields.audit.is_some()
        || fields.dependencies.is_some();

    if has_env_scoped_fields && fields.environment.is_none() {
        return OperationResult::Failure(
            "Environment-scoped fields (install, check, audit, dependencies) require an environment target".into(),
        );
    }

    // Apply environment-scoped fields
    if let Some(ref env_name) = fields.environment {
        if let Some(env_config) = package.environments.get_mut(env_name) {
            if let Some(install) = fields.install {
                env_config.install = install;
            }
            if let Some(check) = fields.check {
                env_config.check = check;
            }
            if let Some(audit) = fields.audit {
                env_config.audit = audit;
            }
            if let Some(dependencies) = fields.dependencies {
                env_config.dependencies = dependencies;
            }
        } else {
            return OperationResult::Failure(
                format!("Environment '{env_name}' not found in package '{package_name}'").into(),
            );
        }
    }

    // Handle add_environment
    if let Some(add_env) = fields.add_environment {
        if package.environments.contains_key(&add_env.name) {
            return OperationResult::Failure(
                format!(
                    "Environment '{}' already exists in package '{package_name}'",
                    add_env.name
                )
                .into(),
            );
        }

        package.environments.insert(
            add_env.name,
            EnvironmentConfig::new(
                add_env.install,
                add_env.check,
                add_env.audit,
                add_env.dependencies,
            ),
        );
    }

    // Handle remove_environment
    if let Some(ref remove_env) = fields.remove_environment
        && package.environments.remove(remove_env).is_none()
    {
        return OperationResult::Failure(
            format!("Environment '{remove_env}' not found in package '{package_name}'").into(),
        );
    }

    // Step 3: Validate and save
    progress.next(sender, "Validating and saving package").await;

    let validation = package.validate(config.environment());
    if validation.issues().has_errors() {
        let error_messages: Vec<String> = validation
            .issues()
            .errors()
            .iter()
            .map(|i| format!("{}: {}", i.field(), i.message()))
            .collect();
        return OperationResult::Failure(
            format!("Validation failed: {}", error_messages.join("; ")).into(),
        );
    }

    if let Err(err) = repo.save_package(&package, &file_path) {
        return OperationResult::Failure(err.into());
    }

    sender
        .send_debug(format!(
            "Package '{}' updated at {}",
            package_name,
            file_path.display()
        ))
        .await;

    OperationResult::Success(OperationSuccess::package_updated(
        package_name.to_string(),
        config.environment().to_string(),
        (progress.current_step(), progress.total_steps()).into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::SelfieConfigBuilder,
        package::{
            GetPackage, PackageBuilder,
            event::{AddEnvironment, OperationContext, OperationResult},
            port::MockPackageRepository,
            service::ProgressTracker,
        },
    };
    use std::path::PathBuf;
    use tokio::sync::mpsc;

    fn test_config() -> crate::config::SelfieConfig {
        SelfieConfigBuilder::default()
            .environment("test-env")
            .package_directory("/test/packages")
            .build()
    }

    fn test_sender() -> (
        EventSender,
        mpsc::Receiver<crate::package::event::PackageEvent>,
    ) {
        let (tx, rx) = mpsc::channel(32);
        let sender = EventSender::new_with_context(
            tx,
            crate::package::event::metadata::OperationType::PackageUpdate,
            "test-pkg".to_string(),
            "test-env".to_string(),
            OperationContext::default(),
        );
        (sender, rx)
    }

    fn create_test_package(name: &str) -> crate::package::Package {
        PackageBuilder::default()
            .name(name)
            .version("1.0.0")
            .description("Test package")
            .homepage("https://example.com")
            .environment("test-env", |b| {
                b.install("brew install test")
                    .check_some("which test")
                    .audit_some("brew info test")
                    .dependencies(vec!["dep1"])
            })
            .path("/test/packages/test-pkg.yml")
            .build()
    }

    #[tokio::test]
    async fn test_update_description() {
        let mut mock_repo = MockPackageRepository::new();
        let config = test_config();
        let (sender, _rx) = test_sender();
        let mut progress = ProgressTracker::new(3);

        let package = create_test_package("test-pkg");
        let get_package =
            GetPackage::from_existing(package, PathBuf::from("/test/packages/test-pkg.yml"));

        mock_repo
            .expect_get_package()
            .return_once(move |_| Ok(get_package));

        mock_repo.expect_save_package().returning(|pkg, _| {
            assert_eq!(pkg.description(), Some("Updated description"));
            Ok(())
        });

        let fields = PackageUpdateFields {
            description: Some("Updated description".to_string()),
            ..Default::default()
        };

        let result = handle_update(
            "test-pkg",
            fields,
            &mock_repo,
            &config,
            &sender,
            &mut progress,
        )
        .await;

        assert!(matches!(result, OperationResult::Success(_)));
    }

    #[tokio::test]
    async fn test_update_environment_scoped_fields() {
        let mut mock_repo = MockPackageRepository::new();
        let config = test_config();
        let (sender, _rx) = test_sender();
        let mut progress = ProgressTracker::new(3);

        let package = create_test_package("test-pkg");
        let get_package =
            GetPackage::from_existing(package, PathBuf::from("/test/packages/test-pkg.yml"));

        mock_repo
            .expect_get_package()
            .return_once(move |_| Ok(get_package));

        mock_repo.expect_save_package().returning(|pkg, _| {
            let env = pkg.environments().get("test-env").unwrap();
            assert_eq!(env.install(), "npm install test");
            assert_eq!(env.check(), None);
            assert_eq!(env.audit(), Some("npm audit test"));
            Ok(())
        });

        let fields = PackageUpdateFields {
            install: Some("npm install test".to_string()),
            check: Some(None), // Remove check command
            audit: Some(Some("npm audit test".to_string())),
            environment: Some("test-env".to_string()),
            ..Default::default()
        };

        let result = handle_update(
            "test-pkg",
            fields,
            &mock_repo,
            &config,
            &sender,
            &mut progress,
        )
        .await;

        assert!(matches!(result, OperationResult::Success(_)));
    }

    #[tokio::test]
    async fn test_update_add_environment() {
        let mut mock_repo = MockPackageRepository::new();
        let config = test_config();
        let (sender, _rx) = test_sender();
        let mut progress = ProgressTracker::new(3);

        let package = create_test_package("test-pkg");
        let get_package =
            GetPackage::from_existing(package, PathBuf::from("/test/packages/test-pkg.yml"));

        mock_repo
            .expect_get_package()
            .return_once(move |_| Ok(get_package));

        mock_repo.expect_save_package().returning(|pkg, _| {
            assert!(pkg.environments().contains_key("new-env"));
            let env = pkg.environments().get("new-env").unwrap();
            assert_eq!(env.install(), "apt install test");
            assert_eq!(env.check(), Some("dpkg -l test"));
            Ok(())
        });

        let fields = PackageUpdateFields {
            add_environment: Some(AddEnvironment {
                name: "new-env".to_string(),
                install: "apt install test".to_string(),
                check: Some("dpkg -l test".to_string()),
                audit: None,
                dependencies: vec![],
            }),
            ..Default::default()
        };

        let result = handle_update(
            "test-pkg",
            fields,
            &mock_repo,
            &config,
            &sender,
            &mut progress,
        )
        .await;

        assert!(matches!(result, OperationResult::Success(_)));
    }

    #[tokio::test]
    async fn test_update_remove_environment() {
        let mut mock_repo = MockPackageRepository::new();
        let config = test_config();
        let (sender, _rx) = test_sender();
        let mut progress = ProgressTracker::new(3);

        // Package with two environments so removing one still leaves a valid package
        let package = PackageBuilder::default()
            .name("test-pkg")
            .version("1.0.0")
            .environment("test-env", |b| b.install("brew install test"))
            .environment("other-env", |b| b.install("apt install test"))
            .path("/test/packages/test-pkg.yml")
            .build();
        let get_package =
            GetPackage::from_existing(package, PathBuf::from("/test/packages/test-pkg.yml"));

        mock_repo
            .expect_get_package()
            .return_once(move |_| Ok(get_package));

        mock_repo.expect_save_package().returning(|pkg, _| {
            assert!(!pkg.environments().contains_key("test-env"));
            assert!(pkg.environments().contains_key("other-env"));
            Ok(())
        });

        let fields = PackageUpdateFields {
            remove_environment: Some("test-env".to_string()),
            ..Default::default()
        };

        let result = handle_update(
            "test-pkg",
            fields,
            &mock_repo,
            &config,
            &sender,
            &mut progress,
        )
        .await;

        assert!(matches!(result, OperationResult::Success(_)));
    }

    #[tokio::test]
    async fn test_update_env_scoped_without_environment_errors() {
        let mut mock_repo = MockPackageRepository::new();
        let config = test_config();
        let (sender, _rx) = test_sender();
        let mut progress = ProgressTracker::new(3);

        let package = create_test_package("test-pkg");
        let get_package =
            GetPackage::from_existing(package, PathBuf::from("/test/packages/test-pkg.yml"));

        mock_repo
            .expect_get_package()
            .return_once(move |_| Ok(get_package));

        let fields = PackageUpdateFields {
            install: Some("new install command".to_string()),
            // No environment set!
            ..Default::default()
        };

        let result = handle_update(
            "test-pkg",
            fields,
            &mock_repo,
            &config,
            &sender,
            &mut progress,
        )
        .await;

        assert!(matches!(result, OperationResult::Failure(_)));
    }

    #[tokio::test]
    async fn test_update_nonexistent_environment_target_errors() {
        let mut mock_repo = MockPackageRepository::new();
        let config = test_config();
        let (sender, _rx) = test_sender();
        let mut progress = ProgressTracker::new(3);

        let package = create_test_package("test-pkg");
        let get_package =
            GetPackage::from_existing(package, PathBuf::from("/test/packages/test-pkg.yml"));

        mock_repo
            .expect_get_package()
            .return_once(move |_| Ok(get_package));

        let fields = PackageUpdateFields {
            install: Some("new install command".to_string()),
            environment: Some("nonexistent-env".to_string()),
            ..Default::default()
        };

        let result = handle_update(
            "test-pkg",
            fields,
            &mock_repo,
            &config,
            &sender,
            &mut progress,
        )
        .await;

        assert!(matches!(result, OperationResult::Failure(_)));
    }
}
