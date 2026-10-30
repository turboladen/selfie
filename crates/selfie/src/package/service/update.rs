//!
//! Helps break down the pieces of running the `package update` command.
//!

use crate::{
    config::SelfieConfig,
    package::{
        EnvironmentConfig,
        event::{
            EventSender, OperationFailure, OperationResult, OperationSuccess, PackageUpdateFields,
            SpecWrite,
        },
        port::PackageRepository,
        service::{
            ProgressTracker,
            validate::{OnWrite, all_issues, collisions_with_other_specs, refuse_or_report},
        },
        unspanned,
    },
};

// The fields of the spec an update wrote, as validation names them.
#[derive(Default)]
struct WrittenFields {
    // Top-level fields, such as `homepage`.
    top_level: Vec<String>,
    // An environment's field the update set, as (environment, field).
    environment_fields: Vec<(String, String)>,
    // Environments the update added, which it wrote whole.
    added_environments: Vec<String>,
}

impl WrittenFields {
    fn top_level(&mut self, field: &str) {
        self.top_level.push(field.to_string());
    }

    fn environment_field(&mut self, environment: &str, field: &str) {
        self.environment_fields
            .push((environment.to_string(), field.to_string()));
    }

    fn added_environment(&mut self, environment: &str) {
        self.added_environments.push(environment.to_string());
    }

    // Whether the issue field `field` is one the update wrote. `environments` is
    // every environment name the updated spec has, so a field is placed in the
    // longest name it starts with: an environment `a` does not own the fields of
    // an environment `a.b`.
    fn contains<'a>(&self, field: &str, environments: impl Iterator<Item = &'a str>) -> bool {
        if self.top_level.iter().any(|written| written == field) {
            return true;
        }
        let owner = environments
            .filter(|name| {
                let owned = crate::package::environment_field(name, "");
                field == owned.trim_end_matches('.') || field.starts_with(&owned)
            })
            .max_by_key(|name| name.len());
        let Some(owner) = owner else {
            return false;
        };
        if self.added_environments.iter().any(|added| added == owner) {
            return true;
        }
        let rest = field
            .strip_prefix(&crate::package::environment_field(owner, ""))
            .unwrap_or_default();
        // An indexed field, such as `dependencies[0]`, belongs to its list.
        self.environment_fields
            .iter()
            .any(|(environment, written)| {
                environment == owner
                    && rest
                        .strip_prefix(written.as_str())
                        .is_some_and(|after| after.is_empty() || after.starts_with('['))
            })
    }
}

pub(super) async fn handle_update<PR>(
    package_name: &str,
    fields: PackageUpdateFields,
    repo: &PR,
    dotfiles_repo: &PR,
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
        Err(err) => {
            return OperationResult::Failure(
                crate::package::port::with_name_hint(repo, err).into(),
            );
        }
    };

    let file_path = get_package.file_path().to_path_buf();
    let mut package = get_package.into_package();
    // The spec as its file holds it, to tell the collisions with other specs the
    // update brings in from those it already had. A spec with no dotfile entry has
    // none either way, and an update adds no entry.
    let on_disk = package
        .has_dotfiles_in_any_environment()
        .then(|| package.clone());

    // Step 2: Apply changes
    progress.next(sender, "Applying updates").await;

    let mut written = WrittenFields::default();

    // Apply top-level fields
    if let Some(description) = fields.description {
        package.description = Some(unspanned(description));
        written.top_level("description");
    }

    if let Some(homepage) = fields.homepage {
        package.homepage = Some(unspanned(homepage));
        written.top_level("homepage");
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

    // Apply environment-scoped fields
    if let Some(ref env_name) = fields.environment {
        if let Some(env_config) = package.environments.value.get_mut(env_name) {
            if let Some(install) = fields.install {
                env_config.install = install;
                written.environment_field(env_name, "install");
            }
            if let Some(check) = fields.check {
                env_config.check = check;
                written.environment_field(env_name, "check");
            }
            if let Some(audit) = fields.audit {
                env_config.audit = audit;
                written.environment_field(env_name, "audit");
            }
            if let Some(dependencies) = fields.dependencies {
                env_config.dependencies = dependencies;
                written.environment_field(env_name, "dependencies");
            }
            if let Some(recommends) = fields.recommends {
                env_config.recommends = recommends;
                written.environment_field(env_name, "recommends");
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

        written.added_environment(&add_env.name);
        written.top_level("environments");
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
    if fields.remove_environment.is_some() {
        written.top_level("environments");
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

    // The whole updated spec is judged as `spec validate` and a create judge it,
    // and everything found is reported. An error stops the update when it is on a
    // field the update wrote, or when `save_package` would refuse the package
    // anyway; the refusal then names the field path and the expected keys, before
    // the file is touched. An error on any other field was there before the update,
    // so it is reported and does not hold up a fix to a different field.
    let issues = all_issues(&package, repo, config.environment());
    // A collision with another spec stops the update when the update brought it in,
    // and is reported when the file had it already, whichever field it is on.
    let across = match collisions_with_other_specs(
        on_disk.as_ref(),
        &package,
        package_name,
        repo,
        dotfiles_repo,
        config,
        sender,
    )
    .await
    {
        Ok(across) => across,
        // Unplaced, the spec was compared with nothing, so it is not written.
        Err(_) => {
            return OperationResult::Failure(OperationFailure::UnplacedSpec {
                action: SpecWrite::Update,
                package_name: package_name.to_string(),
                path: file_path,
            });
        }
    };
    let refused_on_save: Vec<String> = package
        .save_refusal_issues()
        .iter()
        .map(|issue| issue.field().to_string())
        .collect();
    let environments = || package.environments.value.keys().map(String::as_str);
    let judged = issues
        .all_issues()
        .iter()
        .cloned()
        .map(|issue| {
            let refuses = written.contains(issue.field(), environments())
                || refused_on_save.iter().any(|field| field == issue.field());
            let on_write = if refuses {
                OnWrite::Refuses
            } else {
                OnWrite::Reports
            };
            (issue, on_write)
        })
        .chain(
            across
                .issues
                .into_iter()
                .map(|(issue, standing)| (issue, standing.on_write())),
        )
        .collect();
    if let Some(refusal) = refuse_or_report(
        sender,
        SpecWrite::Update,
        package_name,
        config.environment(),
        judged,
    )
    .await
    {
        return OperationResult::Failure(refusal);
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
            event::{
                AddEnvironment, OperationContext, OperationFailure, OperationResult, Outcome,
                PackageEvent, ValidationResultData,
            },
            port::{MockPackageRepository, PackageRepoError},
            service::ProgressTracker,
        },
        validation::ValidationLevel,
    };
    use std::path::PathBuf;
    use tokio::sync::mpsc;

    fn test_config() -> crate::config::SelfieConfig {
        SelfieConfigBuilder::default()
            .environment("test-env")
            .package_directory("/test/packages")
            .build()
    }

    fn test_sender() -> (EventSender, mpsc::Receiver<PackageEvent>) {
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

    // An update that would leave the spec invalid is refused as invalid, with
    // its issues, as a create is, and nothing is saved: the mock has no
    // expectation for a save, so one fails the test.
    #[tokio::test]
    async fn an_invalid_update_is_refused_with_its_issues() {
        let mut mock_repo = MockPackageRepository::new();
        let config = test_config();
        let (sender, _rx) = test_sender();
        let mut progress = ProgressTracker::new(3);
        let get_package = GetPackage::from_existing(
            create_test_package("test-pkg"),
            PathBuf::from("/test/packages/test-pkg.yml"),
        );
        mock_repo
            .expect_get_package()
            .return_once(move |_| Ok(get_package));

        let fields = PackageUpdateFields {
            homepage: Some("not a url".to_string()),
            ..Default::default()
        };

        let result = handle_update(
            "test-pkg",
            fields,
            &mock_repo,
            &MockPackageRepository::new(),
            &config,
            &sender,
            &mut progress,
        )
        .await;

        let OperationResult::Failure(
            failure @ OperationFailure::InvalidSpec {
                action: crate::package::event::SpecWrite::Update,
                package_name,
                issues,
            },
        ) = &result
        else {
            panic!("expected the update to be refused as invalid, got: {result:?}");
        };
        assert_eq!(package_name, "test-pkg");
        assert!(
            issues.iter().any(|issue| issue.field == "homepage"),
            "{issues:?}"
        );
        assert_eq!(
            failure.to_string(),
            "Refusing to update 'test-pkg': the updated spec would not pass validation (1 error), \
             so nothing was written"
        );
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
            &MockPackageRepository::new(),
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
            &MockPackageRepository::new(),
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
            &MockPackageRepository::new(),
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
            &MockPackageRepository::new(),
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
            &MockPackageRepository::new(),
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
            &MockPackageRepository::new(),
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
            &MockPackageRepository::new(),
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
            &MockPackageRepository::new(),
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
                &MockPackageRepository::new(),
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

    // Every validation result the run sent.
    fn validation_results(rx: &mut mpsc::Receiver<PackageEvent>) -> Vec<ValidationResultData> {
        let mut results = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let PackageEvent::ValidationResultCompleted {
                validation_result, ..
            } = event
            {
                results.push(validation_result);
            }
        }
        results
    }

    // Update `package`'s description against a repository whose save gives `save`
    // and is expected `saves` times, returning how the run ended and what it sent.
    async fn update_description_of(
        package: crate::package::Package,
        saves: usize,
        save: Result<(), PackageRepoError>,
    ) -> (OperationResult, Vec<ValidationResultData>) {
        let mut mock_repo = MockPackageRepository::listing_nothing();
        let (sender, mut rx) = test_sender();
        let mut progress = ProgressTracker::new(3);
        let get_package =
            GetPackage::from_existing(package, PathBuf::from("/test/packages/test-pkg.yml"));
        mock_repo
            .expect_get_package()
            .return_once(move |_| Ok(get_package));
        mock_repo
            .expect_save_package()
            .times(saves)
            .return_once(move |_, _| save);
        let fields = PackageUpdateFields {
            description: Some("New description".to_string()),
            ..Default::default()
        };

        let result = handle_update(
            "test-pkg",
            fields,
            &mock_repo,
            &MockPackageRepository::listing_nothing(),
            &test_config(),
            &sender,
            &mut progress,
        )
        .await;
        (result, validation_results(&mut rx))
    }

    // The configured environment is `test-env`; this spec configures only `other`.
    fn package_warning_about_its_environment() -> crate::package::Package {
        PackageBuilder::default()
            .name("test-pkg")
            .environment("other", |b| b.install("true"))
            .path("/test/packages/test-pkg.yml")
            .build()
    }

    // Warnings do not stop an update. The run sends them as a validation result,
    // as a create does, and saves.
    #[tokio::test]
    async fn an_update_that_warns_is_saved_and_reports_the_warning() {
        let (result, results) =
            update_description_of(package_warning_about_its_environment(), 1, Ok(())).await;

        assert!(
            matches!(result, OperationResult::Success(_)),
            "got: {result:?}"
        );
        assert_eq!(results.len(), 1, "got: {results:?}");
        assert_eq!(results[0].package_name, "test-pkg");
        assert_eq!(results[0].environment, "test-env");
        assert_eq!(results[0].outcome, Outcome::Found, "got: {results:?}");
        assert!(
            results[0].issues.iter().any(|issue| {
                matches!(issue.level, ValidationLevel::Warning)
                    && issue
                        .message
                        .contains("Current environment 'test-env' is not configured")
            }),
            "got: {results:?}"
        );
    }

    // The report is sent before the save, so a save that fails still leaves the
    // caller with what validation said.
    #[tokio::test]
    async fn the_warning_is_reported_before_the_save() {
        let failing_save = Err(PackageRepoError::IoError(std::sync::Arc::new(
            std::io::Error::other("disk full"),
        )));

        let (result, results) =
            update_description_of(package_warning_about_its_environment(), 1, failing_save).await;

        assert!(matches!(result, OperationResult::Failure(_)), "{result:?}");
        assert_eq!(results.len(), 1, "got: {results:?}");
    }

    // A notice alone is reported, with no warning beside it.
    #[tokio::test]
    async fn an_update_reports_a_notice_with_no_warning() {
        let package = PackageBuilder::default()
            .name("test-pkg")
            .environment("test-env", |b| b.install("true").check_some("  "))
            .path("/test/packages/test-pkg.yml")
            .build();

        let (result, results) = update_description_of(package, 1, Ok(())).await;

        assert!(
            matches!(result, OperationResult::Success(_)),
            "got: {result:?}"
        );
        assert_eq!(results.len(), 1, "got: {results:?}");
        assert_eq!(results[0].outcome, Outcome::Clean, "got: {results:?}");
        assert!(
            results[0].issues.iter().any(|issue| {
                matches!(issue.level, ValidationLevel::Info)
                    && issue.message.contains("check command is blank")
            }),
            "got: {results:?}"
        );
    }

    // The whole updated spec is judged, so what `spec validate` would say about a
    // part the update did not touch is reported too.
    #[tokio::test]
    async fn an_update_reports_a_notice_about_a_dotfile_it_did_not_touch() {
        let entry: crate::package::DotfileEntry =
            crate::yaml::parse("command: echo key\ntarget: ~/.key\n").expect("fixture must parse");
        let package = PackageBuilder::default()
            .name("test-pkg")
            .environment("test-env", |b| b.install("true"))
            .dotfiles(vec![entry])
            .path("/test/packages/test-pkg.yml")
            .origin(crate::package::SpecOrigin::PackageDirectory)
            .build();

        let (_, results) = update_description_of(package, 1, Ok(())).await;

        assert!(
            results
                .iter()
                .any(|result| result.issues.iter().any(|issue| {
                    matches!(issue.level, ValidationLevel::Info)
                        && issue.message.contains("executes 1 command(s)")
                })),
            "got: {results:?}"
        );
    }

    // A clean update reports nothing.
    #[tokio::test]
    async fn a_clean_update_reports_nothing() {
        let (result, results) =
            update_description_of(create_test_package("test-pkg"), 1, Ok(())).await;

        assert!(
            matches!(result, OperationResult::Success(_)),
            "got: {result:?}"
        );
        assert!(results.is_empty(), "got: {results:?}");
    }

    // A spec that already has an error does not stop an update that adds none: the
    // error is reported as the whole spec's and the update is saved.
    #[tokio::test]
    async fn an_error_the_spec_already_had_is_reported_and_does_not_stop_the_update() {
        let package = PackageBuilder::default()
            .name("test-pkg")
            .homepage("not a url")
            .environment("test-env", |b| b.install("true"))
            .path("/test/packages/test-pkg.yml")
            .build();

        let (result, results) = update_description_of(package, 1, Ok(())).await;

        assert!(
            matches!(result, OperationResult::Success(_)),
            "got: {result:?}"
        );
        assert_eq!(results.len(), 1, "got: {results:?}");
        assert_eq!(results[0].outcome, Outcome::Failed, "got: {results:?}");
        assert!(
            results[0]
                .issues
                .iter()
                .any(|issue| issue.field == "homepage"
                    && matches!(issue.level, ValidationLevel::Error)),
            "got: {results:?}"
        );
    }

    // An update that adds an error is refused even when the spec had another, and
    // nothing is reported or saved.
    #[tokio::test]
    async fn an_error_the_update_adds_stops_it_beside_one_the_spec_had() {
        let package = PackageBuilder::default()
            .name("test-pkg")
            .homepage("not a url")
            .environment("test-env", |b| b.install("true"))
            .path("/test/packages/test-pkg.yml")
            .build();
        let mut mock_repo = MockPackageRepository::new();
        let (sender, mut rx) = test_sender();
        let mut progress = ProgressTracker::new(3);
        let get_package =
            GetPackage::from_existing(package, PathBuf::from("/test/packages/test-pkg.yml"));
        mock_repo
            .expect_get_package()
            .return_once(move |_| Ok(get_package));
        mock_repo.expect_save_package().times(0);
        let fields = PackageUpdateFields {
            environment: Some("test-env".to_string()),
            dependencies: Some(vec![String::new()]),
            ..Default::default()
        };

        let result = handle_update(
            "test-pkg",
            fields,
            &mock_repo,
            &MockPackageRepository::new(),
            &test_config(),
            &sender,
            &mut progress,
        )
        .await;

        let OperationResult::Failure(OperationFailure::InvalidSpec { issues, .. }) = &result else {
            panic!("expected a refusal, got: {result:?}");
        };
        assert!(
            issues
                .iter()
                .any(|issue| issue.field.contains("dependencies")),
            "{issues:?}"
        );
        assert!(validation_results(&mut rx).is_empty());
    }

    // Run `fields` against `package` with a repository that must not save, and
    // return the failure the run ended in with whatever it reported first.
    async fn refused_update(
        package: crate::package::Package,
        fields: PackageUpdateFields,
    ) -> (OperationFailure, Vec<ValidationResultData>) {
        let mut mock_repo = MockPackageRepository::listing_nothing();
        let (sender, mut rx) = test_sender();
        let mut progress = ProgressTracker::new(3);
        let get_package =
            GetPackage::from_existing(package, PathBuf::from("/test/packages/test-pkg.yml"));
        mock_repo
            .expect_get_package()
            .return_once(move |_| Ok(get_package));
        mock_repo.expect_save_package().times(0);

        let result = handle_update(
            "test-pkg",
            fields,
            &mock_repo,
            &MockPackageRepository::listing_nothing(),
            &test_config(),
            &sender,
            &mut progress,
        )
        .await;

        let OperationResult::Failure(failure) = result else {
            panic!("expected a refusal, got: {result:?}");
        };
        (failure, validation_results(&mut rx))
    }

    // An update that writes a new invalid value into a field that was already
    // invalid is the update's own error, whatever the old value's message said.
    #[tokio::test]
    async fn a_new_invalid_value_in_an_already_invalid_field_is_refused() {
        let package = PackageBuilder::default()
            .name("test-pkg")
            .homepage("not a url")
            .environment("test-env", |b| b.install("true").dependencies(vec![""]))
            .path("/test/packages/test-pkg.yml")
            .build();
        let homepage = PackageUpdateFields {
            homepage: Some("also bad".to_string()),
            ..Default::default()
        };
        let dependencies = PackageUpdateFields {
            environment: Some("test-env".to_string()),
            dependencies: Some(vec![String::new(), "git".to_string()]),
            ..Default::default()
        };

        for fields in [homepage, dependencies] {
            let (failure, reported) = refused_update(package.clone(), fields).await;

            assert!(
                matches!(failure, OperationFailure::InvalidSpec { .. }),
                "got: {failure:?}"
            );
            assert!(reported.is_empty(), "got: {reported:?}");
        }
    }

    // An error the save would refuse anyway is refused up front with its
    // structured issues, whichever field the update wrote.
    #[tokio::test]
    async fn an_unknown_key_in_the_file_is_refused_with_its_issues_before_any_save() {
        let misspelled_dotfile_key = "name: test-pkg\nenvironments:\n  test-env:\n    install: \"true\"\n    \
             dotfiles:\n      - source: e.tpl\n        target: ~/.e\n        vasr: 1\n";
        let unknown_top_level_key =
            "name: test-pkg\nextra: 1\nenvironments:\n  test-env:\n    install: \"true\"\n";
        for (yaml, field) in [
            (misspelled_dotfile_key, "vasr"),
            (unknown_top_level_key, "extra"),
        ] {
            // Loaded from `yaml`, so the package carries the file's own keys.
            let mut package: crate::package::Package =
                crate::yaml::parse(yaml).expect("fixture must parse");
            package.set_source(
                PathBuf::from("/test/packages/test-pkg.yml"),
                yaml.to_string(),
                crate::package::SpecOrigin::PackageDirectory,
            );
            let fields = PackageUpdateFields {
                description: Some("New description".to_string()),
                ..Default::default()
            };

            let (failure, _) = refused_update(package, fields).await;

            let OperationFailure::InvalidSpec { issues, .. } = &failure else {
                panic!("expected a structured refusal, got: {failure:?}");
            };
            assert!(
                issues.iter().any(|issue| issue.field.contains(field)),
                "{field}: {issues:?}"
            );
        }
    }

    // The update of one field is not refused for an error in another part of the
    // environment it names: the install command is the only field it wrote.
    #[tokio::test]
    async fn an_install_update_is_saved_beside_an_error_in_a_dotfile_of_that_environment() {
        let yaml = "name: test-pkg\nenvironments:\n  test-env:\n    install: \"true\"\n    dotfiles:\n      \
                    - source: a\n        target: ~/.same\n      - source: b\n        target: ~/.same\n";
        let mut package: crate::package::Package =
            crate::yaml::parse(yaml).expect("fixture must parse");
        package.set_source(
            PathBuf::from("/test/packages/test-pkg.yml"),
            yaml.to_string(),
            crate::package::SpecOrigin::PackageDirectory,
        );
        let mut mock_repo = MockPackageRepository::listing_nothing();
        let (sender, mut rx) = test_sender();
        let mut progress = ProgressTracker::new(3);
        let get_package =
            GetPackage::from_existing(package, PathBuf::from("/test/packages/test-pkg.yml"));
        mock_repo
            .expect_get_package()
            .return_once(move |_| Ok(get_package));
        mock_repo
            .expect_save_package()
            .times(1)
            .returning(|_, _| Ok(()));
        let fields = PackageUpdateFields {
            environment: Some("test-env".to_string()),
            install: Some("echo new".to_string()),
            ..Default::default()
        };

        let result = handle_update(
            "test-pkg",
            fields,
            &mock_repo,
            &MockPackageRepository::listing_nothing(),
            &test_config(),
            &sender,
            &mut progress,
        )
        .await;

        assert!(
            matches!(result, OperationResult::Success(_)),
            "got: {result:?}"
        );
        let results = validation_results(&mut rx);
        assert_eq!(results.len(), 1, "got: {results:?}");
        assert_eq!(results[0].outcome, Outcome::Failed, "got: {results:?}");
        assert!(
            results[0]
                .issues
                .iter()
                .any(|issue| issue.field.contains("dotfiles")),
            "got: {results:?}"
        );
    }

    #[tokio::test]
    async fn removing_the_last_environment_is_refused() {
        let package = create_test_package("test-pkg");
        let fields = PackageUpdateFields {
            remove_environment: Some("test-env".to_string()),
            ..Default::default()
        };

        let (failure, reported) = refused_update(package, fields).await;

        let OperationFailure::InvalidSpec { issues, .. } = &failure else {
            panic!("expected a refusal, got: {failure:?}");
        };
        assert!(
            issues.iter().any(|issue| issue.field == "environments"),
            "{issues:?}"
        );
        assert!(reported.is_empty(), "got: {reported:?}");
    }

    #[tokio::test]
    async fn an_added_environment_with_an_empty_dependency_is_refused() {
        let fields = PackageUpdateFields {
            add_environment: Some(AddEnvironment {
                name: "added".to_string(),
                install: "true".to_string(),
                check: None,
                audit: None,
                dependencies: vec![String::new()],
                recommends: Vec::new(),
            }),
            ..Default::default()
        };

        let (failure, _) = refused_update(create_test_package("test-pkg"), fields).await;

        let OperationFailure::InvalidSpec { issues, .. } = &failure else {
            panic!("expected a refusal, got: {failure:?}");
        };
        assert!(
            issues
                .iter()
                .any(|issue| issue.field == "environments.added.dependencies[0]"),
            "{issues:?}"
        );
    }

    // An environment named `a` does not own the fields of one named `a.b`.
    #[test]
    fn a_written_environment_does_not_cover_one_whose_name_extends_it() {
        let mut written = WrittenFields::default();
        written.environment_field("a", "install");
        let environments = || ["a", "a.b"].into_iter();

        assert!(written.contains("environments.a.install", environments()));
        assert!(!written.contains("environments.a.b.install", environments()));
        assert!(!written.contains("environments.a.check", environments()));

        // The field of `a.b` is placed in `a.b`, the longer name.
        let mut written = WrittenFields::default();
        written.environment_field("a.b", "install");
        assert!(written.contains("environments.a.b.install", environments()));
        assert!(!written.contains("environments.a.install", environments()));
    }

    // Collection left alpha out, so the file's own collisions come from the spec
    // loaded for the update, not from the listing, and are reported, not refused.
    #[tokio::test]
    async fn an_update_whose_spec_collection_left_out_reports_rather_than_refuses() {
        let (config, repo, dotfiles_repo) =
            super::super::validate::tests::alpha_beside_gem_with_packages_unlisted(1);
        let (tx, mut rx) = mpsc::channel(64);
        let sender = EventSender::new_with_context(
            tx,
            crate::package::event::metadata::OperationType::PackageUpdate,
            "alpha".to_string(),
            "test".to_string(),
            OperationContext::default(),
        );
        let fields = PackageUpdateFields {
            description: Some("renamed".to_string()),
            ..Default::default()
        };

        let result = handle_update(
            "alpha",
            fields,
            &repo,
            &dotfiles_repo,
            &config,
            &sender,
            &mut ProgressTracker::new(3),
        )
        .await;

        assert!(matches!(result, OperationResult::Success(_)), "{result:?}");
        assert_eq!(
            super::super::validate::tests::reported_collisions(&mut rx),
            ["dotfiles[0].target"]
        );
    }

    // A spec whose origin names no spec directory cannot be placed among the others,
    // so it is compared with nothing and must not be written.
    #[tokio::test]
    async fn an_update_of_a_spec_that_cannot_be_placed_is_refused() {
        let entry: crate::package::DotfileEntry =
            crate::yaml::parse("source: rc\ntarget: ~/.rc\n").expect("fixture must parse");
        let package = PackageBuilder::default()
            .name("test-pkg")
            .environment("test-env", |b| b.install("true"))
            .dotfiles(vec![entry])
            .path("/test/packages/test-pkg.yml")
            .origin(crate::package::SpecOrigin::Memory)
            .build();
        let mut repo = MockPackageRepository::listing_nothing();
        repo.expect_get_package().return_once(move |_| {
            Ok(GetPackage::from_existing(
                package,
                PathBuf::from("/test/packages/test-pkg.yml"),
            ))
        });
        repo.expect_save_package().times(0);
        let (sender, _rx) = test_sender();
        let fields = PackageUpdateFields {
            description: Some("renamed".to_string()),
            ..Default::default()
        };

        let result = handle_update(
            "test-pkg",
            fields,
            &repo,
            &MockPackageRepository::listing_nothing(),
            &test_config(),
            &sender,
            &mut ProgressTracker::new(3),
        )
        .await;

        let OperationResult::Failure(OperationFailure::UnplacedSpec {
            action: crate::package::event::SpecWrite::Update,
            package_name,
            path,
        }) = result
        else {
            panic!("expected the update to be refused as unplaced, got: {result:?}");
        };
        assert_eq!(package_name, "test-pkg");
        assert_eq!(path, PathBuf::from("/test/packages/test-pkg.yml"));
    }

    // A spec with no dotfile entry cannot share a target, so an update lists no
    // directory: the mocks expect no listing, and one fails the test.
    #[tokio::test]
    async fn update_of_a_spec_with_no_entries_collects_nothing() {
        let mut repo = MockPackageRepository::new();
        let package = create_test_package("test-pkg");
        repo.expect_get_package().return_once(move |_| {
            Ok(GetPackage::from_existing(
                package,
                PathBuf::from("/test/packages/test-pkg.yml"),
            ))
        });
        repo.expect_save_package().times(1).returning(|_, _| Ok(()));
        repo.expect_list_packages().times(0);
        let mut dotfiles_repo = MockPackageRepository::new();
        dotfiles_repo.expect_list_packages().times(0);
        let (sender, _rx) = test_sender();
        let fields = PackageUpdateFields {
            description: Some("renamed".to_string()),
            ..Default::default()
        };

        let result = handle_update(
            "test-pkg",
            fields,
            &repo,
            &dotfiles_repo,
            &test_config(),
            &sender,
            &mut ProgressTracker::new(3),
        )
        .await;

        assert!(matches!(result, OperationResult::Success(_)), "{result:?}");
    }
}
