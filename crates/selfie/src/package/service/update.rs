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
        unspanned,
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
        package.description = Some(unspanned(description));
    }

    if let Some(homepage) = fields.homepage {
        package.homepage = Some(unspanned(homepage));
    }

    // Kept for the blank-install check below, since applying the fields moves them.
    let fields_install = fields.install.clone();
    let added_install = fields
        .add_environment
        .as_ref()
        .map(|env| env.install.clone());

    // Check if environment-scoped fields are present without an environment target
    let has_env_scoped_fields = fields.install.is_some()
        || fields.check.is_some()
        || fields.audit.is_some()
        || fields.dependencies.is_some()
        || fields.recommends.is_some();

    if has_env_scoped_fields && fields.environment.is_none() {
        return OperationResult::Failure(
            "Environment-scoped fields (install, check, audit, dependencies, recommends) require an environment target".into(),
        );
    }

    // Track which environments are modified so we can scope command syntax
    // validation to only those — avoids a deadlock where pre-existing errors
    // in untouched environments block fixes to the targeted environment.
    let mut modified_envs: Vec<String> = Vec::new();

    // Apply environment-scoped fields
    if let Some(ref env_name) = fields.environment {
        if let Some(env_config) = package.environments.value.get_mut(env_name) {
            modified_envs.push(env_name.clone());
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
            if let Some(recommends) = fields.recommends {
                env_config.recommends = recommends;
            }
        } else {
            return OperationResult::Failure(
                format!("Environment '{env_name}' not found in package '{package_name}'").into(),
            );
        }
    }

    // Handle add_environment
    if let Some(add_env) = fields.add_environment {
        if package.environments.value.contains_key(&add_env.name) {
            return OperationResult::Failure(
                format!(
                    "Environment '{}' already exists in package '{package_name}'",
                    add_env.name
                )
                .into(),
            );
        }

        modified_envs.push(add_env.name.clone());
        package.environments.value.insert(
            add_env.name,
            EnvironmentConfig::new(
                add_env.install,
                add_env.check,
                add_env.audit,
                add_env.dependencies,
                add_env.recommends,
            ),
        );
    }

    // Handle remove_environment
    if let Some(ref remove_env) = fields.remove_environment
        && package
            .environments
            .value
            .shift_remove(remove_env)
            .is_none()
    {
        return OperationResult::Failure(
            format!("Environment '{remove_env}' not found in package '{package_name}'").into(),
        );
    }

    // A blank install the caller gave would be saved over a working command and
    // refused by every later install, so it is refused here, where it was given,
    // after the environment it names is known to exist. Validation only warns
    // about a blank install, because the spec create template writes one on
    // purpose.
    let mut given_install = fields_install
        .iter()
        .chain(added_install.iter())
        .map(String::as_str);
    if given_install.any(crate::package::is_blank_command) {
        return OperationResult::Failure(crate::package::BLANK_INSTALL_GIVEN.into());
    }

    // Step 3: Validate and save
    progress.next(sender, "Validating and saving package").await;

    // Validate top-level fields (name, version, URLs, env existence) globally
    let mut all_issues = Vec::new();
    all_issues.extend(package.validate_required_fields());
    all_issues.extend(package.validate_urls());
    all_issues.extend(package.validate_environments_contents(config.environment()));
    // `save_package` refuses a package carrying these anyway; reporting them here
    // gives the field path and the expected key names rather than a bare save
    // failure, and refuses before the file is touched.
    all_issues.extend(package.validate_unknown_dotfile_fields());

    // Validate command syntax only for environments we actually touched
    if modified_envs.is_empty() {
        // No env-scoped changes — skip command syntax validation entirely
    } else {
        all_issues
            .extend(package.validate_command_syntax_for(modified_envs.iter().map(String::as_str)));
    }

    let issues: crate::validation::ValidationIssues = all_issues.into();
    if issues.has_errors() {
        let error_messages: Vec<String> = issues
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
            assert_eq!(env.install(), Some("npm install test"));
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
            assert_eq!(env.install(), Some("apt install test"));
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
                recommends: vec![],
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

    // Removing an environment leaves the rest in the order the file gave them. Three
    // environments, removing the first: a removal that moved the last one into the
    // gap would write `mid` before `alpha`.
    #[tokio::test]
    async fn removing_an_environment_keeps_the_rest_in_order() {
        let mut mock_repo = MockPackageRepository::new();
        let config = test_config();
        let (sender, _rx) = test_sender();
        let mut progress = ProgressTracker::new(3);

        let package = PackageBuilder::default()
            .name("test-pkg")
            .environment("zeta", |b| b.install("echo z"))
            .environment("alpha", |b| b.install("echo a"))
            .environment("mid", |b| b.install("echo m"))
            .path("/test/packages/test-pkg.yml")
            .build();
        let get_package =
            GetPackage::from_existing(package, PathBuf::from("/test/packages/test-pkg.yml"));
        mock_repo
            .expect_get_package()
            .return_once(move |_| Ok(get_package));

        let saved = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = std::sync::Arc::clone(&saved);
        mock_repo.expect_save_package().returning(move |pkg, _| {
            *sink.lock().unwrap() = pkg.environments().keys().cloned().collect();
            Ok(())
        });

        let fields = PackageUpdateFields {
            remove_environment: Some("zeta".to_string()),
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

        assert!(matches!(result, OperationResult::Success(_)), "{result:?}");
        assert_eq!(*saved.lock().unwrap(), ["alpha", "mid"]);
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

    #[tokio::test]
    async fn test_update_env_with_error_in_other_env_succeeds() {
        // Regression: fixing one environment should not be blocked by
        // pre-existing errors in a different environment.
        let mut mock_repo = MockPackageRepository::new();
        let config = test_config();
        let (sender, _rx) = test_sender();
        let mut progress = ProgressTracker::new(3);

        // Package where both envs have unmatched single quotes
        let package = PackageBuilder::default()
            .name("test-pkg")
            .environment("test-env", |b| b.install("echo it's broken"))
            .environment("other-env", |b| b.install("echo it's also broken"))
            .path("/test/packages/test-pkg.yml")
            .build();
        let get_package =
            GetPackage::from_existing(package, PathBuf::from("/test/packages/test-pkg.yml"));

        mock_repo
            .expect_get_package()
            .return_once(move |_| Ok(get_package));

        mock_repo.expect_save_package().returning(|pkg, _| {
            let env = pkg.environments().get("test-env").unwrap();
            assert_eq!(env.install(), Some("echo fixed"));
            Ok(())
        });

        let fields = PackageUpdateFields {
            install: Some("echo fixed".to_string()),
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

        assert!(
            matches!(result, OperationResult::Success(_)),
            "Fixing test-env should succeed even though other-env still has errors"
        );
    }

    // A blank install given to update would replace a working command with one
    // that runs nothing, so it is refused and nothing is saved. The mock expects
    // no save, so a save fails the test.
    #[tokio::test]
    async fn a_blank_install_given_to_update_is_refused() {
        let blank_set = PackageUpdateFields {
            environment: Some("test-env".to_string()),
            install: Some("   ".to_string()),
            ..Default::default()
        };
        // A real install set alongside a blank one added: the blank one is still
        // caught.
        let blank_added_beside_a_real_one = PackageUpdateFields {
            environment: Some("test-env".to_string()),
            install: Some("brew install real".to_string()),
            add_environment: Some(AddEnvironment {
                name: "linux".to_string(),
                install: String::new(),
                check: None,
                audit: None,
                dependencies: vec![],
                recommends: vec![],
            }),
            ..Default::default()
        };
        let blank_added = PackageUpdateFields {
            add_environment: Some(AddEnvironment {
                name: "new-env".to_string(),
                install: "# TODO".to_string(),
                check: None,
                audit: None,
                dependencies: vec![],
                recommends: vec![],
            }),
            ..Default::default()
        };
        for fields in [blank_set, blank_added, blank_added_beside_a_real_one] {
            let mut mock_repo = MockPackageRepository::new();
            let get_package = GetPackage::from_existing(
                create_test_package("test-pkg"),
                PathBuf::from("/test/packages/test-pkg.yml"),
            );
            mock_repo
                .expect_get_package()
                .return_once(move |_| Ok(get_package));
            let (sender, _rx) = test_sender();
            let mut progress = ProgressTracker::new(3);

            let result = handle_update(
                "test-pkg",
                fields,
                &mock_repo,
                &test_config(),
                &sender,
                &mut progress,
            )
            .await;

            let OperationResult::Failure(failure) = result else {
                panic!("a blank install must be refused, got {result:?}");
            };
            assert!(failure.to_string().contains("blank"), "{failure}");
        }
    }
}
