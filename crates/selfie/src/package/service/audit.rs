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
        Err(result) => return *result,
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

    // `valid_packages` drops a file that could not be loaded, and audit is the
    // command a user runs precisely to be told what selfie found. Reporting
    // nothing would mean an unreadable spec is audited by neither this run nor
    // the user, so it warns per file the way `validate_all` does.
    //
    // A warning rather than an error: whether `audit --all` exits non-zero on an
    // unparsable file is user-visible behavior and belongs to its own change.
    for invalid in packages.invalid_packages() {
        sender
            .send_warning(super::skipped_spec_warning(invalid))
            .await;
    }

    let package_names: Vec<String> = packages
        .valid_packages()
        .filter(|p| p.environments().contains_key(config.environment()))
        .map(|p| p.name().to_string())
        .collect();

    let total_packages = package_names.len();
    let max_concurrent = config.max_concurrency().get();

    // Audit packages concurrently in chunks, bounded by max_concurrency.
    // Uses chunks+join_all (not semaphore+spawn) because the function takes
    // borrowed references that can't move into 'static tokio tasks.
    for chunk in package_names.chunks(max_concurrent) {
        if token.is_cancelled() {
            return OperationResult::Failure("Operation cancelled".into());
        }

        let futures: Vec<_> = chunk
            .iter()
            .map(|name| audit_single_in_bulk(name, repo, config, command_runner, sender, token))
            .collect();

        futures::future::join_all(futures).await;
    }

    OperationResult::Success(OperationSuccess::Generic(format!(
        "Audit completed for {total_packages} package(s)",
    )))
}

/// Audit a single package within a bulk operation.
///
/// Errors are emitted as events rather than propagated, so the bulk
/// operation can continue with remaining packages.
async fn audit_single_in_bulk<PR, CR>(
    package_name: &str,
    repo: &PR,
    config: &SelfieConfig,
    command_runner: &CR,
    sender: &EventSender,
    token: &CancellationToken,
) where
    PR: PackageRepository,
    CR: CommandRunner,
{
    if token.is_cancelled() {
        return;
    }

    // Each concurrent audit gets its own progress tracker (3 steps: fetch + env check + audit)
    let mut pkg_progress = ProgressTracker::new(3);

    let package_blob =
        match steps::fetch_package(repo, package_name, sender, &mut pkg_progress).await {
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
}

async fn get_audit_command(
    package_name: &str,
    package_blob: &GetPackage,
    current_env: &str,
    sender: &EventSender,
    progress: &mut ProgressTracker,
) -> Result<(Option<String>, Vec<String>), Box<OperationResult>> {
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
                // Non-zero exit code = Error.
                //
                // Bounded because this message reaches the event stream and is
                // serialized straight into the MCP JSON an assistant reads
                // (`event_collector::audit_details`), so an unbounded stderr here
                // is unbounded egress. `AuditResult::Error` is a rendered
                // sentence rather than a stderr field, so it stays a `String` and
                // the bound is applied at this construction site.
                let stderr = crate::commands::BoundedText::bound(output.stderr());
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
        // Bounded for the same reason as the non-zero-exit arm above, and with
        // the same expression `resolve.rs`'s `run_capture` bounds: no
        // `CommandError` variant's `Display` prints process output, but every one
        // embeds the package file's own `command:` string, which is unbounded.
        // Treating two identical expressions differently is how the gap in the
        // arm above went unnoticed.
        Err(err) => AuditResultData {
            package_name: package_name.to_string(),
            environment: environment.to_string(),
            audit_command: Some(cmd.to_string()),
            result: AuditResult::Error(
                crate::commands::BoundedText::bound(err.to_string().as_bytes()).into_string(),
            ),
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
        let (tx, rx) = mpsc::channel(256);
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
    async fn a_failing_audit_commands_stderr_is_bounded() {
        // `AuditResult::Error` is serialized straight into the MCP JSON, so an
        // unbounded stderr here is unbounded egress to an assistant.
        //
        // Counts surviving bytes rather than checking for the marker: an
        // implementation that appends the marker without cutting passes a suffix
        // check, and that is exactly the defect the marker-only tests missed. The
        // fixture byte is 'Z' because the message prefix "Audit command failed
        // (exit code 1): " contains an 'x' -- counting 'x' reads one too many and
        // measures the prefix as well as the stderr.
        use crate::commands::runner::MAX_BOUNDED_BYTES;

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
        mock_runner.expect_execute().returning(|_, _| {
            Box::pin(async {
                Ok(CommandOutput {
                    output: Output {
                        status: std::process::ExitStatus::from_raw(256),
                        stdout: Vec::new(),
                        stderr: vec![b'Z'; MAX_BOUNDED_BYTES * 3],
                    },
                    duration: std::time::Duration::from_millis(10),
                })
            })
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
                match audit_result {
                    AuditResult::Error(message) => {
                        assert!(
                            message.contains("bytes elided"),
                            "not marked as elided: {message}"
                        );
                        assert_eq!(
                            message.chars().filter(|c| *c == 'Z').count(),
                            MAX_BOUNDED_BYTES,
                            "exactly the bound should survive"
                        );
                    }
                    other => panic!("Expected AuditResult::Error, got: {other:?}"),
                }
            }
            other => panic!("Expected PackageAudited, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_failing_audit_commands_error_message_is_bounded() {
        // The sibling of the test above, for the `Err(err)` arm rather than the
        // non-zero-exit arm. No `CommandError` variant's `Display` prints process
        // output, but every one embeds the package file's own `command:` string,
        // which is unbounded -- so this arm renders untrusted text too. Without
        // this test the bound on it is a line whose removal nothing notices.
        use crate::commands::runner::{CommandError, MAX_BOUNDED_BYTES};

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
        mock_runner.expect_execute().returning(|_, _| {
            Box::pin(async {
                Err(CommandError::Timeout {
                    command: "Z".repeat(MAX_BOUNDED_BYTES * 3),
                    timeout: std::time::Duration::from_secs(5),
                    working_directory: std::path::PathBuf::from("/pkg"),
                })
            })
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
                match audit_result {
                    AuditResult::Error(message) => {
                        assert!(
                            message.contains("bytes elided"),
                            "not marked as elided: {message}"
                        );
                        // 'Z' appears only in the oversized command string, so
                        // the count measures surviving input and nothing else.
                        // The rendered prefix eats into the head's share of the
                        // budget, which is why this is not simply the bound.
                        const PREFIX: &str = "Command timed out after 5s: ";
                        assert_eq!(
                            message.chars().filter(|c| *c == 'Z').count(),
                            MAX_BOUNDED_BYTES - PREFIX.len(),
                            "the head and tail together should come to the bound"
                        );
                    }
                    other => panic!("Expected AuditResult::Error, got: {other:?}"),
                }
            }
            other => panic!("Expected PackageAudited, got: {other:?}"),
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

    // `valid_packages` drops a file that could not be loaded, so the warning is
    // the only thing that names it.
    #[tokio::test]
    async fn test_handle_audit_all_warns_about_invalid_package_files() {
        use crate::package::port::{ListPackagesOutput, PackageParseError};

        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = test_config(temp_dir.path());

        let good = PackageBuilder::default()
            .name("good-pkg")
            .environment("test", |b| b.install("echo install").audit_some("echo bun"))
            .path(temp_dir.path().join("good-pkg.yml"))
            .build();
        let good_for_get = good.clone();
        let good_path = temp_dir.path().join("good-pkg.yml");
        let ghost_path = temp_dir.path().join("ghost.yml");

        let mut mock_repo = MockPackageRepository::new();
        let ghost_for_list = ghost_path.clone();
        mock_repo.expect_list_packages().returning(move || {
            Ok(ListPackagesOutput(vec![
                Ok(good.clone()),
                Err(PackageParseError::new(
                    ghost_for_list.clone(),
                    crate::package::port::PackageParseKind::IrregularFile {
                        kind: "named pipe (fifo)",
                    },
                )),
            ]))
        });
        mock_repo.expect_get_package().returning(move |_| {
            Ok(GetPackage::from_existing(
                good_for_get.clone(),
                good_path.clone(),
            ))
        });

        let mut mock_runner = MockCommandRunner::new();
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

        // The valid package is still audited. A "fix" that aborted the whole
        // run on an unreadable file would fail here rather than on the warning.
        assert!(
            matches!(result, OperationResult::Success(_)),
            "Expected success, got: {result:?}"
        );

        let mut warnings = Vec::new();
        let mut audit_results = 0;
        while let Ok(event) = rx.try_recv() {
            match event {
                crate::package::event::PackageEvent::Warning { message, .. } => {
                    warnings.push(message);
                }
                crate::package::event::PackageEvent::AuditResultCompleted { .. } => {
                    audit_results += 1;
                }
                _ => {}
            }
        }

        assert_eq!(audit_results, 1, "the readable package is still audited");

        let named = warnings
            .iter()
            .find(|w| w.contains("ghost.yml"))
            .unwrap_or_else(|| panic!("no warning named the invalid file; got: {warnings:?}"));
        assert!(named.contains("named pipe (fifo)"), "got: {named}");
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

    #[tokio::test]
    async fn test_audit_all_respects_concurrency_limit() {
        use crate::package::port::ListPackagesOutput;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Tracks peak concurrent command executions.
        struct ConcurrencyTrackingRunner {
            current: Arc<AtomicUsize>,
            peak: Arc<AtomicUsize>,
            delay: std::time::Duration,
        }

        impl ConcurrencyTrackingRunner {
            fn new(delay: std::time::Duration) -> Self {
                Self {
                    current: Arc::new(AtomicUsize::new(0)),
                    peak: Arc::new(AtomicUsize::new(0)),
                    delay,
                }
            }

            fn peak(&self) -> usize {
                self.peak.load(Ordering::SeqCst)
            }
        }

        impl crate::commands::runner::CommandRunner for ConcurrencyTrackingRunner {
            async fn is_command_available(&self, _command: &str) -> bool {
                true
            }

            async fn execute(
                &self,
                _command: &str,
                _token: &CancellationToken,
            ) -> Result<crate::commands::runner::CommandOutput, crate::commands::runner::CommandError>
            {
                let active = self.current.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(active, Ordering::SeqCst);
                tokio::time::sleep(self.delay).await;
                self.current.fetch_sub(1, Ordering::SeqCst);

                Ok(crate::commands::runner::CommandOutput {
                    output: std::process::Output {
                        status: std::os::unix::process::ExitStatusExt::from_raw(0),
                        stdout: b"test-source\n".to_vec(),
                        stderr: Vec::new(),
                    },
                    duration: self.delay,
                })
            }

            async fn execute_with_timeout(
                &self,
                command: &str,
                _timeout: std::time::Duration,
                token: &CancellationToken,
            ) -> Result<crate::commands::runner::CommandOutput, crate::commands::runner::CommandError>
            {
                self.execute(command, token).await
            }

            async fn execute_in_dir(
                &self,
                command: &str,
                _working_dir: &std::path::Path,
                _timeout: std::time::Duration,
                token: &CancellationToken,
            ) -> Result<crate::commands::runner::CommandOutput, crate::commands::runner::CommandError>
            {
                self.execute(command, token).await
            }

            async fn execute_streaming(
                &self,
                _command: &str,
                _timeout: std::time::Duration,
                _output_sender: tokio::sync::mpsc::Sender<crate::commands::runner::OutputChunk>,
                _token: &CancellationToken,
            ) -> Result<crate::commands::runner::CommandOutput, crate::commands::runner::CommandError>
            {
                unimplemented!("not needed for audit tests")
            }

            async fn execute_for_content(
                &self,
                _command: &str,
                _working_dir: &std::path::Path,
                _timeout: std::time::Duration,
                _token: &CancellationToken,
            ) -> Result<crate::commands::runner::ContentOutput, crate::commands::runner::CommandError>
            {
                unimplemented!("auditing runs no content commands")
            }
        }

        let max_concurrent = 2;
        let num_packages = 6;

        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory(temp_dir.path())
            .max_concurrency_unchecked(max_concurrent)
            .build();

        let packages: Vec<_> = (0..num_packages)
            .map(|i| {
                PackageBuilder::default()
                    .name(&format!("pkg-{i}"))
                    .environment("test", |b| {
                        b.install("echo install")
                            .audit_some(format!("echo pkg-{i}"))
                    })
                    .path(temp_dir.path().join(format!("pkg-{i}.yml")))
                    .build()
            })
            .collect();

        let mut mock_repo = MockPackageRepository::new();
        let packages_clone = packages.clone();
        mock_repo.expect_list_packages().returning(move || {
            Ok(ListPackagesOutput(
                packages_clone.iter().cloned().map(Ok).collect(),
            ))
        });

        // get_package returns the matching package
        let packages_for_get = packages.clone();
        let temp_path = temp_dir.path().to_path_buf();
        mock_repo.expect_get_package().returning(move |name| {
            let pkg = packages_for_get
                .iter()
                .find(|p| p.name() == name)
                .unwrap()
                .clone();
            Ok(crate::package::GetPackage::from_existing(
                pkg,
                temp_path.join(format!("{name}.yml")),
            ))
        });

        let runner = ConcurrencyTrackingRunner::new(std::time::Duration::from_millis(50));
        let (sender, _rx) = test_sender();
        let mut progress = ProgressTracker::new(1);
        let token = CancellationToken::new();

        let result =
            handle_audit_all(&mock_repo, &config, &runner, &sender, &mut progress, &token).await;

        assert!(
            matches!(result, OperationResult::Success(_)),
            "Expected success, got: {result:?}"
        );

        let peak_value = runner.peak();
        assert!(
            peak_value <= max_concurrent,
            "Peak concurrency {peak_value} exceeded limit {max_concurrent}"
        );
        assert!(
            peak_value > 1,
            "Expected some concurrency but peak was {peak_value}"
        );
    }
}
