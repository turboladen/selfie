//!
//! Helps break down the pieces of running the `package create` command.
//!

use crate::{
    config::SelfieConfig,
    package::{
        Package,
        event::{EventSender, OperationResult, OperationSuccess},
        port::PackageRepository,
        service::ProgressTracker,
    },
};

pub(super) async fn handle_create<PR>(
    package: Package,
    repo: &PR,
    config: &SelfieConfig,
    sender: &EventSender,
    progress: &mut ProgressTracker,
) -> OperationResult
where
    PR: PackageRepository,
{
    let package_name = package.name().to_string();

    // Step 1: Check if package already exists
    progress
        .next(sender, "Checking if package already exists")
        .await;

    // Only a package that is genuinely absent may be created. Any other answer
    // means a file is there -- unparsable, refused, or ambiguous between two
    // names -- and creating would write over it. `save_package`'s guards cannot
    // help here: the package being written was built in memory, so it carries no
    // top-level keys to refuse over (selfie-3p8a).
    match repo.get_package(&package_name) {
        Ok(existing) => {
            let error = crate::package::port::PackageError::PackageAlreadyExists {
                name: package_name,
                file_path: existing.file_path().to_path_buf(),
            };
            let error_msg = error.to_string();
            sender
                .send_warning(format!("Package already exists: {error_msg}"))
                .await;
            return OperationResult::Failure(error.into());
        }
        Err(e) if e.means_no_such_package() => {}
        Err(e) => {
            sender
                .send_warning(format!(
                    "Refusing to create '{package_name}': something is already there that selfie \
                     could not use, so creating would overwrite it. {e}"
                ))
                .await;
            return OperationResult::Failure(e.into());
        }
    }

    // Step 2: Save the package
    progress.next(sender, "Saving package file").await;

    let file_path = package.path().to_path_buf();

    if let Err(err) = repo.save_package(&package, &file_path) {
        return OperationResult::Failure(err.into());
    }

    sender
        .send_debug(format!(
            "Package '{}' saved to {}",
            package_name,
            file_path.display()
        ))
        .await;

    OperationResult::Success(OperationSuccess::package_created(
        package_name,
        file_path,
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
            PackageBuilder,
            event::{OperationContext, PackageEvent, metadata::OperationType},
            port::{
                MockPackageRepository, PackageError, PackageListError, PackageParseError,
                PackageRepoError,
            },
        },
    };
    use std::{path::PathBuf, sync::Arc};
    use tokio::sync::mpsc;

    fn test_sender() -> (EventSender, mpsc::Receiver<PackageEvent>) {
        let (tx, rx) = mpsc::channel(256);
        let sender = EventSender::new_with_context(
            tx,
            OperationType::PackageCreate,
            "myapp".to_string(),
            "test".to_string(),
            OperationContext::default(),
        );
        (sender, rx)
    }

    fn fixture() -> (tempfile::TempDir, crate::config::SelfieConfig, Package) {
        let temp = tempfile::TempDir::new().unwrap();
        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory(temp.path())
            .build();
        let path = temp.path().join("myapp.yml");
        let package = PackageBuilder::default()
            .name("myapp")
            .environment("test", |b| b.install("true"))
            .path(&path)
            .build();
        (temp, config, package)
    }

    // A real parse failure, not a synthesized one, so the fixture cannot drift
    // from what the repository actually returns.
    fn a_real_parse_error() -> PackageError {
        let source = serde_saphyr::from_str::<Package>("name: [unclosed")
            .expect_err("fixture must fail to parse");
        PackageError::ParseError {
            name: "myapp".to_string(),
            packages_path: PathBuf::from("/packages"),
            failed_file: PathBuf::from("/packages/myapp.yml"),
            source: PackageParseError::YamlParse {
                package_path: PathBuf::from("/packages/myapp.yml"),
                source: Arc::new(source),
            },
        }
    }

    // A file that is present but will not parse is not an absent package.
    // Creating over it destroys what the user wrote, and `save_package`'s guards
    // cannot catch it: the package being written was built in memory, so it has
    // no top-level keys to refuse over.
    #[tokio::test]
    async fn create_refuses_when_a_file_is_there_but_cannot_be_read() {
        let (_temp, config, package) = fixture();
        let (sender, _rx) = test_sender();
        let mut progress = ProgressTracker::new(2);

        let mut repo = MockPackageRepository::new();
        repo.expect_get_package()
            .returning(|_| Err(a_real_parse_error().into()));
        // The assertion that matters: nothing is written.
        repo.expect_save_package().times(0);

        let result = handle_create(package, &repo, &config, &sender, &mut progress).await;

        assert!(
            matches!(result, OperationResult::Failure(_)),
            "got: {result:?}"
        );
    }

    // The control. A genuinely absent package must still be creatable, or the
    // guard above has simply broken `spec create`.
    #[tokio::test]
    async fn create_still_writes_when_no_file_is_there() {
        let (_temp, config, package) = fixture();
        let (sender, _rx) = test_sender();
        let mut progress = ProgressTracker::new(2);

        let mut repo = MockPackageRepository::new();
        repo.expect_get_package().returning(|name| {
            Err(PackageError::PackageNotFound {
                name: name.to_string(),
                packages_path: PathBuf::from("/packages"),
                files_examined: 0,
                search_patterns: vec![],
            }
            .into())
        });
        repo.expect_save_package().times(1).returning(|_, _| Ok(()));

        let result = handle_create(package, &repo, &config, &sender, &mut progress).await;

        assert!(
            matches!(result, OperationResult::Success(_)),
            "a package with no file must still be created, got: {result:?}"
        );
    }

    // The second control. A package directory that does not exist yet holds
    // nothing to overwrite, and the save creates it -- refusing here would mean
    // no first package could be created on a fresh machine.
    #[tokio::test]
    async fn create_still_writes_when_the_package_directory_is_not_there() {
        let (_temp, config, package) = fixture();
        let (sender, _rx) = test_sender();
        let mut progress = ProgressTracker::new(2);

        let mut repo = MockPackageRepository::new();
        repo.expect_get_package().returning(|_| {
            Err(PackageRepoError::PackageListError(
                PackageListError::PackageDirectoryNotFound(PathBuf::from("/packages")),
            ))
        });
        repo.expect_save_package().times(1).returning(|_, _| Ok(()));

        let result = handle_create(package, &repo, &config, &sender, &mut progress).await;

        assert!(
            matches!(result, OperationResult::Success(_)),
            "a missing package directory is not a file to overwrite, got: {result:?}"
        );
    }
}
