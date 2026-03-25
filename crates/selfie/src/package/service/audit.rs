//! Helps break down the pieces of running the `package audit` command.

use super::steps;
use crate::{
    commands::runner::CommandRunner,
    config::SelfieConfig,
    package::{
        GetPackage,
        event::{AuditResult, AuditResultData, EventSender, OperationResult, OperationSuccess},
        port::PackageRepository,
        service::ProgressTracker,
    },
};
use tokio_util::sync::CancellationToken;

pub(super) async fn handle_audit<PR, CR>(
    package_name: &str,
    repo: &PR,
    config: &SelfieConfig,
    command_runner: &CR,
    sender: &EventSender,
    progress: &mut ProgressTracker,
    token: &CancellationToken,
) -> OperationResult
where
    PR: PackageRepository,
    CR: CommandRunner,
{
    // Step 1: Load package from repository
    let package_blob = match steps::fetch_package(repo, package_name, sender, progress).await {
        Ok(pkg) => pkg,
        Err(err) => return OperationResult::Failure(err.into()),
    };

    // Step 2: Get environment-specific audit command
    let (audit_command, dependencies) = match get_audit_command(
        package_name,
        &package_blob,
        config.environment(),
        sender,
        progress,
    )
    .await
    {
        Ok(result) => result,
        Err(result) => return result,
    };

    // Step 3: Execute the audit command
    let audit_result = execute_audit_command(
        package_name,
        config.environment(),
        audit_command.as_deref(),
        &dependencies,
        command_runner,
        sender,
        progress,
        token,
    )
    .await;

    // Send the audit result event
    sender.send_audit_result(audit_result.clone()).await;

    // Return appropriate operation result
    OperationResult::Success(OperationSuccess::package_audited(
        package_name.to_string(),
        config.environment().to_string(),
        audit_result.result,
        (progress.current_step(), progress.total_steps()).into(),
    ))
}

pub(super) async fn handle_audit_all<PR, CR>(
    repo: &PR,
    config: &SelfieConfig,
    command_runner: &CR,
    sender: &EventSender,
    progress: &mut ProgressTracker,
    token: &CancellationToken,
) -> OperationResult
where
    PR: PackageRepository,
    CR: CommandRunner,
{
    // Step 1: List all packages
    progress.next(sender, "Loading all packages").await;

    let packages = match repo.list_packages() {
        Ok(pkgs) => pkgs,
        Err(err) => return OperationResult::Failure(err.into()),
    };

    let package_names: Vec<String> = packages
        .valid_packages()
        .filter(|p| p.environments().contains_key(config.environment()))
        .map(|p| p.name().to_string())
        .collect();

    let total_packages = package_names.len();
    let max_concurrent = config.max_concurrency().get();

    // Audit packages concurrently in chunks, each with its own progress tracker.
    for chunk in package_names.chunks(max_concurrent) {
        if token.is_cancelled() {
            return OperationResult::Failure("Operation cancelled".into());
        }

        let futures: Vec<_> = chunk
            .iter()
            .map(|package_name| async move {
                if token.is_cancelled() {
                    return;
                }

                // Each concurrent audit gets its own progress tracker (3 steps)
                let mut pkg_progress = ProgressTracker::new(3);

                let package_blob =
                    match steps::fetch_package(repo, package_name, sender, &mut pkg_progress).await
                    {
                        Ok(pkg) => pkg,
                        Err(_) => {
                            let audit_result = AuditResultData {
                                package_name: package_name.to_string(),
                                environment: config.environment().to_string(),
                                audit_command: None,
                                result: AuditResult::Error("Failed to load package".to_string()),
                            };
                            sender.send_audit_result(audit_result).await;
                            return;
                        }
                    };

                let (audit_command, dependencies) = match get_audit_command(
                    package_name,
                    &package_blob,
                    config.environment(),
                    sender,
                    &mut pkg_progress,
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => {
                        let audit_result = AuditResultData {
                            package_name: package_name.to_string(),
                            environment: config.environment().to_string(),
                            audit_command: None,
                            result: AuditResult::Error(format!(
                                "Environment '{}' not configured for package '{package_name}'",
                                config.environment()
                            )),
                        };
                        sender.send_audit_result(audit_result).await;
                        return;
                    }
                };

                let audit_result = execute_audit_command(
                    package_name,
                    config.environment(),
                    audit_command.as_deref(),
                    &dependencies,
                    command_runner,
                    sender,
                    &mut pkg_progress,
                    token,
                )
                .await;

                sender.send_audit_result(audit_result).await;
            })
            .collect();

        futures::future::join_all(futures).await;
    }

    OperationResult::Success(OperationSuccess::Generic(format!(
        "Audit completed for {total_packages} package(s)",
    )))
}

async fn get_audit_command(
    package_name: &str,
    package_blob: &GetPackage,
    current_env: &str,
    sender: &EventSender,
    progress: &mut ProgressTracker,
) -> Result<(Option<String>, Vec<String>), OperationResult> {
    progress.next(sender, "Checking package environment").await;

    // Get environment configuration
    let Some(env_config) = package_blob.package().environments().get(current_env) else {
        return steps::handle_missing_environment(package_name, package_blob, current_env);
    };

    let dependencies = env_config.dependencies().to_vec();

    // Get audit command from environment (it's optional)
    match env_config.audit() {
        Some(audit_cmd) => {
            sender
                .send_debug(format!(
                    "Found audit command for environment '{current_env}': {audit_cmd}"
                ))
                .await;
            Ok((Some(audit_cmd.to_string()), dependencies))
        }
        None => {
            sender
                .send_debug(format!(
                    "No audit command defined for environment '{current_env}'"
                ))
                .await;
            Ok((None, dependencies))
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_audit_command<CR>(
    package_name: &str,
    environment: &str,
    audit_command: Option<&str>,
    dependencies: &[String],
    command_runner: &CR,
    sender: &EventSender,
    progress: &mut ProgressTracker,
    token: &CancellationToken,
) -> AuditResultData
where
    CR: CommandRunner,
{
    progress.next(sender, "Running audit command").await;

    let Some(cmd) = audit_command else {
        return AuditResultData {
            package_name: package_name.to_string(),
            environment: environment.to_string(),
            audit_command: None,
            result: AuditResult::NoAuditCommand,
        };
    };

    match command_runner.execute(cmd, token).await {
        Ok(output) => {
            if output.is_success() {
                let stdout = output.stdout_str().to_string();
                let sources: Vec<String> = stdout
                    .lines()
                    .map(|line| line.trim().to_string())
                    .filter(|line| !line.is_empty())
                    .collect();

                if sources.is_empty() {
                    AuditResultData {
                        package_name: package_name.to_string(),
                        environment: environment.to_string(),
                        audit_command: Some(cmd.to_string()),
                        result: AuditResult::NotInstalled,
                    }
                } else {
                    // Cross-reference sources with dependencies + package name
                    let expected: Vec<String> = dependencies
                        .iter()
                        .chain(std::iter::once(&package_name.to_string()))
                        .cloned()
                        .collect();

                    let has_unexpected = sources
                        .iter()
                        .any(|source| !expected.iter().any(|exp| source.eq_ignore_ascii_case(exp)));

                    if has_unexpected {
                        AuditResultData {
                            package_name: package_name.to_string(),
                            environment: environment.to_string(),
                            audit_command: Some(cmd.to_string()),
                            result: AuditResult::Conflicts { sources, expected },
                        }
                    } else {
                        AuditResultData {
                            package_name: package_name.to_string(),
                            environment: environment.to_string(),
                            audit_command: Some(cmd.to_string()),
                            result: AuditResult::Clean { sources },
                        }
                    }
                }
            } else {
                // Non-zero exit code = Error
                let stderr = output.stderr_str().to_string();
                let exit_code = output.exit_code();
                AuditResultData {
                    package_name: package_name.to_string(),
                    environment: environment.to_string(),
                    audit_command: Some(cmd.to_string()),
                    result: AuditResult::Error(format!(
                        "Audit command failed (exit code {exit_code}): {stderr}"
                    )),
                }
            }
        }
        Err(err) => AuditResultData {
            package_name: package_name.to_string(),
            environment: environment.to_string(),
            audit_command: Some(cmd.to_string()),
            result: AuditResult::Error(err.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        commands::runner::{CommandOutput, MockCommandRunner},
        config::SelfieConfigBuilder,
        package::{
            GetPackage, PackageBuilder,
            event::{AuditResult, OperationResult, OperationSuccess},
            port::MockPackageRepository,
        },
    };
    use std::os::unix::process::ExitStatusExt;
    use std::process::Output;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    fn mock_command_output(stdout: &str, success: bool) -> CommandOutput {
        let exit_code = if success { 0 } else { 1 };
        CommandOutput {
            output: Output {
                status: std::process::ExitStatus::from_raw(exit_code * 256),
                stdout: stdout.as_bytes().to_vec(),
                stderr: Vec::new(),
            },
            duration: std::time::Duration::from_millis(10),
        }
    }

    fn test_config(dir: &std::path::Path) -> crate::config::SelfieConfig {
        SelfieConfigBuilder::default()
            .environment("test")
            .package_directory(dir)
            .build()
    }

    fn test_sender() -> (
        EventSender,
        mpsc::Receiver<crate::package::event::PackageEvent>,
    ) {
        let (tx, rx) = mpsc::channel(32);
        let sender = EventSender::new_with_context(
            tx,
            crate::package::event::metadata::OperationType::PackageAudit,
            "test-pkg".to_string(),
            "test".to_string(),
            crate::package::event::OperationContext::default(),
        );
        (sender, rx)
    }

    #[tokio::test]
    async fn test_handle_audit_no_audit_command() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = test_config(temp_dir.path());

        let package = PackageBuilder::default()
            .name("test-pkg")
            .environment("test", |b| {
                b.install("echo install").check_some("echo check")
            })
            .path(temp_dir.path().join("test-pkg.yml"))
            .build();

        let mut mock_repo = MockPackageRepository::new();
        mock_repo.expect_get_package().returning(move |_| {
            Ok(GetPackage::from_existing(
                package.clone(),
                temp_dir.path().join("test-pkg.yml"),
            ))
        });

        let mock_runner = MockCommandRunner::new();

        let (sender, _rx) = test_sender();
        let mut progress = ProgressTracker::new(3);
        let token = CancellationToken::new();

        let result = handle_audit(
            "test-pkg",
            &mock_repo,
            &config,
            &mock_runner,
            &sender,
            &mut progress,
            &token,
        )
        .await;

        match result {
            OperationResult::Success(OperationSuccess::PackageAudited { audit_result, .. }) => {
                assert!(matches!(audit_result, AuditResult::NoAuditCommand));
            }
            other => panic!("Expected PackageAudited success, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_handle_audit_clean_result() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = test_config(temp_dir.path());

        let package = PackageBuilder::default()
            .name("test-pkg")
            .environment("test", |b| {
                b.install("echo install")
                    .check_some("echo check")
                    .audit_some("echo test-pkg")
            })
            .path(temp_dir.path().join("test-pkg.yml"))
            .build();

        let pkg_clone = package.clone();
        let pkg_path = temp_dir.path().join("test-pkg.yml");

        let mut mock_repo = MockPackageRepository::new();
        mock_repo.expect_get_package().returning(move |_| {
            Ok(GetPackage::from_existing(
                pkg_clone.clone(),
                pkg_path.clone(),
            ))
        });

        let mut mock_runner = MockCommandRunner::new();
        mock_runner
            .expect_execute()
            .returning(|_, _| Box::pin(async { Ok(mock_command_output("test-pkg\n", true)) }));

        let (sender, _rx) = test_sender();
        let mut progress = ProgressTracker::new(3);
        let token = CancellationToken::new();

        let result = handle_audit(
            "test-pkg",
            &mock_repo,
            &config,
            &mock_runner,
            &sender,
            &mut progress,
            &token,
        )
        .await;

        match result {
            OperationResult::Success(OperationSuccess::PackageAudited { audit_result, .. }) => {
                assert!(matches!(audit_result, AuditResult::Clean { .. }));
            }
            other => panic!("Expected PackageAudited clean, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_handle_audit_empty_output_means_not_installed() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = test_config(temp_dir.path());

        let package = PackageBuilder::default()
            .name("test-pkg")
            .environment("test", |b| {
                b.install("echo install")
                    .check_some("echo check")
                    .audit_some("echo ''")
            })
            .path(temp_dir.path().join("test-pkg.yml"))
            .build();

        let pkg_clone = package.clone();
        let pkg_path = temp_dir.path().join("test-pkg.yml");

        let mut mock_repo = MockPackageRepository::new();
        mock_repo.expect_get_package().returning(move |_| {
            Ok(GetPackage::from_existing(
                pkg_clone.clone(),
                pkg_path.clone(),
            ))
        });

        let mut mock_runner = MockCommandRunner::new();
        mock_runner
            .expect_execute()
            .returning(|_, _| Box::pin(async { Ok(mock_command_output("", true)) }));

        let (sender, _rx) = test_sender();
        let mut progress = ProgressTracker::new(3);
        let token = CancellationToken::new();

        let result = handle_audit(
            "test-pkg",
            &mock_repo,
            &config,
            &mock_runner,
            &sender,
            &mut progress,
            &token,
        )
        .await;

        match result {
            OperationResult::Success(OperationSuccess::PackageAudited { audit_result, .. }) => {
                assert!(matches!(audit_result, AuditResult::NotInstalled));
            }
            other => panic!("Expected PackageAudited NotInstalled, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_handle_audit_error_on_nonzero_exit() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = test_config(temp_dir.path());

        let package = PackageBuilder::default()
            .name("test-pkg")
            .environment("test", |b| {
                b.install("echo install")
                    .check_some("echo check")
                    .audit_some("false")
            })
            .path(temp_dir.path().join("test-pkg.yml"))
            .build();

        let pkg_clone = package.clone();
        let pkg_path = temp_dir.path().join("test-pkg.yml");

        let mut mock_repo = MockPackageRepository::new();
        mock_repo.expect_get_package().returning(move |_| {
            Ok(GetPackage::from_existing(
                pkg_clone.clone(),
                pkg_path.clone(),
            ))
        });

        let mut mock_runner = MockCommandRunner::new();
        mock_runner
            .expect_execute()
            .returning(|_, _| Box::pin(async { Ok(mock_command_output("", false)) }));

        let (sender, _rx) = test_sender();
        let mut progress = ProgressTracker::new(3);
        let token = CancellationToken::new();

        let result = handle_audit(
            "test-pkg",
            &mock_repo,
            &config,
            &mock_runner,
            &sender,
            &mut progress,
            &token,
        )
        .await;

        match result {
            OperationResult::Success(OperationSuccess::PackageAudited { audit_result, .. }) => {
                assert!(matches!(audit_result, AuditResult::Error(_)));
            }
            other => panic!("Expected PackageAudited Error, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_handle_audit_case_insensitive_matching() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = test_config(temp_dir.path());

        let package = PackageBuilder::default()
            .name("test-pkg")
            .environment("test", |b| {
                b.install("echo install")
                    .check_some("echo check")
                    .audit_some("echo Bun")
                    .dependencies(vec!["bun"])
            })
            .path(temp_dir.path().join("test-pkg.yml"))
            .build();

        let pkg_clone = package.clone();
        let pkg_path = temp_dir.path().join("test-pkg.yml");

        let mut mock_repo = MockPackageRepository::new();
        mock_repo.expect_get_package().returning(move |_| {
            Ok(GetPackage::from_existing(
                pkg_clone.clone(),
                pkg_path.clone(),
            ))
        });

        let mut mock_runner = MockCommandRunner::new();
        mock_runner
            .expect_execute()
            .returning(|_, _| Box::pin(async { Ok(mock_command_output("Bun\n", true)) }));

        let (sender, _rx) = test_sender();
        let mut progress = ProgressTracker::new(3);
        let token = CancellationToken::new();

        let result = handle_audit(
            "test-pkg",
            &mock_repo,
            &config,
            &mock_runner,
            &sender,
            &mut progress,
            &token,
        )
        .await;

        match result {
            OperationResult::Success(OperationSuccess::PackageAudited { audit_result, .. }) => {
                // "Bun" should match dependency "bun" case-insensitively → Clean
                assert!(
                    matches!(audit_result, AuditResult::Clean { .. }),
                    "Expected Clean for case-insensitive match, got: {audit_result:?}"
                );
            }
            other => panic!("Expected PackageAudited clean, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_handle_audit_all_happy_path() {
        use crate::package::port::ListPackagesOutput;

        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = test_config(temp_dir.path());

        let pkg1 = PackageBuilder::default()
            .name("pkg-a")
            .environment("test", |b| b.install("echo install").audit_some("echo bun"))
            .path(temp_dir.path().join("pkg-a.yml"))
            .build();

        let pkg2 = PackageBuilder::default()
            .name("pkg-b")
            .environment("test", |b| b.install("echo install"))
            .path(temp_dir.path().join("pkg-b.yml"))
            .build();

        let pkg1_clone = pkg1.clone();
        let pkg2_clone = pkg2.clone();
        let pkg1_path = temp_dir.path().join("pkg-a.yml");
        let pkg2_path = temp_dir.path().join("pkg-b.yml");

        let mut mock_repo = MockPackageRepository::new();

        // list_packages returns both packages
        mock_repo
            .expect_list_packages()
            .returning(move || Ok(ListPackagesOutput(vec![Ok(pkg1.clone()), Ok(pkg2.clone())])));

        // get_package returns the correct package for each name
        let p1 = pkg1_clone.clone();
        let p2 = pkg2_clone.clone();
        let p1_path = pkg1_path.clone();
        let p2_path = pkg2_path.clone();
        mock_repo.expect_get_package().returning(move |name| {
            if name == "pkg-a" {
                Ok(GetPackage::from_existing(p1.clone(), p1_path.clone()))
            } else {
                Ok(GetPackage::from_existing(p2.clone(), p2_path.clone()))
            }
        });

        let mut mock_runner = MockCommandRunner::new();
        // Only pkg-a has an audit command; pkg-b will get NoAuditCommand
        mock_runner
            .expect_execute()
            .returning(|_, _| Box::pin(async { Ok(mock_command_output("bun\n", true)) }));

        let (sender, mut rx) = test_sender();
        let mut progress = ProgressTracker::new(1);
        let token = CancellationToken::new();

        let result = handle_audit_all(
            &mock_repo,
            &config,
            &mock_runner,
            &sender,
            &mut progress,
            &token,
        )
        .await;

        assert!(
            matches!(result, OperationResult::Success(_)),
            "Expected success, got: {result:?}"
        );

        // Collect audit result events
        let mut audit_results = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let crate::package::event::PackageEvent::AuditResultCompleted {
                audit_result, ..
            } = event
            {
                audit_results.push(audit_result);
            }
        }

        assert_eq!(audit_results.len(), 2, "Should have 2 audit results");
    }

    #[tokio::test]
    async fn test_handle_audit_all_skips_packages_for_other_environments() {
        use crate::package::port::ListPackagesOutput;

        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = test_config(temp_dir.path());

        // Package only has "macos" environment, not "test" — should be excluded
        let pkg = PackageBuilder::default()
            .name("wrong-env-pkg")
            .environment("macos", |b| {
                b.install("brew install something").audit_some("echo brew")
            })
            .path(temp_dir.path().join("wrong-env-pkg.yml"))
            .build();

        let mut mock_repo = MockPackageRepository::new();
        mock_repo
            .expect_list_packages()
            .returning(move || Ok(ListPackagesOutput(vec![Ok(pkg.clone())])));

        let mock_runner = MockCommandRunner::new();

        let (sender, mut rx) = test_sender();
        let mut progress = ProgressTracker::new(1);
        let token = CancellationToken::new();

        let result = handle_audit_all(
            &mock_repo,
            &config,
            &mock_runner,
            &sender,
            &mut progress,
            &token,
        )
        .await;

        assert!(matches!(result, OperationResult::Success(_)));

        // No audit results should be emitted — the package was filtered out
        let mut audit_results = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let crate::package::event::PackageEvent::AuditResultCompleted {
                audit_result, ..
            } = event
            {
                audit_results.push(audit_result);
            }
        }

        assert_eq!(
            audit_results.len(),
            0,
            "Packages for other environments should be skipped"
        );
    }

    #[tokio::test]
    async fn test_handle_audit_conflicts() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = test_config(temp_dir.path());

        let package = PackageBuilder::default()
            .name("test-pkg")
            .environment("test", |b| {
                b.install("echo install")
                    .check_some("echo check")
                    .audit_some("echo unexpected-source")
            })
            .path(temp_dir.path().join("test-pkg.yml"))
            .build();

        let pkg_clone = package.clone();
        let pkg_path = temp_dir.path().join("test-pkg.yml");

        let mut mock_repo = MockPackageRepository::new();
        mock_repo.expect_get_package().returning(move |_| {
            Ok(GetPackage::from_existing(
                pkg_clone.clone(),
                pkg_path.clone(),
            ))
        });

        let mut mock_runner = MockCommandRunner::new();
        mock_runner.expect_execute().returning(|_, _| {
            Box::pin(async { Ok(mock_command_output("unexpected-source\n", true)) })
        });

        let (sender, _rx) = test_sender();
        let mut progress = ProgressTracker::new(3);
        let token = CancellationToken::new();

        let result = handle_audit(
            "test-pkg",
            &mock_repo,
            &config,
            &mock_runner,
            &sender,
            &mut progress,
            &token,
        )
        .await;

        match result {
            OperationResult::Success(OperationSuccess::PackageAudited { audit_result, .. }) => {
                assert!(matches!(audit_result, AuditResult::Conflicts { .. }));
            }
            other => panic!("Expected PackageAudited conflicts, got: {other:?}"),
        }
    }
}
