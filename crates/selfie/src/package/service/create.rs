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

    if let Ok(existing) = repo.get_package(&package_name) {
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
