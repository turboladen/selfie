// Integration tests for the dotfile service layer
//
// These tests verify dotfile deployment operations using real filesystem
// and repository implementations with temporary directories.
//
// Source paths resolve relative to the YAML file's parent directory:
// package dotfiles live in `packages/<name>/`, standalone ones in
// `dotfiles/<name>/`. That is why most tests create source files under
// `dirs.package_dir`.

use selfie::package::SpecOrigin;
use std::path::PathBuf;

use futures::StreamExt;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use test_common::FakeCommandRunner;

use selfie::{
    config::SelfieConfigBuilder,
    dotfile_service::{
        port::{ApplyOptions, DotfileService},
        service::DotfileServiceImpl,
        state::DeployState,
    },
    fs::RealFileSystem,
    package::{
        event::{OperationFailure, OperationResult, OperationSuccess, PackageEvent},
        repository::YamlPackageRepository,
    },
    privilege::{Elevation, Privilege, SudoPolicy},
};

// A service that believes it is running at a fixed privilege.
//
// Injected rather than read from the process, for the same reason `HomeAt`
// exists below: the real answer depends on how the suite was invoked, so a
// developer running `sudo cargo test` would otherwise fail every apply test.
#[derive(Clone, Copy, Debug)]
struct RunningAs(Elevation);

impl Privilege for RunningAs {
    fn elevation(&self) -> Elevation {
        self.0
    }
}

// Collect all events from an event stream
async fn collect_events(stream: selfie::package::event::EventStream) -> Vec<PackageEvent> {
    stream.collect::<Vec<_>>().await
}

// Extract the operation result from collected events
fn get_operation_result(events: &[PackageEvent]) -> Option<&OperationResult> {
    events.iter().find_map(|e| match e {
        PackageEvent::Completed { result, .. } => Some(result),
        _ => None,
    })
}

// Restores `mode` on the path when dropped, so a directory made read-only for a
// test can still be removed after an assertion panics.
struct RestoreMode(PathBuf, u32);

impl Drop for RestoreMode {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt as _;
        // Ignored on failure: panicking in `Drop` during an unwind aborts the test
        // binary, which would hide the assertion that started the unwind.
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(self.1));
    }
}

// Make `dir` unwritable, restoring its mode when the guard drops. `None` where the
// mode bits do not bite -- running as root, or a filesystem that ignores them --
// so a caller skips instead of asserting about a write that succeeded.
//
// Restores the mode the directory actually had, not a fixed one: `TempDir`
// creates at 0o700, so resetting to 0o755 would leave every caller's directory
// looser than it found it, and a fixture whose own subject is a mode would have
// this guard change it.

// The probe is removed again: it is written inside the directory under test, and
// one of the callers goes on to assert about that directory's contents.
#[cfg(unix)]
fn made_unwritable(dir: &std::path::Path) -> Option<RestoreMode> {
    use std::os::unix::fs::PermissionsExt as _;

    let original = std::fs::metadata(dir).unwrap().permissions().mode() & 0o7777;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let restore = RestoreMode(dir.to_path_buf(), original);
    let probe = dir.join("probe");
    if std::fs::write(&probe, "x").is_ok() {
        let _ = std::fs::remove_file(&probe);
        return None;
    }
    Some(restore)
}

// The message of a run that failed, for the operations that report one.
fn failure_message(events: &[PackageEvent]) -> String {
    match get_operation_result(events).expect("no Completed event") {
        OperationResult::Failure(failure) => failure.to_string(),
        other => panic!("expected a Failure, got {other:?}"),
    }
}

// A conflict resolver that counts how often it is asked and always answers
// `Accept`, so a run that reaches it both shows in the count and goes on to
// write.
struct Counting(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl selfie::dotfile_service::port::ConflictResolver for Counting {
    fn resolve(
        &self,
        _target: &str,
        _detail: selfie::dotfile_service::port::ConflictDetail<'_>,
    ) -> selfie::dotfile_service::port::ConflictResolution {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        selfie::dotfile_service::port::ConflictResolution::Accept
    }
}

// Every warning a run emitted.
fn warning_messages(events: &[PackageEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            PackageEvent::Warning { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

// The warnings that mention the dotfiles directory, ignoring case.
fn dotfiles_directory_warnings(events: &[PackageEvent]) -> Vec<String> {
    warning_messages(events)
        .into_iter()
        .filter(|message| {
            let lower = message.to_lowercase();
            lower.contains("dotfiles directory") || lower.contains("standalone dotfiles")
        })
        .collect()
}

// `(drift_count, total_count, refused_count)` of a drift check's completion.
fn drift_summary(events: &[PackageEvent]) -> (usize, usize, usize) {
    match get_operation_result(events) {
        Some(OperationResult::Success(OperationSuccess::DotfileDriftChecked {
            drift_count,
            total_count,
            refused_count,
            ..
        })) => (*drift_count, *total_count, *refused_count),
        other => panic!("expected a drift completion, got: {other:?}"),
    }
}

// Run `work` on a thread and runtime of its own, and give up after `deadline`:
// `None` for a timeout, and a panic in `work` raised again here.
//
// For a test whose failure mode is a read blocked on a fifo. That read cannot be
// cancelled, and a runtime whose worker is stuck in it never finishes dropping, so a
// `tokio::time::timeout` inside the test's own runtime fires and then hangs the test
// at teardown: the deadline turns a hang into a later hang, not a failure. Here the
// blocked thread is abandoned instead, and the process ends with the test binary.
fn within_deadline<T, Fut, F>(deadline: std::time::Duration, work: F) -> Option<T>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T>,
    T: Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let _ = tx.send(runtime.block_on(work()));
    });
    match rx.recv_timeout(deadline) {
        Ok(value) => Some(value),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
        // The work panicked, which dropped the sender: raise that panic here, so a
        // failing fixture reads as itself rather than as a hang.
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => match worker.join() {
            Err(panic) => std::panic::resume_unwind(panic),
            Ok(()) => unreachable!("the worker returned without sending"),
        },
    }
}

// Entries an apply was asked to deploy and declined, which is the counter that
// separates "selfie refused" from "there was nothing to do".
fn refused_count(events: &[PackageEvent]) -> usize {
    match get_operation_result(events).expect("no Completed event") {
        OperationResult::Success(OperationSuccess::DotfilesApplied { refused_count, .. }) => {
            *refused_count
        }
        other => panic!("expected DotfilesApplied, got {other:?}"),
    }
}

// Helper to create a package YAML file with a dotfiles section
fn create_package_with_dotfiles(
    package_dir: &std::path::Path,
    name: &str,
    dotfiles: &[(&str, &str)],
) -> PathBuf {
    let mut dotfiles_yaml = String::from("dotfiles:\n");
    for (source, target) in dotfiles {
        dotfiles_yaml.push_str(&format!(
            "  - source: \"{source}\"\n    target: \"{target}\"\n"
        ));
    }

    let yaml = format!(
        r#"name: {name}
environments:
  test:
    install: "echo installed"
{dotfiles_yaml}"#
    );

    let file_path = package_dir.join(format!("{name}.yml"));
    std::fs::write(&file_path, yaml).unwrap();
    file_path
}

// Write a package file from raw YAML, returning its path.
//
// The escape hatch from [`create_package_with_dotfiles`], which builds the
// `dotfiles:` key itself and so cannot express a file whose defect is in that
// key's *name* or in an entry's extra keys. A `PackageBuilder` fixture cannot
// stand in either: the builder never populates `raw_yaml`, so a test about
// top-level keys built that way passes without ever exercising the check.
fn write_package_yaml(package_dir: &std::path::Path, name: &str, yaml: &str) -> PathBuf {
    let file_path = package_dir.join(format!("{name}.yml"));
    std::fs::write(&file_path, yaml).unwrap();
    file_path
}

// Create standard test directories under a temp dir
struct TestDirs {
    _temp: TempDir,
    package_dir: PathBuf,
    dotfiles_dir: PathBuf,
    target_dir: PathBuf,
    state_dir: PathBuf,
    // What every service built from these dirs believes about its privilege,
    // and whether `--allow-sudo` was passed.
    sudo_policy: SudoPolicy<RunningAs>,
}

impl TestDirs {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let package_dir = temp.path().join("packages");
        let dotfiles_dir = temp.path().join("dotfiles");
        let target_dir = temp.path().join("target");
        let state_dir = temp.path().join("state");
        std::fs::create_dir_all(&package_dir).unwrap();
        std::fs::create_dir_all(&dotfiles_dir).unwrap();
        std::fs::create_dir_all(&target_dir).unwrap();
        std::fs::create_dir_all(&state_dir).unwrap();
        Self {
            _temp: temp,
            package_dir,
            dotfiles_dir,
            target_dir,
            state_dir,
            sudo_policy: SudoPolicy::new(RunningAs(Elevation::Unprivileged)),
        }
    }

    // Build every subsequent service as though the process were at `elevation`.
    fn running_as(mut self, elevation: Elevation) -> Self {
        self.sudo_policy = SudoPolicy::new(RunningAs(elevation));
        self
    }

    // As though `--allow-sudo` had been passed. Keeps whatever elevation is set.
    fn allowing_sudo(mut self) -> Self {
        self.sudo_policy = self.sudo_policy.allowing_sudo();
        self
    }

    // Create a service backed only by the packages directory.
    fn service(
        &self,
    ) -> DotfileServiceImpl<
        YamlPackageRepository<RealFileSystem>,
        RealFileSystem,
        FakeCommandRunner,
        RunningAs,
    > {
        self.service_with_runner(FakeCommandRunner::new())
    }

    // A packages-only service whose provider commands answer from `runner`.
    fn service_with_runner(
        &self,
        runner: FakeCommandRunner,
    ) -> DotfileServiceImpl<
        YamlPackageRepository<RealFileSystem>,
        RealFileSystem,
        FakeCommandRunner,
        RunningAs,
    > {
        self.service_with_runner_and_token(runner, CancellationToken::new())
    }

    // As [`service_with_runner`](Self::service_with_runner), but the caller
    // supplies the cancellation token and may use any runner type.
    //
    // Generic over the runner because the cancellation tests need one that
    // observes the token it is handed, which `FakeCommandRunner` deliberately
    // ignores.
    fn service_with_runner_and_token<CR>(
        &self,
        runner: CR,
        token: CancellationToken,
    ) -> DotfileServiceImpl<YamlPackageRepository<RealFileSystem>, RealFileSystem, CR, RunningAs>
    where
        CR: selfie::commands::CommandRunner + Clone + std::fmt::Debug + Send + Sync + 'static,
    {
        let fs = RealFileSystem;
        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory(&self.package_dir)
            .dotfiles_directory(self.dotfiles_dir.clone())
            .state_directory(self.state_dir.clone())
            .build();
        let repo = YamlPackageRepository::new(
            fs,
            config.package_directory().clone(),
            SpecOrigin::PackageDirectory,
        );
        DotfileServiceImpl::new(repo, fs, runner, config, token, self.sudo_policy)
    }

    // As [`service_with_runner`](Self::service_with_runner), but with a caller-supplied
    // file system, so a test can stage what the port answers.
    fn service_with_fs<F, CR>(
        &self,
        fs: F,
        runner: CR,
    ) -> DotfileServiceImpl<YamlPackageRepository<RealFileSystem>, F, CR, RunningAs>
    where
        F: selfie::fs::FileSystem + Clone + std::fmt::Debug + Send + Sync + 'static,
        CR: selfie::commands::CommandRunner + Clone + std::fmt::Debug + Send + Sync + 'static,
    {
        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory(&self.package_dir)
            .dotfiles_directory(self.dotfiles_dir.clone())
            .state_directory(self.state_dir.clone())
            .build();
        let repo = YamlPackageRepository::new(
            RealFileSystem,
            config.package_directory().clone(),
            SpecOrigin::PackageDirectory,
        );
        DotfileServiceImpl::new(
            repo,
            fs,
            runner,
            config,
            CancellationToken::new(),
            self.sudo_policy,
        )
    }

    // A packages-only service whose `stop_on_error` is set explicitly.
    //
    // The flag decides whether a refused entry ends the run, so a test about
    // that has to set both sides rather than rely on the default.
    fn service_with_runner_and_stop_on_error(
        &self,
        runner: FakeCommandRunner,
        stop_on_error: bool,
    ) -> DotfileServiceImpl<
        YamlPackageRepository<RealFileSystem>,
        RealFileSystem,
        FakeCommandRunner,
        RunningAs,
    > {
        let fs = RealFileSystem;
        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory(&self.package_dir)
            .dotfiles_directory(self.dotfiles_dir.clone())
            .state_directory(self.state_dir.clone())
            .stop_on_error(stop_on_error)
            .build();
        let repo = YamlPackageRepository::new(
            fs,
            config.package_directory().clone(),
            SpecOrigin::PackageDirectory,
        );
        DotfileServiceImpl::new(
            repo,
            fs,
            runner,
            config,
            CancellationToken::new(),
            self.sudo_policy,
        )
    }

    // Create a service backed by both `packages/` and `dotfiles/` directories.
    fn service_with_dotfiles(
        &self,
    ) -> DotfileServiceImpl<
        YamlPackageRepository<RealFileSystem>,
        RealFileSystem,
        FakeCommandRunner,
        RunningAs,
    > {
        let fs = RealFileSystem;
        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory(&self.package_dir)
            .dotfiles_directory(self.dotfiles_dir.clone())
            .state_directory(self.state_dir.clone())
            .build();
        let package_repo = YamlPackageRepository::new(
            fs,
            config.package_directory().clone(),
            SpecOrigin::PackageDirectory,
        );
        let dotfiles_repo = YamlPackageRepository::new(
            fs,
            self.dotfiles_dir.clone(),
            SpecOrigin::DotfilesDirectory,
        );
        DotfileServiceImpl::new(
            package_repo,
            fs,
            FakeCommandRunner::new(),
            config,
            CancellationToken::new(),
            self.sudo_policy,
        )
        .with_dotfiles_repository(dotfiles_repo)
    }

    // Both directories, with `dotfiles_directory` left unset. `dotfiles_dir` is
    // the sibling of `package_dir`, which is where the default resolves.
    fn service_with_default_dotfiles(
        &self,
    ) -> DotfileServiceImpl<
        YamlPackageRepository<RealFileSystem>,
        RealFileSystem,
        FakeCommandRunner,
        RunningAs,
    > {
        let fs = RealFileSystem;
        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory(&self.package_dir)
            .state_directory(self.state_dir.clone())
            .build();
        assert_eq!(
            config.dotfiles_directory(),
            self.dotfiles_dir,
            "the fixture's dotfiles directory must be the unset default"
        );
        let package_repo = YamlPackageRepository::new(
            fs,
            config.package_directory().clone(),
            SpecOrigin::PackageDirectory,
        );
        let dotfiles_repo = YamlPackageRepository::new(
            fs,
            self.dotfiles_dir.clone(),
            SpecOrigin::DotfilesDirectory,
        );
        DotfileServiceImpl::new(
            package_repo,
            fs,
            FakeCommandRunner::new(),
            config,
            CancellationToken::new(),
            self.sudo_policy,
        )
        .with_dotfiles_repository(dotfiles_repo)
    }

    // A service that believes the home directory is `home`.
    //
    // Injected rather than read from the environment: `$HOME` is process-wide
    // and these tests run in parallel.
    // A service whose filesystem cancels the token when `cancel_on` is read, so a
    // cancellation lands inside the run rather than before or after it.
    fn service_cancelling_on_read(
        &self,
        cancel_on: &std::path::Path,
        token: CancellationToken,
    ) -> DotfileServiceImpl<
        YamlPackageRepository<CancelOnReadOf>,
        CancelOnReadOf,
        FakeCommandRunner,
        RunningAs,
    > {
        let fs = CancelOnReadOf(RealFileSystem, cancel_on.to_path_buf(), token.clone());
        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory(&self.package_dir)
            .dotfiles_directory(self.dotfiles_dir.clone())
            .state_directory(self.state_dir.clone())
            .build();
        DotfileServiceImpl::new(
            YamlPackageRepository::new(
                fs.clone(),
                config.package_directory().clone(),
                SpecOrigin::PackageDirectory,
            ),
            fs,
            FakeCommandRunner::new(),
            config,
            token,
            self.sudo_policy,
        )
    }

    fn service_with_home(
        &self,
        home: &std::path::Path,
    ) -> DotfileServiceImpl<YamlPackageRepository<HomeAt>, HomeAt, FakeCommandRunner, RunningAs>
    {
        let fs = HomeAt(RealFileSystem, home.to_path_buf());
        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory(&self.package_dir)
            .dotfiles_directory(self.dotfiles_dir.clone())
            .state_directory(self.state_dir.clone())
            .build();
        DotfileServiceImpl::new(
            YamlPackageRepository::new(
                fs.clone(),
                config.package_directory().clone(),
                SpecOrigin::PackageDirectory,
            ),
            fs.clone(),
            FakeCommandRunner::new(),
            config,
            CancellationToken::new(),
            self.sudo_policy,
        )
        .with_dotfiles_repository(YamlPackageRepository::new(
            fs,
            self.dotfiles_dir.clone(),
            SpecOrigin::DotfilesDirectory,
        ))
    }
}

// Cancels a token the moment a named file is read, then behaves exactly like the
// real filesystem.
//
// Drift's event stream buffers, so a consumer cancelling on the first event it sees
// is already too late — the run has finished. Cancelling from inside the run, on the
// read that the first entry performs, is what places the cancellation between two
// entries, which is the position the guard has to hold.
#[derive(Clone, Debug)]
struct CancelOnReadOf(RealFileSystem, PathBuf, CancellationToken);

impl selfie::fs::FileSystem for CancelOnReadOf {
    // Delegated: this decorator's subject is when the token is canceled, not what is
    // at a directory path.
    fn directory_state(&self, path: &std::path::Path) -> selfie::fs::DirectoryState {
        self.0.directory_state(path)
    }

    fn read_file(&self, path: &std::path::Path) -> Result<String, selfie::fs::FileSystemError> {
        if path == self.1 {
            self.2.cancel();
        }
        self.0.read_file(path)
    }

    fn read_file_bytes(
        &self,
        path: &std::path::Path,
    ) -> Result<Vec<u8>, selfie::fs::FileSystemError> {
        self.0.read_file_bytes(path)
    }

    fn write_file_private(
        &self,
        path: &selfie::fs::TargetPath,
        data: &[u8],
    ) -> Result<(), selfie::fs::FileSystemError> {
        self.0.write_file_private(path, data)
    }

    fn write_file_no_follow(
        &self,
        path: &selfie::fs::TargetPath,
        data: &[u8],
    ) -> Result<(), selfie::fs::FileSystemError> {
        self.0.write_file_no_follow(path, data)
    }

    fn symlink_refusal(
        &self,
        path: &selfie::fs::TargetPath,
    ) -> Option<selfie::fs::FileSystemError> {
        self.0.symlink_refusal(path)
    }

    fn irregular_target_refusal(
        &self,
        path: &selfie::fs::TargetPath,
    ) -> Option<selfie::fs::FileSystemError> {
        self.0.irregular_target_refusal(path)
    }

    fn is_directory(
        &self,
        path: &selfie::fs::TargetPath,
    ) -> Result<bool, selfie::fs::FileSystemError> {
        self.0.is_directory(path)
    }

    fn is_owner_only(
        &self,
        path: &selfie::fs::TargetPath,
    ) -> Result<bool, selfie::fs::FileSystemError> {
        self.0.is_owner_only(path)
    }

    fn remove_file(&self, path: &std::path::Path) -> Result<(), selfie::fs::FileSystemError> {
        self.0.remove_file(path)
    }

    fn path_exists(&self, path: &std::path::Path) -> bool {
        self.0.path_exists(path)
    }

    fn expand_path(&self, path: &std::path::Path) -> Result<PathBuf, selfie::fs::FileSystemError> {
        self.0.expand_path(path)
    }

    fn list_directory(
        &self,
        path: &std::path::Path,
    ) -> Result<Vec<PathBuf>, selfie::fs::FileSystemError> {
        self.0.list_directory(path)
    }

    fn canonicalize(&self, path: &std::path::Path) -> Result<PathBuf, selfie::fs::FileSystemError> {
        self.0.canonicalize(path)
    }

    fn config_dir(&self) -> Result<PathBuf, selfie::fs::FileSystemError> {
        self.0.config_dir()
    }
}

// `RealFileSystem` with a chosen home directory and nothing else changed.
#[derive(Clone, Debug)]
struct HomeAt(RealFileSystem, PathBuf);

impl selfie::fs::FileSystem for HomeAt {
    fn is_directory(
        &self,
        path: &selfie::fs::TargetPath,
    ) -> Result<bool, selfie::fs::FileSystemError> {
        self.0.is_directory(path)
    }

    // Delegated: this decorator's subject is the home directory, not directory state.
    fn directory_state(&self, path: &std::path::Path) -> selfie::fs::DirectoryState {
        self.0.directory_state(path)
    }

    fn irregular_target_refusal(
        &self,
        path: &selfie::fs::TargetPath,
    ) -> Option<selfie::fs::FileSystemError> {
        self.0.irregular_target_refusal(path)
    }

    fn expand_path(&self, path: &std::path::Path) -> Result<PathBuf, selfie::fs::FileSystemError> {
        // Only a bare `~`. Anything else keeps the real behavior, so the
        // decorator cannot quietly change what the rest of track resolves.
        if path == std::path::Path::new("~") {
            return Ok(self.1.clone());
        }
        self.0.expand_path(path)
    }

    fn read_file(&self, path: &std::path::Path) -> Result<String, selfie::fs::FileSystemError> {
        self.0.read_file(path)
    }
    fn read_file_bytes(
        &self,
        path: &std::path::Path,
    ) -> Result<Vec<u8>, selfie::fs::FileSystemError> {
        self.0.read_file_bytes(path)
    }
    fn write_file_private(
        &self,
        path: &selfie::fs::TargetPath,
        data: &[u8],
    ) -> Result<(), selfie::fs::FileSystemError> {
        self.0.write_file_private(path, data)
    }
    fn write_file_no_follow(
        &self,
        path: &selfie::fs::TargetPath,
        data: &[u8],
    ) -> Result<(), selfie::fs::FileSystemError> {
        self.0.write_file_no_follow(path, data)
    }
    fn symlink_refusal(
        &self,
        path: &selfie::fs::TargetPath,
    ) -> Option<selfie::fs::FileSystemError> {
        self.0.symlink_refusal(path)
    }
    fn is_owner_only(
        &self,
        path: &selfie::fs::TargetPath,
    ) -> Result<bool, selfie::fs::FileSystemError> {
        self.0.is_owner_only(path)
    }
    fn remove_file(&self, path: &std::path::Path) -> Result<(), selfie::fs::FileSystemError> {
        self.0.remove_file(path)
    }
    fn path_exists(&self, path: &std::path::Path) -> bool {
        self.0.path_exists(path)
    }
    fn list_directory(
        &self,
        path: &std::path::Path,
    ) -> Result<Vec<PathBuf>, selfie::fs::FileSystemError> {
        self.0.list_directory(path)
    }
    fn canonicalize(&self, path: &std::path::Path) -> Result<PathBuf, selfie::fs::FileSystemError> {
        self.0.canonicalize(path)
    }
    fn config_dir(&self) -> Result<PathBuf, selfie::fs::FileSystemError> {
        self.0.config_dir()
    }
}

// `RealFileSystem` that reports no symlink at a path the first time it is asked and
// the truth afterwards, so a test can stage a link appearing between two checks.
//
// This is the window the deploy path's second `symlink_refusal` exists to narrow: the
// first answer is taken before the resolve runs, and a link planted during the resolve
// would otherwise be read through.
#[derive(Clone, Debug)]
struct SymlinkAppearsAfterFirstLook {
    inner: RealFileSystem,
    looks: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    dir_checks: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl selfie::fs::FileSystem for SymlinkAppearsAfterFirstLook {
    // Delegated: this decorator's subject is the symlink question's second answer, not
    // what is at a directory path.
    fn directory_state(&self, path: &std::path::Path) -> selfie::fs::DirectoryState {
        self.inner.directory_state(path)
    }

    fn is_directory(
        &self,
        path: &selfie::fs::TargetPath,
    ) -> Result<bool, selfie::fs::FileSystemError> {
        self.dir_checks
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.is_directory(path)
    }

    fn symlink_refusal(
        &self,
        path: &selfie::fs::TargetPath,
    ) -> Option<selfie::fs::FileSystemError> {
        use std::sync::atomic::Ordering::SeqCst;
        if self.looks.fetch_add(1, SeqCst) == 0 {
            return None;
        }
        self.inner.symlink_refusal(path)
    }

    fn read_file(&self, path: &std::path::Path) -> Result<String, selfie::fs::FileSystemError> {
        self.inner.read_file(path)
    }

    fn read_file_bytes(
        &self,
        path: &std::path::Path,
    ) -> Result<Vec<u8>, selfie::fs::FileSystemError> {
        self.inner.read_file_bytes(path)
    }

    fn write_file_private(
        &self,
        path: &selfie::fs::TargetPath,
        data: &[u8],
    ) -> Result<(), selfie::fs::FileSystemError> {
        self.inner.write_file_private(path, data)
    }

    fn write_file_no_follow(
        &self,
        path: &selfie::fs::TargetPath,
        data: &[u8],
    ) -> Result<(), selfie::fs::FileSystemError> {
        self.inner.write_file_no_follow(path, data)
    }

    fn irregular_target_refusal(
        &self,
        path: &selfie::fs::TargetPath,
    ) -> Option<selfie::fs::FileSystemError> {
        self.inner.irregular_target_refusal(path)
    }

    fn is_owner_only(
        &self,
        path: &selfie::fs::TargetPath,
    ) -> Result<bool, selfie::fs::FileSystemError> {
        self.inner.is_owner_only(path)
    }

    fn remove_file(&self, path: &std::path::Path) -> Result<(), selfie::fs::FileSystemError> {
        self.inner.remove_file(path)
    }

    fn path_exists(&self, path: &std::path::Path) -> bool {
        self.inner.path_exists(path)
    }

    fn expand_path(&self, path: &std::path::Path) -> Result<PathBuf, selfie::fs::FileSystemError> {
        self.inner.expand_path(path)
    }

    fn list_directory(
        &self,
        path: &std::path::Path,
    ) -> Result<Vec<PathBuf>, selfie::fs::FileSystemError> {
        self.inner.list_directory(path)
    }

    fn canonicalize(&self, path: &std::path::Path) -> Result<PathBuf, selfie::fs::FileSystemError> {
        self.inner.canonicalize(path)
    }

    fn config_dir(&self) -> Result<PathBuf, selfie::fs::FileSystemError> {
        self.inner.config_dir()
    }
}

// `RealFileSystem` that answers the second symlink question with a refusal this code
// does not interpret, so a test can check the re-ask fails closed rather than falling
// back to the answer taken before the resolve.
#[derive(Clone, Debug)]
struct SecondLookIsAnUnknownRefusal {
    inner: RealFileSystem,
    looks: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl selfie::fs::FileSystem for SecondLookIsAnUnknownRefusal {
    // Delegated: this decorator's subject is the second symlink answer, not what is at
    // a directory path.
    fn directory_state(&self, path: &std::path::Path) -> selfie::fs::DirectoryState {
        self.inner.directory_state(path)
    }

    fn symlink_refusal(
        &self,
        path: &selfie::fs::TargetPath,
    ) -> Option<selfie::fs::FileSystemError> {
        use std::sync::atomic::Ordering::SeqCst;
        if self.looks.fetch_add(1, SeqCst) == 0 {
            return None;
        }
        Some(selfie::fs::FileSystemError::IrregularTarget {
            path: path.path().to_path_buf(),
            kind: "character device",
        })
    }

    fn is_directory(
        &self,
        path: &selfie::fs::TargetPath,
    ) -> Result<bool, selfie::fs::FileSystemError> {
        self.inner.is_directory(path)
    }

    fn read_file(&self, path: &std::path::Path) -> Result<String, selfie::fs::FileSystemError> {
        self.inner.read_file(path)
    }

    fn read_file_bytes(
        &self,
        path: &std::path::Path,
    ) -> Result<Vec<u8>, selfie::fs::FileSystemError> {
        self.inner.read_file_bytes(path)
    }

    fn write_file_private(
        &self,
        path: &selfie::fs::TargetPath,
        data: &[u8],
    ) -> Result<(), selfie::fs::FileSystemError> {
        self.inner.write_file_private(path, data)
    }

    fn write_file_no_follow(
        &self,
        path: &selfie::fs::TargetPath,
        data: &[u8],
    ) -> Result<(), selfie::fs::FileSystemError> {
        self.inner.write_file_no_follow(path, data)
    }

    fn irregular_target_refusal(
        &self,
        path: &selfie::fs::TargetPath,
    ) -> Option<selfie::fs::FileSystemError> {
        self.inner.irregular_target_refusal(path)
    }

    fn is_owner_only(
        &self,
        path: &selfie::fs::TargetPath,
    ) -> Result<bool, selfie::fs::FileSystemError> {
        self.inner.is_owner_only(path)
    }

    fn remove_file(&self, path: &std::path::Path) -> Result<(), selfie::fs::FileSystemError> {
        self.inner.remove_file(path)
    }

    fn path_exists(&self, path: &std::path::Path) -> bool {
        self.inner.path_exists(path)
    }

    fn expand_path(&self, path: &std::path::Path) -> Result<PathBuf, selfie::fs::FileSystemError> {
        self.inner.expand_path(path)
    }

    fn list_directory(
        &self,
        path: &std::path::Path,
    ) -> Result<Vec<PathBuf>, selfie::fs::FileSystemError> {
        self.inner.list_directory(path)
    }

    fn canonicalize(&self, path: &std::path::Path) -> Result<PathBuf, selfie::fs::FileSystemError> {
        self.inner.canonicalize(path)
    }

    fn config_dir(&self) -> Result<PathBuf, selfie::fs::FileSystemError> {
        self.inner.config_dir()
    }
}

// `RealFileSystem` whose private writes to one path succeed a fixed number of
// times and fail afterwards, so a test can observe what was on disk between two
// writes of the deploy state.
#[derive(Clone, Debug)]
struct StateWritesFailAfter {
    inner: RealFileSystem,
    state_file: PathBuf,
    allowed: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl selfie::fs::FileSystem for StateWritesFailAfter {
    fn is_directory(
        &self,
        path: &selfie::fs::TargetPath,
    ) -> Result<bool, selfie::fs::FileSystemError> {
        self.inner.is_directory(path)
    }

    // Delegated: this decorator's subject is a failing write, not directory state.
    fn directory_state(&self, path: &std::path::Path) -> selfie::fs::DirectoryState {
        self.inner.directory_state(path)
    }

    fn write_file_private(
        &self,
        path: &selfie::fs::TargetPath,
        data: &[u8],
    ) -> Result<(), selfie::fs::FileSystemError> {
        use std::sync::atomic::Ordering::SeqCst;
        if path.path() == self.state_file
            && self
                .allowed
                .fetch_update(SeqCst, SeqCst, |left| left.checked_sub(1))
                .is_err()
        {
            return Err(selfie::fs::FileSystemError::IoError(std::sync::Arc::new(
                std::io::Error::other("state directory is not writable"),
            )));
        }
        self.inner.write_file_private(path, data)
    }

    fn irregular_target_refusal(
        &self,
        path: &selfie::fs::TargetPath,
    ) -> Option<selfie::fs::FileSystemError> {
        self.inner.irregular_target_refusal(path)
    }
    fn expand_path(&self, path: &std::path::Path) -> Result<PathBuf, selfie::fs::FileSystemError> {
        self.inner.expand_path(path)
    }
    fn read_file(&self, path: &std::path::Path) -> Result<String, selfie::fs::FileSystemError> {
        self.inner.read_file(path)
    }
    fn read_file_bytes(
        &self,
        path: &std::path::Path,
    ) -> Result<Vec<u8>, selfie::fs::FileSystemError> {
        self.inner.read_file_bytes(path)
    }
    fn write_file_no_follow(
        &self,
        path: &selfie::fs::TargetPath,
        data: &[u8],
    ) -> Result<(), selfie::fs::FileSystemError> {
        self.inner.write_file_no_follow(path, data)
    }
    fn symlink_refusal(
        &self,
        path: &selfie::fs::TargetPath,
    ) -> Option<selfie::fs::FileSystemError> {
        self.inner.symlink_refusal(path)
    }
    fn is_owner_only(
        &self,
        path: &selfie::fs::TargetPath,
    ) -> Result<bool, selfie::fs::FileSystemError> {
        self.inner.is_owner_only(path)
    }
    fn remove_file(&self, path: &std::path::Path) -> Result<(), selfie::fs::FileSystemError> {
        self.inner.remove_file(path)
    }
    fn path_exists(&self, path: &std::path::Path) -> bool {
        self.inner.path_exists(path)
    }
    fn list_directory(
        &self,
        path: &std::path::Path,
    ) -> Result<Vec<PathBuf>, selfie::fs::FileSystemError> {
        self.inner.list_directory(path)
    }
    fn canonicalize(&self, path: &std::path::Path) -> Result<PathBuf, selfie::fs::FileSystemError> {
        self.inner.canonicalize(path)
    }
    fn config_dir(&self) -> Result<PathBuf, selfie::fs::FileSystemError> {
        self.inner.config_dir()
    }
}

// The deploy state is written after each record, not once at the end of the run.
//
// Two entries in one package, in declared order, so which deploys first does
// not depend on directory listing. The filesystem allows exactly one write to
// the state file. Written once after the loop, that write would carry both
// entries and the run would report success; written after each record, the
// first entry's save takes it, the second's fails, and the run stops naming
// the target that went unrecorded while the first is already on disk.
#[tokio::test]
async fn state_is_recorded_after_each_deploy_not_after_the_run() {
    let dirs = TestDirs::new();
    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("a.toml"), "a = 1").unwrap();
    std::fs::write(source_dir.join("b.toml"), "b = 2").unwrap();
    let a = dirs.target_dir.join("a.toml");
    let b = dirs.target_dir.join("b.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[
            ("myapp/a.toml", a.to_str().unwrap()),
            ("myapp/b.toml", b.to_str().unwrap()),
        ],
    );

    let state_file = dirs.state_dir.join("deploy-state.yml");
    let fs = StateWritesFailAfter {
        inner: RealFileSystem,
        state_file: state_file.clone(),
        allowed: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(1)),
    };
    let config = SelfieConfigBuilder::default()
        .environment("test")
        .package_directory(&dirs.package_dir)
        .dotfiles_directory(dirs.dotfiles_dir.clone())
        .state_directory(dirs.state_dir.clone())
        .build();
    let service = DotfileServiceImpl::new(
        YamlPackageRepository::new(
            fs.clone(),
            config.package_directory().clone(),
            SpecOrigin::PackageDirectory,
        ),
        fs,
        FakeCommandRunner::new(),
        config,
        CancellationToken::new(),
        dirs.sudo_policy,
    );

    let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

    let message = failure_message(&events);
    assert!(
        message.contains("failing to record") && message.contains(b.to_str().unwrap()),
        "the run must stop and name the unrecorded target: {message}"
    );
    let warnings = warning_messages(&events);
    assert!(
        warnings.iter().any(|w| w.starts_with("Deployed")
            && w.contains(b.to_str().unwrap())
            && w.contains("deploy-state.yml")),
        "the warning must say the target was deployed and where it could not be recorded: {warnings:?}"
    );
    assert!(a.exists() && b.exists(), "both targets were deployed");

    let written = std::fs::read_to_string(&state_file).expect("the first save reached disk");
    let state: DeployState = selfie::yaml::parse(&written).expect("state file parses");
    assert!(
        state.get(a.to_str().unwrap()).is_some(),
        "the first deployment was not on disk before the second: {written}"
    );
    assert!(
        state.get(b.to_str().unwrap()).is_none(),
        "the second deployment reached disk through a save that was made to fail: {written}"
    );
}

#[tokio::test]
async fn test_apply_all_deploys_new_dotfile() {
    let dirs = TestDirs::new();

    // Create a dotfile source file
    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    let service = dirs.service();
    let stream = service.apply_all(ApplyOptions::default()).await;
    let events = collect_events(stream).await;

    let has_deploying = events
        .iter()
        .any(|e| matches!(e, PackageEvent::DotfileDeploying { .. }));
    let has_deployed = events
        .iter()
        .any(|e| matches!(e, PackageEvent::DotfileDeployed { .. }));
    assert!(has_deploying, "Should emit DotfileDeploying event");
    assert!(has_deployed, "Should emit DotfileDeployed event");

    assert!(target_file.exists(), "Target file should be created");
    let content = std::fs::read_to_string(&target_file).unwrap();
    assert_eq!(content, "key = \"value\"");

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfilesApplied {
            deployed_count,
            skipped_count,
            conflict_count,
            ..
        }) => {
            assert_eq!(*deployed_count, 1);
            assert_eq!(*skipped_count, 0);
            assert_eq!(*conflict_count, 0);
        }
        other => panic!("Expected DotfilesApplied success, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_apply_all_skips_when_up_to_date() {
    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    let service = dirs.service();

    // First apply
    let stream = service.apply_all(ApplyOptions::default()).await;
    let _ = collect_events(stream).await;

    // Second apply - should skip
    let stream = service.apply_all(ApplyOptions::default()).await;
    let events = collect_events(stream).await;

    let has_skipped = events
        .iter()
        .any(|e| matches!(e, PackageEvent::DotfileSkipped { .. }));
    assert!(
        has_skipped,
        "Should emit DotfileSkipped event on second apply"
    );

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfilesApplied {
            deployed_count,
            skipped_count,
            ..
        }) => {
            assert_eq!(*deployed_count, 0);
            assert_eq!(*skipped_count, 1);
        }
        other => panic!("Expected DotfilesApplied success, got: {other:?}"),
    }
}

// selfie-gr8z.3. A dry run accepts nothing, so `--yes` cannot turn a conflict into
// a skip. An accept would carry the entry to `perform_deploy`'s dry-run skip and
// report it as skipped, with no conflict event and no diff, leaving the summary at
// zero conflicts — and the preview someone runs to see what `--yes` would overwrite
// is the one place that count has to be right.
//
// Both halves are asserted: the conflict that must appear, and the "dry run" skip
// that must not. The event that must not appear is as much the fix as the one that
// must.
#[tokio::test]
async fn a_dry_run_reports_a_conflict_even_when_yes_would_accept_it() {
    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "from the repository\n").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    std::fs::write(&target_file, "hand edited on this machine\n").unwrap();
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    let service = dirs.service();
    let options = ApplyOptions {
        dry_run: true,
        auto_accept: true,
        ..Default::default()
    };
    let events = collect_events(service.apply_all(options).await).await;

    let conflict = events
        .iter()
        .find_map(|e| match e {
            PackageEvent::DotfileConflict { diff, .. } => Some(diff.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no conflict was reported: {events:?}"));
    assert!(
        !conflict.is_empty(),
        "the conflict carries no diff, which is what a real run would prompt on"
    );

    assert!(
        !events.iter().any(
            |e| matches!(e, PackageEvent::DotfileSkipped { reason, .. } if reason == "dry run")
        ),
        "the entry was reported as a dry-run skip as well as a conflict: {events:?}"
    );

    match get_operation_result(&events).expect("Should have a Completed event") {
        OperationResult::Success(OperationSuccess::DotfilesApplied {
            conflict_count,
            skipped_count,
            deployed_count,
            ..
        }) => {
            assert_eq!(*conflict_count, 1, "the conflict must be counted");
            assert_eq!(*skipped_count, 0, "it is not also a skip");
            assert_eq!(*deployed_count, 0, "a dry run deploys nothing");
        }
        other => panic!("expected DotfilesApplied, got: {other:?}"),
    }

    assert_eq!(
        std::fs::read_to_string(&target_file).unwrap(),
        "hand edited on this machine\n",
        "a dry run must not write"
    );
}

// The control for the commit above: outside a dry run, `--yes` still overwrites.
// Without this, refusing every auto-accept would pass the test above.
#[tokio::test]
async fn yes_still_overwrites_a_conflict_when_it_is_not_a_dry_run() {
    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "from the repository\n").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    std::fs::write(&target_file, "hand edited on this machine\n").unwrap();
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    let service = dirs.service();
    let options = ApplyOptions {
        auto_accept: true,
        ..Default::default()
    };
    let events = collect_events(service.apply_all(options).await).await;

    assert_eq!(
        std::fs::read_to_string(&target_file).unwrap(),
        "from the repository\n",
        "--yes must still overwrite outside a dry run"
    );
    match get_operation_result(&events).expect("Should have a Completed event") {
        OperationResult::Success(OperationSuccess::DotfilesApplied {
            deployed_count,
            conflict_count,
            ..
        }) => {
            assert_eq!(*deployed_count, 1);
            assert_eq!(*conflict_count, 0);
        }
        other => panic!("expected DotfilesApplied, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_apply_dry_run_does_not_write() {
    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    let service = dirs.service();
    let options = ApplyOptions {
        dry_run: true,
        ..Default::default()
    };
    let stream = service.apply_all(options).await;
    let events = collect_events(stream).await;

    let has_skipped_dry_run = events
        .iter()
        .any(|e| matches!(e, PackageEvent::DotfileSkipped { reason, .. } if reason == "dry run"));
    assert!(
        has_skipped_dry_run,
        "Should emit DotfileSkipped with 'dry run' reason"
    );

    let has_deploying = events
        .iter()
        .any(|e| matches!(e, PackageEvent::DotfileDeploying { .. }));
    assert!(
        !has_deploying,
        "Should NOT emit DotfileDeploying in dry run"
    );

    assert!(
        !target_file.exists(),
        "Target file should NOT exist in dry run"
    );

    // Verify completion counts: deployed should be 0, skipped should include the dry-run skip
    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfilesApplied {
            deployed_count,
            skipped_count,
            ..
        }) => {
            assert_eq!(*deployed_count, 0, "deployed_count should be 0 in dry run");
            assert_eq!(
                *skipped_count, 1,
                "skipped_count should include the dry-run skip"
            );
        }
        other => panic!("Expected DotfilesApplied success, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_apply_specific_package() {
    let dirs = TestDirs::new();

    let source_dir_a = dirs.package_dir.join("app-a");
    let source_dir_b = dirs.package_dir.join("app-b");
    std::fs::create_dir_all(&source_dir_a).unwrap();
    std::fs::create_dir_all(&source_dir_b).unwrap();
    std::fs::write(source_dir_a.join("a.conf"), "config-a").unwrap();
    std::fs::write(source_dir_b.join("b.conf"), "config-b").unwrap();

    let target_a = dirs.target_dir.join("a.conf");
    let target_b = dirs.target_dir.join("b.conf");

    create_package_with_dotfiles(
        &dirs.package_dir,
        "app-a",
        &[("app-a/a.conf", target_a.to_str().unwrap())],
    );
    create_package_with_dotfiles(
        &dirs.package_dir,
        "app-b",
        &[("app-b/b.conf", target_b.to_str().unwrap())],
    );

    let service = dirs.service();
    let stream = service.apply("app-a", ApplyOptions::default()).await;
    let events = collect_events(stream).await;

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfilesApplied { deployed_count, .. }) => {
            assert_eq!(*deployed_count, 1);
        }
        other => panic!("Expected DotfilesApplied success, got: {other:?}"),
    }

    assert!(target_a.exists(), "app-a dotfile should be deployed");
    assert!(!target_b.exists(), "app-b dotfile should NOT be deployed");
}

#[tokio::test]
async fn test_apply_conflict_detected() {
    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"new-value\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    let service = dirs.service();

    // First deploy
    let stream = service.apply_all(ApplyOptions::default()).await;
    let _ = collect_events(stream).await;

    // Modify target and source
    std::fs::write(&target_file, "key = \"user-modified\"").unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"updated-source\"").unwrap();

    // Apply again without auto_accept
    let stream = service.apply_all(ApplyOptions::default()).await;
    let events = collect_events(stream).await;

    let has_conflict = events
        .iter()
        .any(|e| matches!(e, PackageEvent::DotfileConflict { .. }));
    assert!(has_conflict, "Should emit DotfileConflict event");

    let content = std::fs::read_to_string(&target_file).unwrap();
    assert_eq!(content, "key = \"user-modified\"");
}

#[tokio::test]
async fn test_apply_conflict_auto_accept() {
    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"original\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    let service = dirs.service();

    // First deploy
    let stream = service.apply_all(ApplyOptions::default()).await;
    let _ = collect_events(stream).await;

    // Modify target and source
    std::fs::write(&target_file, "key = \"user-modified\"").unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"updated-source\"").unwrap();

    // Apply with auto_accept
    let options = ApplyOptions {
        auto_accept: true,
        ..Default::default()
    };
    let stream = service.apply_all(options).await;
    let events = collect_events(stream).await;

    let has_deployed = events
        .iter()
        .any(|e| matches!(e, PackageEvent::DotfileDeployed { .. }));
    assert!(has_deployed, "Should deploy with auto_accept");

    let content = std::fs::read_to_string(&target_file).unwrap();
    assert_eq!(content, "key = \"updated-source\"");
}

#[tokio::test]
async fn test_apply_conflict_resolver_accept() {
    use selfie::dotfile_service::port::{ConflictDetail, ConflictResolution, ConflictResolver};
    use std::sync::Arc;

    // A test resolver that always accepts conflicts.
    struct AlwaysAccept;
    impl ConflictResolver for AlwaysAccept {
        fn resolve(&self, _target: &str, _detail: ConflictDetail<'_>) -> ConflictResolution {
            ConflictResolution::Accept
        }
    }

    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"original\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    let service = dirs.service();

    // First deploy
    let stream = service.apply_all(ApplyOptions::default()).await;
    let _ = collect_events(stream).await;

    // Modify target and source to create a conflict
    std::fs::write(&target_file, "key = \"user-modified\"").unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"updated-source\"").unwrap();

    // Apply with a resolver that accepts
    let options = ApplyOptions {
        conflict_resolver: Some(Arc::new(AlwaysAccept)),
        ..Default::default()
    };
    let stream = service.apply_all(options).await;
    let events = collect_events(stream).await;

    // Should deploy (not emit a conflict event)
    let has_conflict = events
        .iter()
        .any(|e| matches!(e, PackageEvent::DotfileConflict { .. }));
    assert!(
        !has_conflict,
        "Should NOT emit DotfileConflict when resolver accepts"
    );

    let has_deployed = events
        .iter()
        .any(|e| matches!(e, PackageEvent::DotfileDeployed { .. }));
    assert!(has_deployed, "Should deploy when resolver accepts");

    let content = std::fs::read_to_string(&target_file).unwrap();
    assert_eq!(content, "key = \"updated-source\"");
}

#[tokio::test]
async fn test_apply_conflict_resolver_skip() {
    use selfie::dotfile_service::port::{ConflictDetail, ConflictResolution, ConflictResolver};
    use std::sync::Arc;

    // A test resolver that always skips conflicts.
    struct AlwaysSkip;
    impl ConflictResolver for AlwaysSkip {
        fn resolve(&self, _target: &str, _detail: ConflictDetail<'_>) -> ConflictResolution {
            ConflictResolution::Skip
        }
    }

    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"original\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    let service = dirs.service();

    // First deploy
    let stream = service.apply_all(ApplyOptions::default()).await;
    let _ = collect_events(stream).await;

    // Modify target and source to create a conflict
    std::fs::write(&target_file, "key = \"user-modified\"").unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"updated-source\"").unwrap();

    // Apply with a resolver that skips
    let options = ApplyOptions {
        conflict_resolver: Some(Arc::new(AlwaysSkip)),
        ..Default::default()
    };
    let stream = service.apply_all(options).await;
    let events = collect_events(stream).await;

    // Should NOT deploy, should emit conflict event (resolver returned Skip)
    let has_deployed = events
        .iter()
        .any(|e| matches!(e, PackageEvent::DotfileDeployed { .. }));
    assert!(!has_deployed, "Should NOT deploy when resolver skips");

    // Target should still have the user's content
    let content = std::fs::read_to_string(&target_file).unwrap();
    assert_eq!(content, "key = \"user-modified\"");
}

#[tokio::test]
async fn test_check_drift_detects_target_change() {
    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    let service = dirs.service();

    // First deploy
    let stream = service.apply_all(ApplyOptions::default()).await;
    let _ = collect_events(stream).await;

    // Modify target externally
    std::fs::write(&target_file, "key = \"modified\"").unwrap();

    // Check drift
    let stream = service.check_drift().await;
    let events = collect_events(stream).await;

    let has_drift = events
        .iter()
        .any(|e| matches!(e, PackageEvent::DotfileDriftDetected { .. }));
    assert!(has_drift, "Should detect drift after target modification");

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfileDriftChecked {
            drift_count,
            total_count,
            ..
        }) => {
            assert_eq!(*drift_count, 1);
            assert_eq!(*total_count, 1);
        }
        other => panic!("Expected DotfileDriftChecked success, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_check_drift_no_drift_when_up_to_date() {
    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    let service = dirs.service();

    // Deploy
    let stream = service.apply_all(ApplyOptions::default()).await;
    let _ = collect_events(stream).await;

    // Check drift - should be clean
    let stream = service.check_drift().await;
    let events = collect_events(stream).await;

    let has_drift = events
        .iter()
        .any(|e| matches!(e, PackageEvent::DotfileDriftDetected { .. }));
    assert!(!has_drift, "Should not detect drift when up to date");

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfileDriftChecked { drift_count, .. }) => {
            assert_eq!(*drift_count, 0);
        }
        other => panic!("Expected DotfileDriftChecked success, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_check_drift_missing_source_emits_warning() {
    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    let service = dirs.service();

    // First deploy
    let stream = service.apply_all(ApplyOptions::default()).await;
    let _ = collect_events(stream).await;

    // Delete the source file
    std::fs::remove_file(source_dir.join("config.toml")).unwrap();

    // Check drift — should emit a warning about missing source, not panic
    let stream = service.check_drift().await;
    let events = collect_events(stream).await;

    let has_warning = events
        .iter()
        .any(|e| matches!(e, PackageEvent::Warning { .. }));
    assert!(
        has_warning,
        "Should emit a warning when source file is missing during drift check"
    );

    // Should still complete successfully
    let result = get_operation_result(&events).expect("Should have a Completed event");
    assert!(
        matches!(
            result,
            OperationResult::Success(OperationSuccess::DotfileDriftChecked { .. })
        ),
        "Should still complete with DotfileDriftChecked even with missing source"
    );
}

#[tokio::test]
async fn test_apply_all_no_dotfiles_packages() {
    let dirs = TestDirs::new();

    let yaml = r#"name: no-config-pkg
environments:
  test:
    install: "echo installed"
"#;
    std::fs::write(dirs.package_dir.join("no-config-pkg.yml"), yaml).unwrap();

    let service = dirs.service();

    let stream = service.apply_all(ApplyOptions::default()).await;
    let events = collect_events(stream).await;

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfilesApplied {
            deployed_count,
            skipped_count,
            conflict_count,
            ..
        }) => {
            assert_eq!(*deployed_count, 0);
            assert_eq!(*skipped_count, 0);
            assert_eq!(*conflict_count, 0);
        }
        other => panic!("Expected DotfilesApplied success, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_apply_rejects_path_traversal() {
    let dirs = TestDirs::new();

    let target_file = dirs.target_dir.join("secret.txt");

    // Source uses "../" to escape the dotfiles directory — should be caught by
    // validate_source_path's normalize_path logic before any file I/O.
    create_package_with_dotfiles(
        &dirs.package_dir,
        "evil-pkg",
        &[("../../etc/passwd", target_file.to_str().unwrap())],
    );

    let service = dirs.service();
    let stream = service.apply_all(ApplyOptions::default()).await;
    let events = collect_events(stream).await;

    // Should get a warning specifically about path traversal
    let has_traversal_warning = events.iter().any(|e| {
        matches!(e, PackageEvent::Warning { message, .. } if message.contains("escapes YAML base directory"))
    });
    assert!(
        has_traversal_warning,
        "Should emit a warning about path escaping YAML base directory"
    );

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfilesApplied {
            deployed_count,
            skipped_count,
            refused_count,
            ..
        }) => {
            // Refused, not skipped: selfie was asked to deploy this and did not.
            // The `skipped_count == 0` half is the load-bearing one — it is what
            // fails if the two buckets are merged again (selfie-c28).
            assert_eq!(*refused_count, 1);
            assert_eq!(*skipped_count, 0);
            assert_eq!(*deployed_count, 0);
        }
        other => panic!("Expected DotfilesApplied success, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_apply_missing_source_warns_and_skips() {
    let dirs = TestDirs::new();

    let target_file = dirs.target_dir.join("config.toml");
    // Source file "nonexistent/config.toml" does not exist alongside the YAML
    create_package_with_dotfiles(
        &dirs.package_dir,
        "missing-src",
        &[("nonexistent/config.toml", target_file.to_str().unwrap())],
    );

    let service = dirs.service();
    let stream = service.apply_all(ApplyOptions::default()).await;
    let events = collect_events(stream).await;

    let has_warning = events
        .iter()
        .any(|e| matches!(e, PackageEvent::Warning { .. }));
    assert!(has_warning, "Should emit a warning about missing source");

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfilesApplied {
            deployed_count,
            skipped_count,
            refused_count,
            ..
        }) => {
            // Refused, not skipped: selfie was asked to deploy this and did not.
            // The `skipped_count == 0` half is the load-bearing one — it is what
            // fails if the two buckets are merged again (selfie-c28).
            assert_eq!(*refused_count, 1);
            assert_eq!(*skipped_count, 0);
            assert_eq!(*deployed_count, 0);
        }
        other => panic!("Expected DotfilesApplied success, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_apply_source_only_change_redeploys() {
    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"original\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    let service = dirs.service();

    // First deploy
    let stream = service.apply_all(ApplyOptions::default()).await;
    let _ = collect_events(stream).await;

    // Change ONLY the source file (target still matches last deploy)
    std::fs::write(source_dir.join("config.toml"), "key = \"updated\"").unwrap();

    // Apply again — should redeploy (RepoChanged drift), not conflict
    let stream = service.apply_all(ApplyOptions::default()).await;
    let events = collect_events(stream).await;

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfilesApplied {
            deployed_count,
            conflict_count,
            ..
        }) => {
            assert_eq!(*deployed_count, 1);
            assert_eq!(*conflict_count, 0);
        }
        other => panic!("Expected DotfilesApplied success, got: {other:?}"),
    }

    let content = std::fs::read_to_string(&target_file).unwrap();
    assert_eq!(content, "key = \"updated\"");
}

// The apply failed for a name no package answers, for this reason.
fn assert_no_such_package(
    events: &[PackageEvent],
    expected_name: &str,
    expected_reason: selfie::package::event::NoSuchPackageReason,
) {
    match get_operation_result(events) {
        Some(OperationResult::Failure(OperationFailure::NoSuchPackage { name, reason })) => {
            assert_eq!(name, expected_name);
            assert_eq!(*reason, expected_reason);
        }
        other => panic!("expected NoSuchPackage, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_apply_nonexistent_package_name() {
    let dirs = TestDirs::new();

    // Create a real package, but we'll apply a non-existent one
    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    let service = dirs.service();
    let stream = service.apply("no-such-pkg", ApplyOptions::default()).await;
    let events = collect_events(stream).await;

    // A success with every count at zero would read as "already up to date".
    assert_no_such_package(
        &events,
        "no-such-pkg",
        selfie::package::event::NoSuchPackageReason::NotFound,
    );

    assert!(
        !target_file.exists(),
        "Target file should NOT be deployed for non-matching package"
    );
}

// The package asked for may be a standalone dotfile in the directory selfie
// could not list, so "not found" alone would send the user looking for a typo.
#[tokio::test]
async fn apply_by_name_says_an_unlistable_directory_may_hold_the_package() {
    let Some((dirs, _target)) = dirs_with_an_unlistable_dotfiles_directory() else {
        eprintln!("SKIP apply_by_name_says_an_unlistable_directory_may_hold_the_package");
        return;
    };

    let events = collect_events(
        dirs.service_with_dotfiles()
            .apply("standalone", ApplyOptions::default())
            .await,
    )
    .await;

    assert_no_such_package(
        &events,
        "standalone",
        selfie::package::event::NoSuchPackageReason::MaybeInUnlistableDirectory,
    );
}

// A symlink loop at the dotfiles directory is not a directory that could not be
// listed, and the reason says so. "Could not be listed" asserts a directory is there
// holding entries selfie cannot see, which sends the user to look inside something
// that may not exist.
//
// The pair with the test above is the point: two states, two reasons. A single test
// asserting "some unreadable reason" would pass with both collapsed into one.
#[tokio::test]
async fn apply_by_name_says_an_unchecked_directory_is_unknown_rather_than_unlistable() {
    let dirs = TestDirs::new();
    std::fs::remove_dir_all(&dirs.dotfiles_dir).unwrap();
    std::os::unix::fs::symlink(&dirs.dotfiles_dir, &dirs.dotfiles_dir).unwrap();

    let events = collect_events(
        dirs.service_with_dotfiles()
            .apply("standalone", ApplyOptions::default())
            .await,
    )
    .await;

    assert_no_such_package(
        &events,
        "standalone",
        selfie::package::event::NoSuchPackageReason::MaybeInUncheckableDirectory,
    );
}

// A spec that failed to parse is dropped before the name is looked for, so "no
// package named" would send the user looking for a file that is there.
#[tokio::test]
async fn apply_by_name_says_an_unparsable_spec_could_not_be_loaded() {
    let dirs = TestDirs::new();
    write_package_yaml(&dirs.package_dir, "bat", "name: bat\nenvironments: [\n");

    let events = collect_events(dirs.service().apply("bat", ApplyOptions::default()).await).await;

    assert_no_such_package(
        &events,
        "bat",
        selfie::package::event::NoSuchPackageReason::NotLoaded,
    );
}

// A package is named by its file, as package lookup resolves it, so a `name:`
// field that differs from the file name does not hide the package from apply.
#[tokio::test]
async fn apply_by_name_matches_the_file_name_not_the_name_field() {
    let dirs = TestDirs::new();
    std::fs::write(dirs.package_dir.join("init.lua"), "-- nvim").unwrap();
    let target = dirs.target_dir.join("init.lua");
    write_package_yaml(
        &dirs.package_dir,
        "nvim",
        &format!(
            "name: neovim\nenvironments:\n  test:\n    install: \"true\"\ndotfiles:\n  - \
             source: init.lua\n    target: {}\n",
            target.display()
        ),
    );

    let events = collect_events(dirs.service().apply("nvim", ApplyOptions::default()).await).await;

    assert_eq!(refused_count(&events), 0, "events: {events:?}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "-- nvim");
}

// Package files are matched ignoring case, so apply accepts a name the other
// package commands accept.
#[tokio::test]
async fn apply_by_name_ignores_case() {
    let dirs = TestDirs::new();
    std::fs::write(dirs.package_dir.join("bat.conf"), "theme = dark").unwrap();
    let target = dirs.target_dir.join("bat.conf");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "bat",
        &[("bat.conf", target.to_str().unwrap())],
    );

    let events = collect_events(dirs.service().apply("BAT", ApplyOptions::default()).await).await;

    assert_eq!(refused_count(&events), 0, "events: {events:?}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "theme = dark");
}

// A spec's name is its file name with case folded, so `dotfiles/Bat.yml` is the
// same package as `packages/bat.yml`, and only the packages/ copy deploys.
#[tokio::test]
async fn a_name_in_both_directories_differing_in_case_deploys_only_the_packages_copy() {
    let dirs = TestDirs::new();
    std::fs::write(dirs.package_dir.join("bat.conf"), "from packages").unwrap();
    let packages_target = dirs.target_dir.join("packages-bat.conf");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "bat",
        &[("bat.conf", packages_target.to_str().unwrap())],
    );
    std::fs::write(dirs.dotfiles_dir.join("bat.conf"), "from dotfiles").unwrap();
    let dotfiles_target = dirs.target_dir.join("dotfiles-bat.conf");
    create_package_with_dotfiles(
        &dirs.dotfiles_dir,
        "Bat",
        &[("bat.conf", dotfiles_target.to_str().unwrap())],
    );

    let events = collect_events(
        dirs.service_with_dotfiles()
            .apply_all(ApplyOptions::default())
            .await,
    )
    .await;

    assert_eq!(
        std::fs::read_to_string(&packages_target).unwrap(),
        "from packages"
    );
    assert!(
        !dotfiles_target.exists(),
        "the dotfiles/ copy must not deploy; events: {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            PackageEvent::Warning { message, .. } if message.contains("Duplicate name 'bat'")
        )),
        "the collision must be reported; events: {events:?}"
    );
}

// A packages/ spec that failed to parse still claims its name. Deploying the
// dotfiles/ spec of that name in its place would apply a file the user did not
// mean, and applying the name says the packages/ spec could not be loaded.
#[tokio::test]
async fn an_unparsable_packages_spec_keeps_its_dotfiles_namesake_from_deploying() {
    let dirs = TestDirs::new();
    write_package_yaml(&dirs.package_dir, "bat", "name: bat\nenvironments: [\n");
    std::fs::write(dirs.dotfiles_dir.join("bat.conf"), "from dotfiles").unwrap();
    let dotfiles_target = dirs.target_dir.join("bat.conf");
    create_package_with_dotfiles(
        &dirs.dotfiles_dir,
        "bat",
        &[("bat.conf", dotfiles_target.to_str().unwrap())],
    );

    let all = collect_events(
        dirs.service_with_dotfiles()
            .apply_all(ApplyOptions::default())
            .await,
    )
    .await;

    assert!(
        !dotfiles_target.exists(),
        "the dotfiles/ copy must not deploy; events: {all:?}"
    );
    assert!(
        all.iter().any(|e| matches!(
            e,
            PackageEvent::Warning { message, .. }
                if message.contains("Not using 'bat' from dotfiles/")
        )),
        "the skipped namesake must be reported; events: {all:?}"
    );

    let named = collect_events(
        dirs.service_with_dotfiles()
            .apply("bat", ApplyOptions::default())
            .await,
    )
    .await;

    assert!(
        !dotfiles_target.exists(),
        "the dotfiles/ copy must not deploy; events: {named:?}"
    );
    assert_no_such_package(
        &named,
        "bat",
        selfie::package::event::NoSuchPackageReason::NotLoaded,
    );
}

#[tokio::test]
async fn test_deploy_state_persists_across_service_instances() {
    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    // Deploy with first service instance
    let service1 = dirs.service();
    let stream = service1.apply_all(ApplyOptions::default()).await;
    let _ = collect_events(stream).await;

    // Create a FRESH service instance and apply again
    let service2 = dirs.service();
    let stream = service2.apply_all(ApplyOptions::default()).await;
    let events = collect_events(stream).await;

    // Should skip (up to date), proving state was read from disk
    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfilesApplied { skipped_count, .. }) => {
            assert_eq!(*skipped_count, 1);
        }
        other => panic!("Expected DotfilesApplied success, got: {other:?}"),
    }
}

// A state file in the shape selfie wrote before entries were keyed by target is
// refused like any other unparsable file, with the repair named. Drift warns
// and carries on; apply refuses and leaves the file as it was.
//
// The cost of that refusal is one run: once the file is moved aside, the next
// apply records every target whose content already matches without a prompt.
#[tokio::test]
async fn a_source_keyed_state_file_is_refused_with_the_repair_named() {
    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    std::fs::write(&target_file, "key = \"value\"").unwrap();
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    let checksum = selfie::dotfile_service::deploy::compute_checksum(b"key = \"value\"");
    let old_shape = format!(
        "deployed:\n  myapp/config.toml:\n    target: {}\n    source_checksum: {checksum}\n    \
         deployed_checksum: {checksum}\n    deployed_at: \"2026-01-01T00:00:00+00:00\"\n",
        target_file.display()
    );
    let state_file = dirs.state_dir.join("deploy-state.yml");
    std::fs::write(&state_file, &old_shape).unwrap();

    let events = collect_events(dirs.service().check_drift().await).await;
    let warnings = warning_messages(&events);
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("Cannot parse deploy state") && w.contains("move it aside")),
        "drift must report the old shape as unparsable and name the repair: {warnings:?}"
    );

    let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;
    let message = failure_message(&events);
    assert!(
        message.contains("Cannot parse deploy state") && message.contains("move it aside"),
        "apply must refuse over the old shape and name the repair: {message}"
    );
    assert_eq!(
        std::fs::read_to_string(&state_file).unwrap(),
        old_shape,
        "the old-shape state file was written over"
    );

    // The one-time cost: with the file gone, the matching target is recorded
    // through the skip arm and nothing is deployed or asked about.
    std::fs::remove_file(&state_file).unwrap();
    let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;
    match get_operation_result(&events).expect("no Completed event") {
        OperationResult::Success(OperationSuccess::DotfilesApplied {
            deployed_count,
            skipped_count,
            conflict_count,
            ..
        }) => {
            assert_eq!(
                (*deployed_count, *skipped_count, *conflict_count),
                (0, 1, 0)
            );
        }
        other => panic!("expected the matching target to be skipped and recorded, got {other:?}"),
    }
    let written = std::fs::read_to_string(&state_file).expect("state file rewritten");
    let state: DeployState = selfie::yaml::parse(&written).expect("new shape parses");
    assert!(
        state.get(target_file.to_str().unwrap()).is_some(),
        "the target was not recorded under its own path: {written}"
    );
}

// One source deployed to two targets is two records, because each target has
// its own file on disk and its own checksum.
#[tokio::test]
async fn one_source_deployed_to_two_targets_records_both() {
    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("shell");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("rc"), "alias ll='ls -l'\n").unwrap();

    let zshrc = dirs.target_dir.join(".zshrc");
    let bashrc = dirs.target_dir.join(".bashrc");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "shell",
        &[
            ("shell/rc", zshrc.to_str().unwrap()),
            ("shell/rc", bashrc.to_str().unwrap()),
        ],
    );

    let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;
    match get_operation_result(&events).expect("no Completed event") {
        OperationResult::Success(OperationSuccess::DotfilesApplied { deployed_count, .. }) => {
            assert_eq!(*deployed_count, 2);
        }
        other => panic!("expected both targets to deploy, got {other:?}"),
    }

    let written =
        std::fs::read_to_string(dirs.state_dir.join("deploy-state.yml")).expect("state file");
    let state: DeployState = selfie::yaml::parse(&written).expect("state file parses");
    assert_eq!(
        state.entries().len(),
        2,
        "two targets from one source collapsed into one record: {written}"
    );
    for target in [&zshrc, &bashrc] {
        let entry = state
            .get(target.to_str().unwrap())
            .unwrap_or_else(|| panic!("{} was not recorded: {written}", target.display()));
        assert_eq!(entry.source(), "shell/rc");
    }
}

// A spec the collection could not parse leaves part of the check undone, and
// `unloaded_specs` is what a caller reads to learn that. The count comes from
// the same warnings the run relays, so the two cannot disagree.
#[tokio::test]
async fn check_drift_counts_a_spec_it_could_not_load() {
    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    std::fs::write(&target_file, "key = \"value\"").unwrap();
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );
    std::fs::write(dirs.package_dir.join("broken.yml"), "environments: {oops\n").unwrap();

    let events = collect_events(dirs.service().check_drift().await).await;

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfileDriftChecked {
            unloaded_specs,
            refused_count,
            ..
        }) => {
            assert_eq!(*unloaded_specs, 1, "events: {events:?}");
            // Not a refusal: nothing declined to act here, and the remedy is
            // the user's to apply to the file, so conflating the two would
            // point a caller at the wrong fix.
            assert_eq!(*refused_count, 0, "events: {events:?}");
        }
        other => panic!("Expected DotfileDriftChecked success, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_dry_run_does_not_persist_state() {
    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    // Dry-run apply
    let service1 = dirs.service();
    let options = ApplyOptions {
        dry_run: true,
        ..Default::default()
    };
    let stream = service1.apply_all(options).await;
    let _ = collect_events(stream).await;

    // Create fresh service and do a real apply
    let service2 = dirs.service();
    let stream = service2.apply_all(ApplyOptions::default()).await;
    let events = collect_events(stream).await;

    // Should deploy (not skip), proving dry run didn't write state
    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfilesApplied { deployed_count, .. }) => {
            assert_eq!(*deployed_count, 1);
        }
        other => panic!("Expected DotfilesApplied success, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_apply_multiple_dotfiles_in_one_package() {
    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value1\"").unwrap();
    std::fs::write(source_dir.join("settings.yml"), "setting: true").unwrap();

    let target_file1 = dirs.target_dir.join("config.toml");
    let target_file2 = dirs.target_dir.join("settings.yml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[
            ("myapp/config.toml", target_file1.to_str().unwrap()),
            ("myapp/settings.yml", target_file2.to_str().unwrap()),
        ],
    );

    let service = dirs.service();
    let stream = service.apply_all(ApplyOptions::default()).await;
    let events = collect_events(stream).await;

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfilesApplied { deployed_count, .. }) => {
            assert_eq!(*deployed_count, 2);
        }
        other => panic!("Expected DotfilesApplied success, got: {other:?}"),
    }

    assert!(target_file1.exists(), "First dotfile should be deployed");
    assert!(target_file2.exists(), "Second dotfile should be deployed");
}

#[tokio::test]
async fn test_apply_target_parent_dir_is_file() {
    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();

    // Create a regular file where the parent directory should be, making it
    // impossible to create the target path (a file can't also be a directory)
    let blocker = dirs.target_dir.join("not-a-dir");
    std::fs::write(&blocker, "I am a file, not a directory").unwrap();

    let target_file = blocker.join("config.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    let service = dirs.service();
    let stream = service.apply_all(ApplyOptions::default()).await;
    let events = collect_events(stream).await;

    let has_warning = events
        .iter()
        .any(|e| matches!(e, PackageEvent::Warning { .. }));
    assert!(
        has_warning,
        "Should emit a warning about write failure when parent path is a file"
    );

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfilesApplied {
            deployed_count,
            skipped_count,
            refused_count,
            ..
        }) => {
            // Refused, not skipped: selfie was asked to deploy this and did not.
            // The `skipped_count == 0` half is the load-bearing one — it is what
            // fails if the two buckets are merged again (selfie-c28).
            assert_eq!(*refused_count, 1);
            assert_eq!(*skipped_count, 0);
            assert_eq!(*deployed_count, 0);
        }
        other => panic!("Expected DotfilesApplied success, got: {other:?}"),
    }
}

// A state file selfie cannot parse is never written over: apply refuses before
// it deploys anything, so the file is still there to repair.
//
// The positive control runs the same package once the file is usable, so a
// refusal that fires for every state file, or a package that never deploys,
// cannot pass this.
#[tokio::test]
async fn apply_refuses_over_an_unparsable_state_file_and_leaves_it_untouched() {
    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    let state_file = dirs.state_dir.join("deploy-state.yml");
    let garbage = b"{{{{not valid yaml!!! garbage $$$";
    std::fs::write(&state_file, garbage).unwrap();

    let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

    let message = failure_message(&events);
    assert!(
        message.contains("Cannot parse deploy state") && message.contains("deploy-state.yml"),
        "the refusal must say the file could not be parsed and name it: {message}"
    );
    assert!(
        !target_file.exists(),
        "a dotfile was deployed by a run that could not record it"
    );
    assert_eq!(
        std::fs::read(&state_file).unwrap(),
        garbage,
        "the unparsable state file was written over"
    );

    // Control: the same package deploys once the state file is usable.
    std::fs::write(&state_file, "deployed: {}\n").unwrap();
    let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;
    match get_operation_result(&events).expect("no Completed event") {
        OperationResult::Success(OperationSuccess::DotfilesApplied { deployed_count, .. }) => {
            assert_eq!(*deployed_count, 1);
        }
        other => panic!("control: expected the package to deploy, got {other:?}"),
    }
    assert!(
        target_file.exists(),
        "control: the dotfile was not deployed"
    );
}

// A dry run writes nothing, so it previews against an empty state and warns
// rather than refusing; the file is left as it was.
#[tokio::test]
async fn a_dry_run_over_an_unparsable_state_file_warns_and_writes_nothing() {
    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    let state_file = dirs.state_dir.join("deploy-state.yml");
    let garbage = b"{{{{not valid yaml!!! garbage $$$";
    std::fs::write(&state_file, garbage).unwrap();

    let events = collect_events(
        dirs.service()
            .apply_all(ApplyOptions {
                dry_run: true,
                ..ApplyOptions::default()
            })
            .await,
    )
    .await;

    assert!(
        matches!(
            get_operation_result(&events),
            Some(OperationResult::Success(_))
        ),
        "a dry run must not refuse: {events:?}"
    );
    let warnings = warning_messages(&events);
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("Cannot parse deploy state")
                && w.contains("continuing as though nothing had been deployed")),
        "the dry run must say what it ignored: {warnings:?}"
    );
    assert!(!target_file.exists(), "a dry run wrote a dotfile");
    assert_eq!(
        std::fs::read(&state_file).unwrap(),
        garbage,
        "a dry run wrote the state file"
    );
}

#[tokio::test]
async fn test_check_drift_with_no_prior_deploys() {
    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    // Create target manually (not via selfie) so there's no deploy state
    std::fs::write(&target_file, "key = \"value\"").unwrap();

    let service = dirs.service();
    let stream = service.check_drift().await;
    let events = collect_events(stream).await;

    let has_drift = events
        .iter()
        .any(|e| matches!(e, PackageEvent::DotfileDriftDetected { .. }));
    assert!(
        has_drift,
        "Should detect drift when target exists but wasn't tracked"
    );

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfileDriftChecked { drift_count, .. }) => {
            assert_eq!(*drift_count, 1);
        }
        other => panic!("Expected DotfileDriftChecked success, got: {other:?}"),
    }
}

// ─── Dual-repository tests (packages/ + dotfiles/) ─────────────────────────

#[tokio::test]
async fn test_apply_deploys_from_both_packages_and_dotfiles_dirs() {
    let dirs = TestDirs::new();

    // Package dotfile: YAML + source in packages/
    let pkg_source_dir = dirs.package_dir.join("starship");
    std::fs::create_dir_all(&pkg_source_dir).unwrap();
    std::fs::write(pkg_source_dir.join("starship.toml"), "format = \"bold\"").unwrap();

    let pkg_target = dirs.target_dir.join("starship.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "starship",
        &[("starship/starship.toml", pkg_target.to_str().unwrap())],
    );

    // Standalone dotfile: YAML + source in dotfiles/
    let dot_source_dir = dirs.dotfiles_dir.join("dprint");
    std::fs::create_dir_all(&dot_source_dir).unwrap();
    std::fs::write(dot_source_dir.join("dprint.jsonc"), "{\"lineWidth\": 80}").unwrap();

    let dot_target = dirs.target_dir.join("dprint.jsonc");
    create_package_with_dotfiles(
        &dirs.dotfiles_dir,
        "dprint",
        &[("dprint/dprint.jsonc", dot_target.to_str().unwrap())],
    );

    // Use the dual-repo service
    let service = dirs.service_with_dotfiles();
    let stream = service.apply_all(ApplyOptions::default()).await;
    let events = collect_events(stream).await;

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfilesApplied { deployed_count, .. }) => {
            assert_eq!(*deployed_count, 2, "Should deploy from both repos");
        }
        other => panic!("Expected DotfilesApplied success, got: {other:?}"),
    }

    assert!(pkg_target.exists(), "Package dotfile should be deployed");
    assert!(dot_target.exists(), "Standalone dotfile should be deployed");
    assert_eq!(
        std::fs::read_to_string(&pkg_target).unwrap(),
        "format = \"bold\""
    );
    assert_eq!(
        std::fs::read_to_string(&dot_target).unwrap(),
        "{\"lineWidth\": 80}"
    );
}

#[tokio::test]
async fn test_apply_specific_name_finds_standalone_dotfile() {
    let dirs = TestDirs::new();

    // Only a standalone dotfile in dotfiles/, nothing in packages/
    let dot_source_dir = dirs.dotfiles_dir.join("dprint");
    std::fs::create_dir_all(&dot_source_dir).unwrap();
    std::fs::write(dot_source_dir.join("dprint.jsonc"), "{\"lineWidth\": 80}").unwrap();

    let dot_target = dirs.target_dir.join("dprint.jsonc");
    create_package_with_dotfiles(
        &dirs.dotfiles_dir,
        "dprint",
        &[("dprint/dprint.jsonc", dot_target.to_str().unwrap())],
    );

    let service = dirs.service_with_dotfiles();
    let stream = service.apply("dprint", ApplyOptions::default()).await;
    let events = collect_events(stream).await;

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfilesApplied { deployed_count, .. }) => {
            assert_eq!(*deployed_count, 1);
        }
        other => panic!("Expected DotfilesApplied success, got: {other:?}"),
    }

    assert!(dot_target.exists(), "Standalone dotfile should be deployed");
}

// A package spec must declare an environment, and a standalone dotfile spec must
// not have to. `selfie dotfiles track` writes the second kind with no
// environments at all, so a rule that asked the same of both would refuse every
// dotfile anyone has tracked.
//
// Every other standalone fixture in this file goes through
// `create_package_with_dotfiles`, which writes an `environments:` block. So this
// is the only test that fails when apply stops distinguishing the two, and the
// pair below is what makes the distinction rather than the absence itself
// observable: the same bytes, refused from the other directory.
mod a_spec_declaring_no_environment {
    use super::*;

    // What `dotfiles track` writes: a source, a target, and nothing else.
    fn write_spec(dir: &std::path::Path, target: &std::path::Path) {
        std::fs::create_dir_all(dir.join("gemrc")).unwrap();
        std::fs::write(dir.join("gemrc/.gemrc").as_path(), "GEM").unwrap();
        write_package_yaml(
            dir,
            "gemrc",
            &format!(
                "name: gemrc\ndotfiles:\n  - source: \"gemrc/.gemrc\"\n    target: \"{}\"\n",
                target.display()
            ),
        );
    }

    #[tokio::test]
    async fn deploys_from_the_dotfiles_directory() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join(".gemrc");
        write_spec(&dirs.dotfiles_dir, &target);

        let events = collect_events(
            dirs.service_with_dotfiles()
                .apply_all(ApplyOptions::default())
                .await,
        )
        .await;

        assert_eq!(
            refused_count(&events),
            0,
            "a tracked dotfile has no environments by design: {:?}",
            warning_messages(&events)
        );
        assert_eq!(
            std::fs::read_to_string(&target).ok().as_deref(),
            Some("GEM"),
            "the tracked dotfile must deploy: {:?}",
            warning_messages(&events)
        );
    }

    #[tokio::test]
    async fn is_refused_from_the_package_directory() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join(".gemrc");
        write_spec(&dirs.package_dir, &target);

        let events = collect_events(
            dirs.service_with_dotfiles()
                .apply_all(ApplyOptions::default())
                .await,
        )
        .await;

        assert!(
            !target.exists(),
            "a package spec that cannot be installed anywhere must deploy nothing"
        );
        assert!(
            warning_messages(&events)
                .iter()
                .any(|w| w.contains("Skipping package 'gemrc'")
                    && w.contains("environments' section")),
            "the refusal must name the package and what to add: {:?}",
            warning_messages(&events)
        );
    }
}

#[tokio::test]
async fn test_check_drift_covers_standalone_dotfiles() {
    let dirs = TestDirs::new();

    // Standalone dotfile in dotfiles/
    let dot_source_dir = dirs.dotfiles_dir.join("dprint");
    std::fs::create_dir_all(&dot_source_dir).unwrap();
    std::fs::write(dot_source_dir.join("dprint.jsonc"), "{\"lineWidth\": 80}").unwrap();

    let dot_target = dirs.target_dir.join("dprint.jsonc");
    create_package_with_dotfiles(
        &dirs.dotfiles_dir,
        "dprint",
        &[("dprint/dprint.jsonc", dot_target.to_str().unwrap())],
    );

    let service = dirs.service_with_dotfiles();

    // Deploy first
    let stream = service.apply_all(ApplyOptions::default()).await;
    let _ = collect_events(stream).await;

    // Modify the target externally
    std::fs::write(&dot_target, "{\"lineWidth\": 120}").unwrap();

    // Drift check should detect the standalone dotfile change
    let stream = service.check_drift().await;
    let events = collect_events(stream).await;

    let has_drift = events
        .iter()
        .any(|e| matches!(e, PackageEvent::DotfileDriftDetected { .. }));
    assert!(
        has_drift,
        "Should detect drift in standalone dotfile after target modification"
    );

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfileDriftChecked { drift_count, .. }) => {
            assert_eq!(*drift_count, 1);
        }
        other => panic!("Expected DotfileDriftChecked success, got: {other:?}"),
    }
}

// ───────────────────────────────── Track tests ─────────────────────────────────

#[tokio::test]
async fn test_track_standalone_creates_spec_and_copies_file() {
    let dirs = TestDirs::new();

    // Create a "target" file to track (simulating ~/.config/starship.toml)
    let target_file = dirs.target_dir.join("starship.toml");
    std::fs::write(&target_file, "format = \"$all\"").unwrap();

    let service = dirs.service_with_dotfiles();
    let stream = service
        .track_standalone("starship", target_file.to_str().unwrap())
        .await;
    let events = collect_events(stream).await;

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfileTracked { name, .. }) => {
            assert_eq!(name, "starship");
        }
        other => panic!("Expected DotfileTracked success, got: {other:?}"),
    }

    // Source file should be copied into dotfiles_dir/starship/starship.toml
    let copied = dirs.dotfiles_dir.join("starship").join("starship.toml");
    assert!(
        copied.exists(),
        "Source file should be copied to dotfiles dir"
    );
    assert_eq!(
        std::fs::read_to_string(&copied).unwrap(),
        "format = \"$all\""
    );

    // YAML spec should be created at dotfiles_dir/starship.yml
    let spec = dirs.dotfiles_dir.join("starship.yml");
    assert!(spec.exists(), "YAML spec should be created");
    let spec_content = std::fs::read_to_string(&spec).unwrap();
    assert!(
        spec_content.contains("starship/starship.toml"),
        "Spec should reference the source file with subdirectory"
    );
}

#[tokio::test]
async fn test_track_standalone_fails_when_target_missing() {
    let dirs = TestDirs::new();
    let service = dirs.service_with_dotfiles();

    let stream = service
        .track_standalone("missing", "/nonexistent/file.toml")
        .await;
    let events = collect_events(stream).await;

    let result = get_operation_result(&events).expect("Should have a Completed event");
    assert!(
        matches!(result, OperationResult::Failure(_)),
        "Should fail when target file doesn't exist"
    );
}

// Tracking copies the file into the dotfiles directory, and the writer creates
// missing parent directories. A directory removed after the service was built
// has to stop the track, or a mistyped path quietly becomes a new directory
// holding one spec.
#[tokio::test]
async fn track_standalone_refuses_a_missing_dotfiles_directory_and_does_not_create_it() {
    let dirs = TestDirs::new();
    let target_file = dirs.target_dir.join("starship.toml");
    std::fs::write(&target_file, "format = \"$all\"").unwrap();
    let service = dirs.service_with_dotfiles();
    std::fs::remove_dir_all(&dirs.dotfiles_dir).unwrap();

    let events = collect_events(
        service
            .track_standalone("starship", target_file.to_str().unwrap())
            .await,
    )
    .await;

    let message = failure_message(&events);
    assert!(
        message.contains("Cannot track a standalone dotfile"),
        "got: {message}"
    );
    assert!(
        message.contains(&dirs.dotfiles_dir.display().to_string()),
        "the refusal must name the dotfiles directory, got: {message}"
    );
    assert!(
        !dirs.dotfiles_dir.exists(),
        "the missing dotfiles directory must not be created"
    );
}

// A symlink loop is a path nothing is known about, and it refuses. Two sentences
// it must not get: "does not exist", which carries a `mkdir -p` hint that cannot
// work on a path already there, and "could not be listed", which claims a
// directory is there hiding entries when no such thing has been established.
//
// The wording is asserted, not only the refusal. Every state refuses, so a test
// checking that a refusal happened cannot tell the states apart, and this one
// could not before: it named a classification its assertions never reached.
#[tokio::test]
async fn track_standalone_reports_a_symlink_loop_dotfiles_directory_as_unknown() {
    let dirs = TestDirs::new();
    let target_file = dirs.target_dir.join("starship.toml");
    std::fs::write(&target_file, "format = \"$all\"").unwrap();
    let service = dirs.service_with_dotfiles();
    std::fs::remove_dir_all(&dirs.dotfiles_dir).unwrap();
    std::os::unix::fs::symlink(&dirs.dotfiles_dir, &dirs.dotfiles_dir).unwrap();

    let events = collect_events(
        service
            .track_standalone("starship", target_file.to_str().unwrap())
            .await,
    )
    .await;

    let message = failure_message(&events);
    assert!(
        message.contains("Cannot track a standalone dotfile"),
        "got: {message}"
    );
    assert!(
        message.contains("could not be checked"),
        "a loop is a path nothing is known about, got: {message}"
    );
    assert!(
        !message.contains("could not be listed"),
        "nothing established that a directory is there, got: {message}"
    );
    assert!(
        !message.contains("mkdir -p"),
        "a path that is already there offers no hint that cannot work, got: {message}"
    );
    assert!(
        !dirs.dotfiles_dir.join("starship.yml").exists(),
        "nothing must be written when the directory cannot be checked"
    );
    assert!(
        !dirs.dotfiles_dir.join("starship").exists(),
        "nothing must be written when the directory cannot be listed"
    );
}

#[tokio::test]
async fn test_track_for_package_adds_dotfile_to_existing_package() {
    let dirs = TestDirs::new();

    // Create an existing package without dotfiles
    let yaml = r#"name: alacritty
environments:
  test:
    install: "echo installed"
"#;
    std::fs::write(dirs.package_dir.join("alacritty.yml"), yaml).unwrap();

    // Create a "target" file to track
    let target_file = dirs.target_dir.join("alacritty.toml");
    std::fs::write(&target_file, "[font]\nsize = 12").unwrap();

    let service = dirs.service();
    let stream = service
        .track_for_package("alacritty", target_file.to_str().unwrap())
        .await;
    let events = collect_events(stream).await;

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfileTracked { name, .. }) => {
            assert_eq!(name, "alacritty");
        }
        other => panic!("Expected DotfileTracked success, got: {other:?}"),
    }

    // Source file should be copied into a subdirectory named after the package
    let copied = dirs.package_dir.join("alacritty").join("alacritty.toml");
    assert!(
        copied.exists(),
        "Source file should be copied into package subdirectory"
    );
    assert_eq!(
        std::fs::read_to_string(&copied).unwrap(),
        "[font]\nsize = 12"
    );

    // Package YAML should now contain a dotfiles section with relative source path
    let updated_yaml = std::fs::read_to_string(dirs.package_dir.join("alacritty.yml")).unwrap();
    assert!(
        updated_yaml.contains("dotfiles"),
        "Updated YAML should contain dotfiles section"
    );
    assert!(
        updated_yaml.contains("alacritty/alacritty.toml"),
        "Updated YAML should reference the tracked file with subdirectory"
    );
}

// selfie-ir68.15. Two guards, one per loop, and a test each: with one entry per
// package the outer guard catches the cancel before the inner one is reached, so no
// single fixture pins both.
//
// Cancelled on the read the first entry performs, so the cancellation lands inside
// the run. Cancelling from the consumer cannot: the event stream buffers, so the run
// has already finished by the time the first event is read, and drift reaches no
// command runner for the `cancellation` module's trigger to work through.
#[tokio::test]
async fn a_cancelled_drift_check_stops_between_entries_of_one_package() {
    let dirs = TestDirs::new();

    // Two entries in ONE package, so only the guard inside the entry loop can stop
    // the run between them.
    let source_dir = dirs.package_dir.join("both");
    std::fs::create_dir_all(&source_dir).unwrap();
    for name in ["first", "second"] {
        std::fs::write(source_dir.join(format!("{name}.toml")), "from-repo").unwrap();
        std::fs::write(dirs.target_dir.join(format!("{name}.toml")), "drifted").unwrap();
    }
    create_package_with_dotfiles(
        &dirs.package_dir,
        "both",
        &[
            (
                "both/first.toml",
                dirs.target_dir.join("first.toml").to_str().unwrap(),
            ),
            (
                "both/second.toml",
                dirs.target_dir.join("second.toml").to_str().unwrap(),
            ),
        ],
    );

    let token = CancellationToken::new();
    let service = dirs.service_cancelling_on_read(&source_dir.join("first.toml"), token.clone());

    let events = collect_events(service.check_drift().await).await;

    assert_cancelled_without_counts(&events);
    let examined = events
        .iter()
        .filter(|e| matches!(e, PackageEvent::DotfileDriftDetected { .. }))
        .count();
    assert_eq!(
        examined, 1,
        "the run continued to the package's second entry: {events:?}"
    );
}

// The third case, and the one neither loop guard can reach: with no packages at all
// a `for` body's first statement never runs. Such a run used to report
// DotfileDriftChecked with zero counts and exit 0 — a clean bill of health for a check
// that examined nothing.
#[tokio::test]
async fn a_cancelled_drift_check_over_no_packages_reports_the_cancellation() {
    let dirs = TestDirs::new();

    // Deliberately no packages, and the token is cancelled before the run so the
    // cancel cannot depend on anything the run does.
    let token = CancellationToken::new();
    token.cancel();
    let service = dirs.service_with_runner_and_token(FakeCommandRunner::new(), token);

    let events = collect_events(service.check_drift().await).await;

    assert_cancelled_without_counts(&events);
}

// The other guard. A package with no entries for this environment never enters the
// inner loop, so only the guard at the top of the package loop can stop a run
// walking a directory of them.
#[tokio::test]
async fn a_cancelled_drift_check_stops_between_packages() {
    let dirs = TestDirs::new();

    // One package with an entry, to trigger the cancel, then two with none.
    let source_dir = dirs.package_dir.join("first");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "from-repo").unwrap();
    std::fs::write(dirs.target_dir.join("first.toml"), "drifted").unwrap();
    create_package_with_dotfiles(
        &dirs.package_dir,
        "first",
        &[(
            "first/config.toml",
            dirs.target_dir.join("first.toml").to_str().unwrap(),
        )],
    );
    for name in ["second", "third"] {
        std::fs::write(
            dirs.package_dir.join(format!("{name}.yml")),
            format!("name: {name}\nenvironments:\n  test:\n    install: \"echo i\"\n"),
        )
        .unwrap();
    }

    let token = CancellationToken::new();
    let service = dirs.service_cancelling_on_read(&source_dir.join("config.toml"), token.clone());

    let events = collect_events(service.check_drift().await).await;

    // With no guard on the package loop the entry-less packages are walked and the
    // run completes, reporting counts for a run the user interrupted.
    assert_cancelled_without_counts(&events);
}

// Reported as a cancellation, not as a failure carrying prose: that is the event the
// CLI turns into exit 130, which is what Ctrl+C is supposed to produce. A completion
// carrying counts must not arrive alongside it.
fn assert_cancelled_without_counts(events: &[PackageEvent]) {
    let canceled = events
        .iter()
        .find_map(|e| match e {
            PackageEvent::Canceled { reason, .. } => Some(reason.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no cancellation was reported: {events:?}"));
    assert!(
        canceled.contains("Drift"),
        "the cancellation must name the operation the user ran: {canceled}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, PackageEvent::Completed { .. })),
        "a cancelled run must not also report counts: {events:?}"
    );
}

// selfie-ir68.16. Package lookup folds case, so `BAT` resolves `packages/bat.yml`,
// and the copy has to land beside that spec rather than under the spelling the
// caller happened to type.
//
// Asserted on the recorded `source:` rather than on a directory listing, because
// `packages/BAT/` and `packages/bat/` are the same directory on a case-insensitive
// volume: the listing would look right there while the spec still recorded a
// spelling that disagrees with every other entry in it.
#[tokio::test]
async fn a_package_track_spelled_in_another_case_copies_beside_the_spec() {
    let dirs = TestDirs::new();

    let yaml = r#"name: bat
environments:
  test:
    install: "echo installed"
"#;
    std::fs::write(dirs.package_dir.join("bat.yml"), yaml).unwrap();

    let target_file = dirs.target_dir.join("batrc");
    std::fs::write(&target_file, "--theme=ansi").unwrap();

    let service = dirs.service();
    let events = collect_events(
        service
            .track_for_package("BAT", target_file.to_str().unwrap())
            .await,
    )
    .await;

    match get_operation_result(&events).expect("Should have a Completed event") {
        OperationResult::Success(OperationSuccess::DotfileTracked { .. }) => {}
        other => panic!("expected the track to succeed, got: {other:?}"),
    }

    let spec = std::fs::read_to_string(dirs.package_dir.join("bat.yml")).unwrap();
    assert!(
        spec.contains("source: bat/batrc"),
        "the entry must record the spec's own name, got:\n{spec}"
    );
    assert!(
        !spec.contains("BAT/"),
        "the entry records the caller's spelling:\n{spec}"
    );
}

// selfie-ir68.16. `spec_name_from_file_name` splits on the last dot, so a spec file
// named `...yml` is loadable under the name `..`, and a copy directory composed from
// that name lands outside the package directory. The guard asks about containment,
// not about characters, which is what separates this from the ordinary dotted stem
// in the control below.

// The wording is asserted, not just the absence of the file. A spec that failed to
// load would leave nothing written for an unrelated reason, and this test would then
// pass while the guard did nothing.
#[tokio::test]
async fn a_package_track_refuses_a_copy_directory_outside_the_package_directory() {
    let dirs = TestDirs::new();

    // Loadable, and its name is `..`.
    let yaml = r#"name: dots
environments:
  test:
    install: "echo installed"
"#;
    std::fs::write(dirs.package_dir.join("...yml"), yaml).unwrap();

    let target_file = dirs.target_dir.join("gemrc");
    std::fs::write(&target_file, "gem: --no-document").unwrap();

    let service = dirs.service();
    let events = collect_events(
        service
            .track_for_package("..", target_file.to_str().unwrap())
            .await,
    )
    .await;

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Failure(OperationFailure::Generic(message)) => {
            assert!(
                message.contains("outside"),
                "the refusal must say what it prevented: {message}"
            );
            assert!(
                message.contains(".."),
                "the refusal must name what it refused: {message}"
            );
        }
        other => panic!("expected the track to be refused, got: {other:?}"),
    }

    // The escape itself: one level above the package directory is where
    // `packages/..` resolves to.
    let escaped = dirs.package_dir.parent().unwrap().join("gemrc");
    assert!(
        !escaped.exists(),
        "a copy was written outside the package directory at {}",
        escaped.display()
    );
}

// The other name the split produces: `..yml` yields `.`, which composes the spec's
// own directory rather than one below it. The copy would land beside the specs and
// the entry would record a `source:` naming a directory that is not there.
#[tokio::test]
async fn a_package_track_refuses_a_copy_directory_that_is_the_package_directory() {
    let dirs = TestDirs::new();

    let yaml = r#"name: dot
environments:
  test:
    install: "echo installed"
"#;
    std::fs::write(dirs.package_dir.join("..yml"), yaml).unwrap();

    let target_file = dirs.target_dir.join("gemrc");
    std::fs::write(&target_file, "gem: --no-document").unwrap();

    let service = dirs.service();
    let events = collect_events(
        service
            .track_for_package(".", target_file.to_str().unwrap())
            .await,
    )
    .await;

    match get_operation_result(&events).expect("Should have a Completed event") {
        OperationResult::Failure(OperationFailure::Generic(message)) => {
            assert!(
                message.contains("rather than a directory of its own"),
                "the refusal must say what it prevented: {message}"
            );
        }
        other => panic!("expected the track to be refused, got: {other:?}"),
    }
    assert!(
        !dirs.package_dir.join("gemrc").exists(),
        "a copy was written straight into the package directory"
    );
}

// The control for the guard above, and the reason it asks about containment rather
// than about characters. `python3.11.yml` is an ordinary package that loads and
// deploys today; a guard on the name's characters would make it untrackable.
#[tokio::test]
async fn a_package_track_still_works_for_a_dotted_spec_stem() {
    let dirs = TestDirs::new();

    let yaml = r#"name: python
environments:
  test:
    install: "echo installed"
"#;
    std::fs::write(dirs.package_dir.join("python3.11.yml"), yaml).unwrap();

    let target_file = dirs.target_dir.join("pythonrc");
    std::fs::write(&target_file, "import sys").unwrap();

    let service = dirs.service();
    let events = collect_events(
        service
            .track_for_package("python3.11", target_file.to_str().unwrap())
            .await,
    )
    .await;

    match get_operation_result(&events).expect("Should have a Completed event") {
        OperationResult::Success(OperationSuccess::DotfileTracked { .. }) => {}
        other => panic!("a dotted spec stem must still track, got: {other:?}"),
    }
    assert!(
        dirs.package_dir
            .join("python3.11")
            .join("pythonrc")
            .exists(),
        "the copy must land in the package's own directory"
    );
}

#[tokio::test]
async fn test_track_for_package_fails_when_package_not_found() {
    let dirs = TestDirs::new();

    let target_file = dirs.target_dir.join("some.conf");
    std::fs::write(&target_file, "content").unwrap();

    let service = dirs.service();
    let stream = service
        .track_for_package("nonexistent", target_file.to_str().unwrap())
        .await;
    let events = collect_events(stream).await;

    let result = get_operation_result(&events).expect("Should have a Completed event");
    // Typed, not stringified. Every other single-package path carries the load
    // error through with its type intact, so an adapter rendering the parts in its
    // own channels -- a source snippet, a structured location -- reaches this
    // command too instead of silently skipping it.
    assert!(
        matches!(
            result,
            OperationResult::Failure(OperationFailure::Package(_))
        ),
        "the load failure must keep its type, got: {result:?}"
    );
    let message = failure_message(&events);
    assert!(
        message.contains("nonexistent"),
        "the failure must still name the package, got: {message}"
    );
}

// Tracking a file under the home directory records it as `~/…`.
//
// The home directory is injected rather than read, so the test says nothing
// about the machine it runs on: `/Users` vs `/home` and the real `$HOME` are
// both out of the picture.
#[tokio::test]
async fn a_tracked_target_under_home_is_recorded_relative_to_it() {
    use selfie::package::port::PackageRepository;

    let dirs = TestDirs::new();
    let home = dirs.target_dir.clone();
    let target_file = home.join(".config").join("ghostty").join("config");
    std::fs::create_dir_all(target_file.parent().unwrap()).unwrap();
    std::fs::write(&target_file, "font-size = 13").unwrap();

    let events = collect_events(
        dirs.service_with_home(&home)
            .track_standalone("ghostty", target_file.to_str().unwrap())
            .await,
    )
    .await;

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfileTracked { target_path, .. }) => {
            assert_eq!(
                target_path, "~/.config/ghostty/config",
                "the reported target must be the one that was recorded"
            );
        }
        other => panic!("Expected DotfileTracked success, got: {other:?}"),
    }

    let spec = std::fs::read_to_string(dirs.dotfiles_dir.join("ghostty.yml")).unwrap();
    assert!(
        spec.contains("~/.config/ghostty/config"),
        "spec should record a home-relative target, got:\n{spec}"
    );
    assert!(
        !spec.contains(home.to_str().unwrap()),
        "spec should not name this machine's home directory, got:\n{spec}"
    );

    // The recorded form has to survive the reader, or track writes a spec that
    // apply cannot use. `~` alone is YAML's null.
    let reread = YamlPackageRepository::new(
        RealFileSystem,
        dirs.dotfiles_dir.clone(),
        SpecOrigin::DotfilesDirectory,
    )
    .get_package("ghostty")
    .expect("the tracked spec should load again");
    assert_eq!(
        reread.package().dotfiles()[0].target(),
        "~/.config/ghostty/config"
    );
}

// `track_for_package` writes the entry through a different function than
// `track_standalone`, so it needs its own proof rather than an argument that
// the two are alike.
#[tokio::test]
async fn a_target_tracked_into_a_package_is_recorded_relative_to_home() {
    let dirs = TestDirs::new();
    let home = dirs.target_dir.clone();
    let target_file = home.join(".config").join("bat").join("config");
    std::fs::create_dir_all(target_file.parent().unwrap()).unwrap();
    std::fs::write(&target_file, "--theme=ansi").unwrap();
    create_package_with_dotfiles(&dirs.package_dir, "bat", &[]);

    let events = collect_events(
        dirs.service_with_home(&home)
            .track_for_package("bat", target_file.to_str().unwrap())
            .await,
    )
    .await;

    let result = get_operation_result(&events).expect("Should have a Completed event");
    assert!(
        matches!(result, OperationResult::Success(_)),
        "tracking should succeed, got: {result:?}"
    );

    let spec = std::fs::read_to_string(dirs.package_dir.join("bat.yml")).unwrap();
    assert!(
        spec.contains("~/.config/bat/config"),
        "spec should record a home-relative target, got:\n{spec}"
    );
    assert!(
        !spec.contains(home.to_str().unwrap()),
        "spec should not name this machine's home directory, got:\n{spec}"
    );
}

// Re-tracking a file already in the spec reports the spec's target, not the
// path the caller happened to type. The two differ exactly here: the spec holds
// `~/…` and the caller passes an absolute path.
#[tokio::test]
async fn re_tracking_reports_the_target_the_spec_holds() {
    let dirs = TestDirs::new();
    let home = dirs.target_dir.clone();
    let target_file = home.join(".config").join("bat").join("config");
    std::fs::create_dir_all(target_file.parent().unwrap()).unwrap();
    std::fs::write(&target_file, "--theme=ansi").unwrap();
    create_package_with_dotfiles(
        &dirs.package_dir,
        "bat",
        &[("bat/config", "~/.config/bat/config")],
    );

    let events = collect_events(
        dirs.service_with_home(&home)
            .track_for_package("bat", target_file.to_str().unwrap())
            .await,
    )
    .await;

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfileTracked {
            target_path,
            was_already_tracked,
            ..
        }) => {
            assert!(was_already_tracked, "the entry was already in the spec");
            assert_eq!(target_path, "~/.config/bat/config");
        }
        other => panic!("Expected DotfileTracked success, got: {other:?}"),
    }
}

// A target outside the home directory keeps its absolute form. Without this the
// two tests above would pass on an implementation that tildes every path.
#[tokio::test]
async fn a_tracked_target_outside_home_keeps_its_absolute_path() {
    let dirs = TestDirs::new();
    let elsewhere = dirs.state_dir.join("nginx.conf");
    std::fs::write(&elsewhere, "worker_processes 1;").unwrap();

    let events = collect_events(
        dirs.service_with_home(&dirs.target_dir)
            .track_standalone("nginx", elsewhere.to_str().unwrap())
            .await,
    )
    .await;

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfileTracked { target_path, .. }) => {
            assert_eq!(target_path, elsewhere.to_str().unwrap());
        }
        other => panic!("Expected DotfileTracked success, got: {other:?}"),
    }

    let spec = std::fs::read_to_string(dirs.dotfiles_dir.join("nginx.yml")).unwrap();
    assert!(
        spec.contains(elsewhere.to_str().unwrap()),
        "spec should keep the absolute target, got:\n{spec}"
    );
}

// ─── Secret-bearing dotfiles ────────────────────────────────────────────────
//
// Content that comes from a command, or from a template with `vars`, is resolved
// at apply time, compared in memory, and never recorded. See ADR-0003.

mod secret_bearing {
    use super::*;
    use selfie::dotfile_service::port::{ConflictDetail, ConflictResolution, ConflictResolver};
    use std::sync::{Arc, Mutex};

    // A value distinctive enough that finding it anywhere is unambiguous.
    const SECRET: &str = "s3cr3t-v4lue-DO-NOT-LEAK";

    // Write a package whose single dotfile is a whole-file provider entry.
    fn provider_package(package_dir: &std::path::Path, target: &str, command: &str) {
        let yaml = format!(
            "name: creds\nenvironments:\n  test:\n    install: \"echo i\"\ndotfiles:\n  \
             - command: \"{command}\"\n    target: \"{target}\"\n"
        );
        std::fs::write(package_dir.join("creds.yml"), yaml).unwrap();
    }

    // Write a package whose single dotfile is a template, plus the template.
    fn template_package(
        package_dir: &std::path::Path,
        target: &str,
        template_body: &str,
        vars: &[(&str, &str)],
    ) {
        std::fs::create_dir_all(package_dir.join("creds")).unwrap();
        std::fs::write(package_dir.join("creds/credentials.tpl"), template_body).unwrap();

        let mut yaml = format!(
            "name: creds\nenvironments:\n  test:\n    install: \"echo i\"\ndotfiles:\n  \
             - source: \"creds/credentials.tpl\"\n    target: \"{target}\"\n    vars:\n"
        );
        for (name, command) in vars {
            yaml.push_str(&format!("      {name}: \"{command}\"\n"));
        }
        std::fs::write(package_dir.join("creds.yml"), yaml).unwrap();
    }

    // Assert that no event reproduces `secret`, in any rendering.
    //
    // Scans every event and every field rather than one variant's diff: a leak
    // added to a warning, or to a newly introduced field, has to fail this too.
    // *Which* renderings count is `test_common::secrets`' decision, not this
    // call site's — see that module for what they are and what they miss.
    #[track_caller]
    fn assert_no_event_mentions(events: &[PackageEvent], secret: impl AsRef<[u8]>) {
        let secret = secret.as_ref();
        for event in events {
            test_common::assert_secret_free(&format!("{event:?}"), secret, "an event");
        }
    }

    // A resolver that accepts every conflict.
    //
    // Secret-bearing entries ignore `auto_accept`, so a test needing the
    // overwrite path has to go through a resolver — the same route a human at a
    // terminal takes.
    struct AlwaysAcceptSecret;

    impl ConflictResolver for AlwaysAcceptSecret {
        fn resolve(&self, _target: &str, _detail: ConflictDetail<'_>) -> ConflictResolution {
            ConflictResolution::Accept
        }
    }

    fn accepting() -> ApplyOptions {
        ApplyOptions {
            conflict_resolver: Some(Arc::new(AlwaysAcceptSecret)),
            ..Default::default()
        }
    }

    fn state_file(dirs: &TestDirs) -> PathBuf {
        dirs.state_dir.join("deploy-state.yml")
    }

    #[tokio::test]
    async fn provider_content_is_deployed_to_an_absent_target() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert_eq!(std::fs::read_to_string(&target).unwrap(), SECRET);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, PackageEvent::DotfileDeployed { .. })),
        );
    }

    // ---- A provider or binding whose output could not be read (selfie-ql8m) ----
    //
    // The harm is not "resolve returns an error" — it is a truncated credential
    // reaching a file. A short read looks like a short command: the
    // `MAX_CONTENT_BYTES` cap is a maximum, so a prefix sails under it, and for a
    // var binding a prefix is still non-empty. So these assert the filesystem, and
    // each has a control proving the same fixture *does* write when the read
    // succeeds.

    #[tokio::test]
    async fn a_provider_whose_output_could_not_be_read_writes_no_target() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().stdout_read_failing("op read x");
        let service = dirs.service_with_runner(runner);

        let _ = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert!(
            !target.exists(),
            "a truncated credential was written to {}",
            target.display()
        );
    }

    #[tokio::test]
    async fn a_provider_whose_output_could_not_be_read_leaves_a_pre_existing_target_intact() {
        let dirs = TestDirs::new();
        use std::os::unix::fs::PermissionsExt as _;

        let target = dirs.target_dir.join("credentials");
        std::fs::write(&target, "previous-credential-abc123").unwrap();
        let mode_before = std::fs::metadata(&target).unwrap().permissions().mode();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().stdout_read_failing("op read x");
        let service = dirs.service_with_runner(runner);

        // `accepting()`, not `ApplyOptions::default()`. Without a resolver a
        // secret-bearing entry whose target differs is reported as a conflict and
        // skipped, so the target would survive for a reason that has nothing to
        // do with the read — and mutating `run_capture` to return partial content
        // left this test passing. It has to take the overwrite path for the
        // assertion below to mean anything.
        let _ = collect_events(service.apply_all(accepting()).await).await;

        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"previous-credential-abc123",
            "a working credential was replaced with a truncated one"
        );
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode(),
            mode_before,
            "the target's mode changed despite nothing being deployed"
        );
    }

    #[tokio::test]
    async fn the_same_provider_fixture_does_write_when_the_read_succeeds() {
        // Control for both tests above. Without it they pass if apply never
        // reached the entry at all — a broken fixture would look like a fix.
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::fs::write(&target, "previous-credential-abc123").unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let _ = collect_events(service.apply_all(accepting()).await).await;

        assert_eq!(std::fs::read_to_string(&target).unwrap(), SECRET);
    }

    #[tokio::test]
    async fn a_binding_whose_output_could_not_be_read_writes_no_target() {
        // The second write path. A truncated binding is spliced into a rendered
        // file, and neither the emptiness check nor the size cap can see it.
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        template_package(
            &dirs.package_dir,
            target.to_str().unwrap(),
            "key: {{ api_key }}\n",
            &[("api_key", "op read x")],
        );

        let runner = FakeCommandRunner::new().stdout_read_failing("op read x");
        let service = dirs.service_with_runner(runner);

        let _ = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert!(
            !target.exists(),
            "a file holding a truncated credential was written to {}",
            target.display()
        );
    }

    #[tokio::test]
    async fn a_binding_whose_output_could_not_be_read_leaves_a_pre_existing_target_intact() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::fs::write(&target, "key: previous-credential-abc123\n").unwrap();
        template_package(
            &dirs.package_dir,
            target.to_str().unwrap(),
            "key: {{ api_key }}\n",
            &[("api_key", "op read x")],
        );

        let runner = FakeCommandRunner::new().stdout_read_failing("op read x");
        let service = dirs.service_with_runner(runner);

        // `accepting()` for the same reason as the provider case above: without a
        // resolver this would be skipped as a conflict and prove nothing.
        let _ = collect_events(service.apply_all(accepting()).await).await;

        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "key: previous-credential-abc123\n",
            "a working credential was replaced with a truncated one"
        );
    }

    #[tokio::test]
    async fn the_same_binding_fixture_does_write_when_the_read_succeeds() {
        // Control for the two binding tests above. Seeded and `accepting()` so it
        // exercises the same overwrite path the pre-existing-target test does —
        // a control taking a different path would not control anything.
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::fs::write(&target, "key: previous-credential-abc123\n").unwrap();
        template_package(
            &dirs.package_dir,
            target.to_str().unwrap(),
            "key: {{ api_key }}\n",
            &[("api_key", "op read x")],
        );

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let _ = collect_events(service.apply_all(accepting()).await).await;

        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            format!("key: {SECRET}\n")
        );
    }

    #[tokio::test]
    async fn provider_commands_run_in_the_package_directory() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", b"v");
        let service = dirs.service_with_runner(runner.clone());

        let _ = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert_eq!(
            runner.calls(),
            vec![("op read x".to_string(), dirs.package_dir.clone())],
        );
    }

    #[tokio::test]
    async fn an_in_sync_target_is_skipped() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::fs::write(&target, SECRET).unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        // `starts_with`, not equality: a target seeded by `std::fs::write` is
        // 0644, so this also takes the permissions-tightening branch, whose
        // reason extends the same prefix. Both are in-sync skips, which is what
        // this test is about; the two modes are covered separately below.
        let skipped = events.iter().any(|e| {
            matches!(
                e,
                PackageEvent::DotfileSkipped { reason, .. }
                    if reason.starts_with("already in sync")
            )
        });
        assert!(skipped, "expected an in-sync skip, got: {events:?}");
    }

    #[tokio::test]
    async fn a_differing_target_is_a_conflict_and_is_not_overwritten() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::fs::write(&target, "hand-edited").unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert!(
            events
                .iter()
                .any(|e| matches!(e, PackageEvent::DotfileConflict { .. })),
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "hand-edited",
            "a conflict must leave the target alone"
        );
    }

    #[tokio::test]
    async fn a_secret_entry_records_no_deploy_state() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let _ = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        // A stored checksum of a credential is a confirmation oracle: ADR-0003.
        let state = std::fs::read_to_string(state_file(&dirs)).unwrap_or_default();
        assert!(
            !state.contains("credentials"),
            "secret-bearing entries must record no deploy state, got: {state}"
        );
    }

    #[tokio::test]
    async fn a_plain_repo_file_entry_still_records_deploy_state() {
        // Proves the existing checksum path is untouched by the secret path.
        let dirs = TestDirs::new();
        let source_dir = dirs.package_dir.join("myapp");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();
        let target = dirs.target_dir.join("config.toml");
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );

        let service = dirs.service();
        let _ = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        let state = std::fs::read_to_string(state_file(&dirs)).unwrap();
        assert!(state.contains("myapp/config.toml"), "got: {state}");
    }

    #[tokio::test]
    async fn a_secret_target_is_written_owner_only_even_over_a_world_readable_file() {
        use std::os::unix::fs::PermissionsExt as _;

        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::fs::write(&target, "old").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let _ = collect_events(service.apply_all(accepting()).await).await;

        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "credential targets must be owner-only");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), SECRET);
    }

    #[tokio::test]
    async fn an_in_sync_target_with_lax_permissions_is_tightened() {
        use std::os::unix::fs::PermissionsExt as _;

        // The adoption path ADR-0003 names as the reason this design is safe: a
        // machine with pre-existing config whose content already matches. Being
        // told "already in sync" while the file stays world-readable would leave
        // the user believing it is managed to the standard the docs promise
        // unconditionally.
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::fs::write(&target, SECRET).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "an in-sync target must still end up owner-only"
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            SECRET,
            "content must be unchanged"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                PackageEvent::DotfileSkipped { reason, .. } if reason.contains("permissions")
            )),
            "the tightening must be reported rather than done silently: {events:?}"
        );
        assert_no_event_mentions(&events, SECRET);
    }

    #[tokio::test]
    async fn an_in_sync_target_readable_only_by_its_group_is_still_tightened() {
        use std::os::unix::fs::PermissionsExt as _;

        // 0640 leaks to the group but not to others, so a check that only looks
        // at the "other" bits would pass it. On a shared machine the group is
        // exactly who you are hiding a credential from.
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::fs::write(&target, SECRET).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let _ = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "group-readable is not owner-only");
    }

    #[tokio::test]
    async fn an_in_sync_target_already_owner_only_is_left_completely_alone() {
        use std::os::unix::fs::MetadataExt as _;
        use std::os::unix::fs::PermissionsExt as _;

        // The counterpart: tightening must be conditional. Rewriting a correct
        // file on every apply would churn the inode and make "already in sync" a
        // lie.
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::fs::write(&target, SECRET).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        let before = std::fs::metadata(&target).unwrap().ino();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert_eq!(
            std::fs::metadata(&target).unwrap().ino(),
            before,
            "an already-correct target must not be rewritten"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                PackageEvent::DotfileSkipped { reason, .. }
                    if reason == "already in sync"
            )),
            "expected a plain in-sync skip, got: {events:?}"
        );
    }

    #[tokio::test]
    async fn a_symlinked_secret_target_is_replaced_not_written_through() {
        let dirs = TestDirs::new();
        let elsewhere = dirs.target_dir.join("elsewhere");
        std::fs::write(&elsewhere, "untouched").unwrap();
        let target = dirs.target_dir.join("credentials");
        std::os::unix::fs::symlink(&elsewhere, &target).unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let _ = collect_events(service.apply_all(accepting()).await).await;

        assert_eq!(
            std::fs::read_to_string(&elsewhere).unwrap(),
            "untouched",
            "the credential must not be written through the link"
        );
        assert!(
            !std::fs::symlink_metadata(&target)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link itself must be replaced"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), SECRET);
    }

    // A link that appears while the provider command is running.
    //
    // The check in `usable_target` runs before the resolve, so its answer is stale by
    // the time the target is read. Without the second check immediately before that
    // read, the classifier reads *through* the newly planted link and the destination's
    // bytes reach the conflict resolver.
    //
    // The file system double reports no link the first time it is asked and the truth
    // afterwards, which is the only way to stage this: a real link is either there for
    // both checks or neither.
    #[tokio::test]
    async fn a_link_appearing_during_the_resolve_is_still_not_read_through() {
        const ELSEWHERE: &str = "PRIVATE-KEY-MATERIAL-9d41b7e2-not-ours";

        let dirs = TestDirs::new();
        let elsewhere = dirs.target_dir.join("id_ed25519");
        std::fs::write(&elsewhere, ELSEWHERE).unwrap();
        let target = dirs.target_dir.join("credentials");
        std::os::unix::fs::symlink(&elsewhere, &target).unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let looks = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dir_checks = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fs = SymlinkAppearsAfterFirstLook {
            inner: RealFileSystem,
            looks: looks.clone(),
            dir_checks: dir_checks.clone(),
        };
        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_fs(fs, runner);

        let resolver = Arc::new(RecordingResolver::default());
        let options = ApplyOptions {
            conflict_resolver: Some(resolver.clone()),
            ..Default::default()
        };
        let events = collect_events(service.apply_all(options).await).await;

        let seen = resolver.seen.lock().unwrap().clone();
        for value in &seen {
            test_common::assert_secret_free(value, ELSEWHERE.as_bytes(), "the resolver");
        }
        assert_no_event_mentions(&events, ELSEWHERE);

        // The double has to have been exercised, or this test cannot tell a staged
        // race from an ordinary link: both end in a replacement the resolver never
        // sees. Two looks means the first answered "plain" and the second the truth,
        // which is the window itself.
        assert!(
            looks.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "the target must have been asked about twice, once before the resolve and \
             once before the read"
        );
        // A witness that the first answer really was "plain": the directory question is
        // asked only of a plain target, so a link seen on the first pass would never
        // reach it. Without this, a double that reported the link both times would
        // still satisfy the count above.
        assert!(
            dir_checks.load(std::sync::atomic::Ordering::SeqCst) >= 1,
            "the first pass must have classified the target as plain"
        );

        // The control: the run really did deploy, so the absence above is not the
        // absence of any work at all.
        assert_eq!(std::fs::read_to_string(&target).unwrap(), SECRET);
        assert_eq!(
            std::fs::read_to_string(&elsewhere).unwrap(),
            ELSEWHERE,
            "the destination must be left exactly as it was"
        );
    }

    // The re-ask fails closed, as the first ask does. Falling back to the answer taken
    // before the resolve would swallow a refusal this code cannot interpret, at the one
    // point where the next statement reads the target.
    #[tokio::test]
    async fn an_unknown_refusal_at_the_second_ask_refuses_the_entry() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::fs::write(&target, "previous").unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let fs = SecondLookIsAnUnknownRefusal {
            inner: RealFileSystem,
            looks: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_fs(fs, runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert!(
            warning_messages(&events)
                .iter()
                .any(|w| w.starts_with("Skipping '") && w.contains("character device")),
            "the entry must be refused on a refusal it cannot interpret: {:?}",
            warning_messages(&events)
        );
        // The control: nothing was written, so the refusal really stopped the deploy
        // rather than merely adding a line beside it.
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "previous",
            "a refused entry must leave the target alone"
        );
    }

    // The security case. A link at a secret target must not be read *through*: the
    // classifier would return the destination's bytes, the conflict summary would
    // count its lines, and the resolver would receive them as `current`, so an
    // interactive reveal would print someone else's file.
    // `~/.config/app/creds -> ~/.ssh/id_ed25519` is the shape that matters.
    //
    // Asserted on what the resolver received, because no event ever carries those
    // bytes: an event scan cannot observe this leak and passes on the unfixed code.
    #[tokio::test]
    async fn a_symlinked_secret_target_is_never_read_through_to_the_resolver() {
        const ELSEWHERE: &str = "PRIVATE-KEY-MATERIAL-e3f9a1c7-not-ours";

        let dirs = TestDirs::new();
        let elsewhere = dirs.target_dir.join("id_ed25519");
        std::fs::write(&elsewhere, ELSEWHERE).unwrap();
        let target = dirs.target_dir.join("credentials");
        std::os::unix::fs::symlink(&elsewhere, &target).unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let resolver = Arc::new(RecordingResolver::default());
        let options = ApplyOptions {
            conflict_resolver: Some(resolver.clone()),
            ..Default::default()
        };
        let events = collect_events(service.apply_all(options).await).await;

        // Both scans go through the shared helper, which matches the value as text
        // *and* as a byte array. A credential renders both ways and the two share no
        // characters, so a `contains` on the literal alone passes a leak of the whole
        // value.
        let seen = resolver.seen.lock().unwrap().clone();
        for value in &seen {
            test_common::assert_secret_free(value, ELSEWHERE.as_bytes(), "the resolver");
        }
        assert_no_event_mentions(&events, ELSEWHERE);

        // The positive control: without it this passes when nothing was deployed.
        // A plain `contains`, because it asserts the secret IS there.
        assert_eq!(std::fs::read_to_string(&target).unwrap(), SECRET);
        assert_eq!(
            std::fs::read_to_string(&elsewhere).unwrap(),
            ELSEWHERE,
            "the destination must be left exactly as it was"
        );
    }

    // The link is replaced even when the destination already holds the resolved
    // content, and even when it is already owner-only.
    //
    // Mode `0600` is the whole fixture. `settle_in_sync` skips only when
    // `is_owner_only` answers true, and that call follows the link -- so a `0644`
    // destination is replaced by the tightening path whether or not a link is
    // handled correctly, and the fixture could not tell the two apart.
    #[tokio::test]
    async fn a_symlinked_secret_target_is_replaced_even_when_the_destination_matches() {
        use std::os::unix::fs::PermissionsExt as _;

        let dirs = TestDirs::new();
        let elsewhere = dirs.target_dir.join("already-right");
        std::fs::write(&elsewhere, SECRET).unwrap();
        std::fs::set_permissions(&elsewhere, std::fs::Permissions::from_mode(0o600)).unwrap();
        let target = dirs.target_dir.join("credentials");
        std::os::unix::fs::symlink(&elsewhere, &target).unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let _ = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert!(
            !std::fs::symlink_metadata(&target)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link must be replaced whatever the destination already held"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), SECRET);
    }

    // A link selfie could not read still deploys, and the warning names the link
    // alone. Printing "unknown" for the destination would state a fact about
    // selfie rather than about the user's file.
    #[tokio::test]
    async fn a_replaced_link_whose_destination_is_unreadable_is_still_named() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::os::unix::fs::symlink(dirs.target_dir.join("nowhere"), &target).unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        let warnings = warning_messages(&events);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("was a symlink") && w.contains("Replaced")),
            "the replacement must be reported as what happened: {warnings:?}"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), SECRET);
    }

    // The warning follows the write, so a failed write never reports a replacement
    // that did not happen.
    //
    // The write is made to fail by taking write permission off the target's own
    // directory: `write_file_private` creates its temporary file there, so it
    // cannot even begin. A warning sent before the write would appear here.
    #[tokio::test]
    async fn a_failed_write_to_a_symlinked_target_reports_no_replacement() {
        let dirs = TestDirs::new();
        let holding = dirs.target_dir.join("locked");
        std::fs::create_dir_all(&holding).unwrap();
        let elsewhere = dirs.target_dir.join("elsewhere");
        std::fs::write(&elsewhere, "untouched").unwrap();
        let target = holding.join("credentials");
        std::os::unix::fs::symlink(&elsewhere, &target).unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let Some(_restore) = made_unwritable(&holding) else {
            eprintln!(
                "SKIP a_failed_write_to_a_symlinked_target_reports_no_replacement: mode bits do \
                 not bite here"
            );
            return;
        };
        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        let warnings = warning_messages(&events);
        assert!(
            warnings.iter().any(|w| w.contains("Failed to write")),
            "the write must be reported as failed: {warnings:?}"
        );
        assert!(
            !warnings.iter().any(|w| w.contains("Replaced")),
            "a failed write must not claim the link was replaced: {warnings:?}"
        );
        // The control: the link survived because nothing was written, which is what
        // makes the absence above meaningful rather than vacuous.
        assert!(
            std::fs::symlink_metadata(&target)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link must still be there, since the write never happened"
        );
    }

    // Make `dir` impossible to look inside, so a stat of anything under it fails
    // with a permission error rather than a verdict.
    //
    // `None` when the mode does not bite -- running as root, or a file system that
    // ignores it -- so a caller skips rather than asserting about a check that
    // succeeded. The returned guard restores the mode on unwind, so a failing
    // assertion does not leave a directory the harness cannot clean up.
    //
    // Not `made_unwritable`: that sets `0o500`, which still allows the traversal
    // this needs to fail.
    fn made_unstattable(dir: &std::path::Path) -> Option<RestoreMode> {
        use std::os::unix::fs::PermissionsExt as _;

        let original = std::fs::metadata(dir).unwrap().permissions().mode() & 0o7777;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        let restore = RestoreMode(dir.to_path_buf(), original);
        if std::fs::read_dir(dir.join("inner")).is_ok() {
            return None;
        }
        Some(restore)
    }

    // Build a package whose target is `link`, run it, and report what the runner was
    // asked to do. Zero calls is the assertion these share: nothing may run for a
    // target that provably cannot be written.
    //
    // `FakeCommandRunner::call_count` rather than a mock's `times(0)`: the harness
    // here is built around the fake and a real file system, which is what makes the
    // link on disk a real link rather than a mocked answer.
    async fn calls_for_target(dirs: &TestDirs, link: &std::path::Path) -> (usize, Vec<String>) {
        provider_package(&dirs.package_dir, link.to_str().unwrap(), "op read x");
        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let counted = runner.clone();
        let service = dirs.service_with_runner(runner);
        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;
        (counted.call_count(), warning_messages(&events))
    }

    // A link to a directory is replaced like any other link: the rename lands on the
    // link itself and never on the directory behind it, so the credential goes where
    // the entry asked and the directory is untouched.
    //
    // The command count is the assertion that separates this from a refusal, and it
    // matches the plain-link case rather than being asserted on its own.
    #[tokio::test]
    async fn a_secret_target_linked_to_a_directory_is_replaced() {
        let dirs = TestDirs::new();
        let destination = dirs.target_dir.join("a-directory");
        std::fs::create_dir_all(destination.join("kept")).unwrap();
        let target = dirs.target_dir.join("credentials");
        std::os::unix::fs::symlink(&destination, &target).unwrap();

        let (calls, warnings) = calls_for_target(&dirs, &target).await;

        assert_eq!(
            calls, 1,
            "a link is replaced, so its command runs: {warnings:?}"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), SECRET);
        assert!(
            !std::fs::symlink_metadata(&target)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link must be replaced"
        );
        assert!(
            destination.join("kept").exists(),
            "the directory behind the link must be untouched"
        );
    }

    // A directory *at* the target, with no link involved. A rename cannot replace a
    // directory with a file, so the write can never succeed and nothing may run for
    // it -- a credential fetch can raise a biometric prompt, and it would be raised
    // for a deploy that then fails.
    #[tokio::test]
    async fn a_bare_directory_at_a_secret_target_refuses_before_running_anything() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::fs::create_dir_all(target.join("inner")).unwrap();

        let (calls, warnings) = calls_for_target(&dirs, &target).await;

        assert_eq!(calls, 0, "no command may run: {warnings:?}");
        assert!(
            warnings.iter().any(|w| w.starts_with("Skipping '")
                && w.contains("a file cannot replace a directory")
                && w.contains("No command was run")),
            "the refusal must say what is there and that nothing ran: {warnings:?}"
        );
        assert!(
            target.join("inner").exists(),
            "the directory must be left alone"
        );
    }

    // A target selfie cannot classify is not one it writes a credential over. The
    // directory holding it has no execute bit, so the stat cannot reach it and answers
    // with a permission error rather than a verdict.
    //
    // A plain target, because this is where the write really lands. A link is replaced
    // whatever is behind it, so nothing about its destination refuses it.
    #[tokio::test]
    async fn a_plain_secret_target_that_cannot_be_classified_refuses() {
        let dirs = TestDirs::new();
        let closed = dirs.target_dir.join("closed");
        std::fs::create_dir_all(closed.join("inner")).unwrap();
        let target = closed.join("inner").join("credentials");

        let Some(_restore) = made_unstattable(&closed) else {
            eprintln!(
                "SKIP a_plain_secret_target_that_cannot_be_classified_refuses: mode bits do not \
                 bite here"
            );
            return;
        };
        let (calls, warnings) = calls_for_target(&dirs, &target).await;

        assert_eq!(
            calls, 0,
            "an unclassifiable target must refuse before running: {warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.starts_with("Skipping '")
                && w.contains("could not determine what is at the target")),
            "got: {warnings:?}"
        );
    }

    // A fifo behind a *relative* link, which is what the destination check's argument
    // turns on. The check stats the target path, following the link, so it sees the
    // fifo. A check that resolved the link's own text instead would resolve
    // "pipe" against the process's working directory, find nothing, and let the
    // credential fetch run for a target the writer then refuses.
    //
    // Through `within_deadline`, as every fifo test is: a regression reaching the read
    // blocks on the fifo, and must fail rather than hang.
    #[test]
    fn a_secret_target_relatively_linked_to_a_fifo_refuses_before_running_anything() {
        let dirs = TestDirs::new();
        let pipe = dirs.target_dir.join("pipe");
        nix::unistd::mkfifo(&pipe, nix::sys::stat::Mode::S_IRWXU).unwrap();
        let target = dirs.target_dir.join("credentials");
        std::os::unix::fs::symlink("pipe", &target).unwrap();

        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");
        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let counted = runner.clone();
        let service = dirs.service_with_runner(runner);
        let events = within_deadline(std::time::Duration::from_secs(10), move || async move {
            collect_events(service.apply_all(ApplyOptions::default()).await).await
        })
        .expect("apply must not block on a fifo behind a link");
        let (calls, warnings) = (counted.call_count(), warning_messages(&events));

        assert_eq!(
            calls, 0,
            "a fifo behind a relative link must refuse before running: {warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("fifo")),
            "the refusal must name what is there: {warnings:?}"
        );
    }

    // The control on the fail-closed arm above, in the other direction. A dangling
    // link's destination is absent, which is provably not a directory, so it must
    // still deploy -- folding that error into the refusing arm would refuse every
    // dangling link, which decision 3 replaces.
    #[tokio::test]
    async fn a_dangling_secret_target_link_still_deploys() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::os::unix::fs::symlink(dirs.target_dir.join("not-there"), &target).unwrap();

        let (calls, warnings) = calls_for_target(&dirs, &target).await;

        assert_eq!(calls, 1, "the command must run: {warnings:?}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), SECRET);
    }

    // A preview over a symlinked target says what a real run would do, because the
    // outcome does not depend on content it must not fetch: the link is replaced
    // whatever the credential turns out to be.
    //
    // The rendered summary is asserted, not just the counts. That line is what a
    // user reads, and it has no dry-run wording of its own -- so counting a preview
    // as a deployment would print "1 deployed" for a run that wrote nothing. A
    // preview counts a deploy it would make as a skip, which is what the
    // repository-file path already does.
    #[tokio::test]
    async fn a_dry_run_over_a_symlinked_secret_target_previews_the_replacement() {
        let dirs = TestDirs::new();
        let elsewhere = dirs.target_dir.join("elsewhere");
        std::fs::write(&elsewhere, "untouched").unwrap();
        let target = dirs.target_dir.join("credentials");
        std::os::unix::fs::symlink(&elsewhere, &target).unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let counted = runner.clone();
        let service = dirs.service_with_runner(runner);

        let options = ApplyOptions {
            dry_run: true,
            ..Default::default()
        };
        let events = collect_events(service.apply_all(options).await).await;

        assert_eq!(counted.call_count(), 0, "a preview must run no command");
        assert!(
            std::fs::symlink_metadata(&target)
                .unwrap()
                .file_type()
                .is_symlink(),
            "a preview must not replace anything"
        );

        assert!(
            events.iter().any(|e| matches!(
                e,
                PackageEvent::DotfileSkipped { reason, .. }
                    if reason.contains("would run 1 command(s)")
                        && reason.contains("then replace the symlink")
            )),
            "the preview must name the outcome a real run would reach: {:?}",
            events
                .iter()
                .filter(|e| matches!(e, PackageEvent::DotfileSkipped { .. }))
                .collect::<Vec<_>>()
        );

        let result = get_operation_result(&events).expect("the run must complete");
        let rendered = format!("{result:?}");
        match result {
            OperationResult::Success(OperationSuccess::DotfilesApplied {
                deployed_count,
                skipped_count,
                refused_count,
                ..
            }) => {
                assert_eq!(
                    *deployed_count, 0,
                    "a preview wrote nothing, so nothing was deployed: {rendered}"
                );
                assert_eq!(*skipped_count, 1, "counted as a skip: {rendered}");
                assert_eq!(*refused_count, 0, "nothing was refused: {rendered}");
            }
            other => panic!("expected DotfilesApplied, got: {other:?}"),
        }
    }

    // The refusal is the same in a preview as in a real run, and still runs nothing.
    // A bare directory is what exercises it: a link to one is replaced now, and a link
    // to a fifo is refused by the guard ahead of this on both paths already.
    #[tokio::test]
    async fn a_dry_run_over_a_bare_directory_at_a_secret_target_refuses() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::fs::create_dir_all(&target).unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let counted = runner.clone();
        // `stop_on_error` is on by default and a refusal would abort the run before
        // the summary, leaving nothing to read the counts off. The counts are what
        // this test is about.
        let service = dirs.service_with_runner_and_stop_on_error(runner, false);

        let options = ApplyOptions {
            dry_run: true,
            ..Default::default()
        };
        let events = collect_events(service.apply_all(options).await).await;

        assert_eq!(counted.call_count(), 0, "a preview must run no command");
        let warnings = warning_messages(&events);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("a file cannot replace a directory")),
            "a preview must refuse what a real run refuses: {warnings:?}"
        );

        let result = get_operation_result(&events).expect("the run must complete");
        match result {
            OperationResult::Success(OperationSuccess::DotfilesApplied {
                refused_count,
                skipped_count,
                ..
            }) => {
                assert_eq!(*refused_count, 1, "counted as a refusal");
                assert_eq!(*skipped_count, 0, "not counted as a skip as well");
            }
            other => panic!("expected DotfilesApplied, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn non_utf8_provider_output_survives_byte_exact() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("id_ed25519");
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read key");

        let bytes = [0x00u8, 0xff, 0xfe, 0x0a];
        let runner = FakeCommandRunner::new().succeeding("op read key", &bytes);
        let service = dirs.service_with_runner(runner);

        let _ = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert_eq!(std::fs::read(&target).unwrap(), bytes);
    }

    #[tokio::test]
    async fn a_template_renders_its_bindings() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        template_package(
            &dirs.package_dir,
            target.to_str().unwrap(),
            "key: {{ api_key }}\ncorp: {{ corp }}\n",
            &[("api_key", "op read a"), ("corp", "teller get B")],
        );

        let runner = FakeCommandRunner::new()
            .succeeding("op read a", SECRET.as_bytes())
            .succeeding("teller get B", b"corp-token");
        let service = dirs.service_with_runner(runner);

        let _ = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            format!("key: {SECRET}\ncorp: corp-token\n")
        );
    }

    #[tokio::test]
    async fn two_secret_entries_do_not_share_values() {
        // Each entry's bindings must be built fresh. A binding map reused across
        // entries would splice one entry's secret into the other's file.
        let dirs = TestDirs::new();
        std::fs::create_dir_all(dirs.package_dir.join("creds")).unwrap();
        std::fs::write(dirs.package_dir.join("creds/a.tpl"), "value: {{ va }}\n").unwrap();
        // b.tpl references a name it does not declare — `va` belongs to the first
        // entry. Per-entry bindings leave it verbatim; a binding map reused across
        // entries would resolve it and splice the first entry's secret in here.
        // The two entries must use *different* names for this to be observable at
        // all: with a shared name the second binding simply overwrites the first.
        std::fs::write(
            dirs.package_dir.join("creds/b.tpl"),
            "value: {{ vb }}\nborrowed: {{ va }}\n",
        )
        .unwrap();

        let target_a = dirs.target_dir.join("a.conf");
        let target_b = dirs.target_dir.join("b.conf");
        let yaml = format!(
            "name: creds\nenvironments:\n  test:\n    install: \"echo i\"\ndotfiles:\n  \
             - source: \"creds/a.tpl\"\n    target: \"{}\"\n    vars:\n      va: \"read-a\"\n  \
             - source: \"creds/b.tpl\"\n    target: \"{}\"\n    vars:\n      vb: \"read-b\"\n",
            target_a.display(),
            target_b.display()
        );
        std::fs::write(dirs.package_dir.join("creds.yml"), yaml).unwrap();

        let runner = FakeCommandRunner::new()
            .succeeding("read-a", b"AAAAAAAA")
            .succeeding("read-b", b"BBBBBBBB");
        let service = dirs.service_with_runner(runner);

        let _ = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        let a = std::fs::read_to_string(&target_a).unwrap();
        let b = std::fs::read_to_string(&target_b).unwrap();
        assert_eq!(a, "value: AAAAAAAA\n");
        assert_eq!(
            b, "value: BBBBBBBB\nborrowed: {{ va }}\n",
            "the first entry's binding must not be visible to the second"
        );
        assert!(!b.contains("AAAAAAAA"), "a's value bled into b: {b}");
    }

    #[tokio::test]
    async fn a_failing_provider_stops_the_apply_when_stop_on_error_is_set() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().failing("op read x", b"not logged in");
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert!(
            matches!(
                get_operation_result(&events),
                Some(OperationResult::Failure(_))
            ),
            "stop_on_error defaults to true, so a failed resolve aborts"
        );
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn a_failing_provider_reports_stderr() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().failing("op read x", b"not logged in");
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert!(
            format!("{events:?}").contains("not logged in"),
            "a failure must stay diagnosable"
        );
    }

    #[tokio::test]
    async fn stderr_from_a_succeeding_provider_never_surfaces() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        // A provider run with a verbose flag can echo secret material to stderr.
        let runner = FakeCommandRunner::new().succeeding_noisy(
            "op read x",
            b"content",
            format!("debug: token={SECRET}").as_bytes(),
        );
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert_no_event_mentions(&events, SECRET);
    }

    #[tokio::test]
    async fn empty_provider_output_is_an_error_and_does_not_truncate_the_target() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::fs::write(&target, "existing credential").unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", b"");
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert!(format!("{events:?}").contains("produced no output"));
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "existing credential"
        );
    }

    #[tokio::test]
    async fn a_dry_run_executes_no_command_at_all() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner.clone());

        let options = ApplyOptions {
            dry_run: true,
            ..Default::default()
        };
        let events = collect_events(service.apply_all(options).await).await;

        // Resolving is what runs the user's commands, and a preview must not do
        // that — it reaches a secret store and can raise a biometric prompt.
        assert_eq!(
            runner.call_count(),
            0,
            "--dry-run must not execute a provider command: {:?}",
            runner.calls()
        );
        assert!(!target.exists(), "dry run must not write");
        assert!(
            events.iter().any(|e| matches!(
                e,
                PackageEvent::DotfileSkipped { reason, .. } if reason.contains("dry run")
            )),
            "the dry run should still report the entry, got: {events:?}"
        );
        assert_no_event_mentions(&events, SECRET);
    }

    #[tokio::test]
    async fn an_invalid_entry_is_refused_rather_than_guessed_at() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        let yaml = format!(
            "name: creds\nenvironments:\n  test:\n    install: \"echo i\"\ndotfiles:\n  \
             - source: \"a.tpl\"\n    command: \"op read x\"\n    target: \"{}\"\n",
            target.display()
        );
        std::fs::write(dirs.package_dir.join("creds.yml"), yaml).unwrap();

        let runner = FakeCommandRunner::new();
        let service = dirs.service_with_runner(runner.clone());

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert!(
            format!("{events:?}").contains("exactly one of"),
            "got: {events:?}"
        );
        assert_eq!(runner.call_count(), 0, "an invalid entry must run nothing");
        assert!(!target.exists());
    }

    // ─── Enumeration must not resolve ───────────────────────────────────────

    #[tokio::test]
    async fn a_drift_check_executes_no_binding() {
        let dirs = TestDirs::new();
        let provider_target = dirs.target_dir.join("provider.conf");
        let template_target = dirs.target_dir.join("template.conf");

        std::fs::create_dir_all(dirs.package_dir.join("creds")).unwrap();
        std::fs::write(dirs.package_dir.join("creds/t.tpl"), "key: {{ v }}\n").unwrap();
        let yaml = format!(
            "name: creds\nenvironments:\n  test:\n    install: \"echo i\"\ndotfiles:\n  \
             - command: \"op read x\"\n    target: \"{}\"\n  \
             - source: \"creds/t.tpl\"\n    target: \"{}\"\n    vars:\n      v: \"op read y\"\n",
            provider_target.display(),
            template_target.display()
        );
        std::fs::write(dirs.package_dir.join("creds.yml"), yaml).unwrap();

        // Scripted so that a resolve attempt would succeed rather than error —
        // the assertion is that it never happens, not that it fails.
        let runner = FakeCommandRunner::new()
            .succeeding("op read x", SECRET.as_bytes())
            .succeeding("op read y", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner.clone());

        let events = collect_events(service.check_drift().await).await;

        assert_eq!(
            runner.call_count(),
            0,
            "a read-only operation must not run a provider command: {:?}",
            runner.calls()
        );
        assert_no_event_mentions(&events, SECRET);
    }

    #[tokio::test]
    async fn a_drift_check_reports_secret_entries_without_counting_them_as_drift() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.check_drift().await).await;

        assert!(
            events.iter().any(|e| matches!(
                e,
                PackageEvent::DotfileSkipped { reason, .. } if reason.contains("provider-sourced")
            )),
            "secret entries should be identified, got: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, PackageEvent::DotfileDriftDetected { .. })),
            "counting them as drift would make sync status permanently dirty"
        );

        // The summary must agree: zero drift, not "one unverifiable therefore one
        // drifted".
        match get_operation_result(&events) {
            Some(OperationResult::Success(OperationSuccess::DotfileDriftChecked {
                drift_count,
                ..
            })) => assert_eq!(*drift_count, 0),
            other => panic!("expected a drift summary, got: {other:?}"),
        }
    }

    // ─── Leak regression ────────────────────────────────────────────────────

    #[tokio::test]
    async fn no_event_carries_the_secret_across_a_full_apply() {
        // Covers deploy, in-sync skip, and conflict in one run, for both a
        // whole-file provider and a templated entry.
        for existing in [None, Some(SECRET), Some("hand-edited")] {
            let dirs = TestDirs::new();
            let provider_target = dirs.target_dir.join("provider.conf");
            let template_target = dirs.target_dir.join("template.conf");

            std::fs::create_dir_all(dirs.package_dir.join("creds")).unwrap();
            std::fs::write(dirs.package_dir.join("creds/t.tpl"), "key: {{ v }}\n").unwrap();
            let yaml = format!(
                "name: creds\nenvironments:\n  test:\n    install: \"echo i\"\ndotfiles:\n  \
                 - command: \"op read x\"\n    target: \"{}\"\n  \
                 - source: \"creds/t.tpl\"\n    target: \"{}\"\n    vars:\n      v: \"op read y\"\n",
                provider_target.display(),
                template_target.display()
            );
            std::fs::write(dirs.package_dir.join("creds.yml"), yaml).unwrap();

            if let Some(content) = existing {
                std::fs::write(&provider_target, content).unwrap();
                std::fs::write(&template_target, content).unwrap();
            }

            let runner = FakeCommandRunner::new()
                .succeeding("op read x", SECRET.as_bytes())
                .succeeding("op read y", SECRET.as_bytes());
            let service = dirs.service_with_runner(runner);

            let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

            assert_no_event_mentions(&events, SECRET);

            // Positive control: the run really did handle the secret, so this
            // cannot be passing because nothing happened.
            assert!(
                events.iter().any(|e| matches!(
                    e,
                    PackageEvent::DotfileDeployed { .. }
                        | PackageEvent::DotfileSkipped { .. }
                        | PackageEvent::DotfileConflict { .. }
                )),
                "no dotfile outcome was produced for existing={existing:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_secret_conflict_event_reports_structure_only() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::fs::write(&target, "line one\nline two\nline three\n").unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        let conflict = events
            .iter()
            .find_map(|e| match e {
                PackageEvent::DotfileConflict { diff, .. } => Some(diff),
                _ => None,
            })
            .expect("expected a conflict event");

        assert!(conflict.contains("lines"), "got: {conflict}");
        assert!(conflict.contains("content hidden"), "got: {conflict}");
        test_common::assert_secret_free(conflict, SECRET, "the conflict diff");
        assert!(
            conflict.contains("op read x"),
            "the command is a reference, not a credential, and should be shown: {conflict}"
        );
    }

    // Every other overwrite selfie performs keeps a copy of what it displaced, so
    // a user who has seen that said elsewhere would assume this one does too.
    // Accepting is the only way past a secret conflict and there is no undo.
    #[tokio::test]
    async fn a_secret_conflict_says_no_copy_is_kept() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::fs::write(&target, "hand-edited credential").unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        let summary = events
            .iter()
            .find_map(|event| match event {
                PackageEvent::DotfileConflict { diff, .. } => Some(diff),
                _ => None,
            })
            .expect("a conflict must be reported");

        assert!(
            summary.contains("no copy of the current target is kept"),
            "got: {summary}"
        );
        assert!(
            !summary.contains("copied aside"),
            "nothing is copied aside here: {summary}"
        );
        test_common::assert_secret_free(summary, SECRET, "the conflict summary");
    }

    #[tokio::test]
    async fn auto_accept_does_not_overwrite_a_secret_target() {
        // `auto_accept` is caller-settable — the MCP server exposes it to an
        // assistant — so honoring it would let a non-interactive caller silently
        // overwrite a hand-edited credentials file. The spec requires provider
        // conflicts to be reported and skipped without an interactive resolver,
        // whatever auto_accept says.
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::fs::write(&target, "hand-edited credential").unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let options = ApplyOptions {
            auto_accept: true,
            ..Default::default()
        };
        let events = collect_events(service.apply_all(options).await).await;

        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "hand-edited credential",
            "auto_accept must not force-overwrite a secret-bearing target"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, PackageEvent::DotfileConflict { .. })),
            "the conflict must still be reported, got: {events:?}"
        );
        assert_no_event_mentions(&events, SECRET);
    }

    // ADR-0003 keeps nothing derived from a credential on disk, and the content a
    // secret target already held is the credential itself — worse to persist than
    // the checksum the ADR already refuses. Owner-only permissions do not change
    // that, and the only way past the conflict is a human answering a prompt,
    // where a CLI may offer to show them both values first.
    #[tokio::test]
    async fn a_secret_target_is_not_backed_up() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::fs::write(&target, "previous-credential-DO-NOT-KEEP").unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(accepting()).await).await;

        // Control: the resolver accepted and the overwrite happened, so the run
        // really did reach the point where a copy would have been made.
        assert_eq!(std::fs::read_to_string(&target).unwrap(), SECRET);
        assert!(
            !dirs.state_dir.join("backups").exists(),
            "no copy of a credential may be left on disk"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, PackageEvent::DotfileDeployed { backup: None, .. })),
            "the deployment must report that nothing was kept: {events:?}"
        );
        assert_no_event_mentions(&events, SECRET);
        assert_no_event_mentions(&events, "previous-credential-DO-NOT-KEEP");
    }

    #[tokio::test]
    async fn auto_accept_still_overwrites_an_ordinary_repo_file() {
        // The guard above is specific to secret-bearing entries; `--yes` keeps
        // working for ordinary dotfiles, which have a diff and a recorded state.
        let dirs = TestDirs::new();
        let source_dir = dirs.package_dir.join("myapp");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(source_dir.join("config.toml"), "key = \"from-repo\"").unwrap();
        let target = dirs.target_dir.join("config.toml");
        std::fs::write(&target, "key = \"hand-edited\"").unwrap();
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );

        let service = dirs.service();
        let options = ApplyOptions {
            auto_accept: true,
            ..Default::default()
        };
        let _ = collect_events(service.apply_all(options).await).await;

        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "key = \"from-repo\""
        );
    }

    #[tokio::test]
    async fn an_existing_but_unreadable_target_is_not_silently_overwritten() {
        use std::os::unix::fs::PermissionsExt as _;

        // An unreadable file is still a file, and it may be the very credential an
        // overwrite would destroy. Treating "cannot read" as "not there" would
        // deploy over it with no prompt.
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::fs::write(&target, "existing credential").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o000)).unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert!(
            events
                .iter()
                .any(|e| matches!(e, PackageEvent::DotfileConflict { .. })),
            "an unreadable target must be a conflict, got: {events:?}"
        );

        // Restore permissions so the content can be checked.
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "existing credential",
            "the unreadable target must not have been overwritten"
        );
        assert_no_event_mentions(&events, SECRET);
    }

    #[tokio::test]
    async fn an_unreadable_target_conflict_says_so_rather_than_reporting_zero_lines() {
        use std::os::unix::fs::PermissionsExt as _;

        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::fs::write(&target, "existing credential").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o000)).unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();

        let summary = events
            .iter()
            .find_map(|e| match e {
                PackageEvent::DotfileConflict { diff, .. } => Some(diff),
                _ => None,
            })
            .expect("expected a conflict event");

        assert!(
            summary.contains("could not be read"),
            "an empty-looking '0 lines' would understate what an overwrite \
             destroys, got: {summary}"
        );
    }

    #[tokio::test]
    async fn a_template_source_escaping_the_package_directory_is_refused() {
        // Apply never runs validation, so the static `..` check on `source` is not
        // a gate. Without a runtime containment check, a crafted template source
        // splices the contents of a file outside the package directory into a
        // deployed dotfile.
        let dirs = TestDirs::new();
        let outside = dirs._temp.path().join("outside.tpl");
        std::fs::write(&outside, "STOLEN-FROM-OUTSIDE: {{ v }}\n").unwrap();

        let target = dirs.target_dir.join("credentials");
        let yaml = format!(
            "name: creds\nenvironments:\n  test:\n    install: \"echo i\"\ndotfiles:\n  \
             - source: \"../outside.tpl\"\n    target: \"{}\"\n    vars:\n      v: \"op read x\"\n",
            target.display()
        );
        std::fs::write(dirs.package_dir.join("creds.yml"), yaml).unwrap();

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert!(!target.exists(), "the escaping template must not deploy");
        assert!(
            format!("{events:?}").contains("escapes the package directory"),
            "expected a containment refusal, got: {events:?}"
        );
        assert!(
            !format!("{events:?}").contains("STOLEN-FROM-OUTSIDE"),
            "the outside file's contents must not surface"
        );
    }

    #[tokio::test]
    async fn a_repo_file_source_escaping_the_package_directory_is_still_refused() {
        // The pre-existing guard on the ordinary path, asserted here so moving it
        // into a shared module cannot quietly drop it.
        let dirs = TestDirs::new();
        let outside = dirs._temp.path().join("outside.conf");
        std::fs::write(&outside, "outside content").unwrap();

        let target = dirs.target_dir.join("escaped.conf");
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("../outside.conf", target.to_str().unwrap())],
        );

        let service = dirs.service();
        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert!(!target.exists(), "the escaping source must not deploy");
        assert!(
            format!("{events:?}").contains("escapes YAML base directory"),
            "got: {events:?}"
        );
    }

    #[tokio::test]
    async fn a_dry_run_refuses_a_relative_target_the_same_way_a_real_apply_does() {
        // The dry-run short-circuit must sit after the checks that would refuse
        // the entry outright, or a preview claims it "would run N commands" for
        // something a real apply would never touch.
        let dirs = TestDirs::new();
        let yaml = "name: creds\nenvironments:\n  test:\n    install: \"echo i\"\ndotfiles:\n  \
                    - command: \"op read x\"\n    target: \"relative/credentials\"\n";
        std::fs::write(dirs.package_dir.join("creds.yml"), yaml).unwrap();

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner.clone());

        let options = ApplyOptions {
            dry_run: true,
            ..Default::default()
        };
        let events = collect_events(service.apply_all(options).await).await;

        assert_eq!(runner.call_count(), 0);
        assert!(
            format!("{events:?}").contains("is not absolute"),
            "a dry run should report the same refusal a real apply would, got: {events:?}"
        );
        // The refusal is a failure, not a skip, so `stop_on_error` (default true)
        // ends the preview here — which is what `docs/package-files.md` promises
        // and what nothing asserted before (selfie-m5dv).
        assert!(
            matches!(
                get_operation_result(&events).expect("no Completed event"),
                OperationResult::Failure(_)
            ),
            "got: {events:?}"
        );
        assert!(
            !format!("{events:?}").contains("would run"),
            "must not claim it would run commands for an entry that can never deploy"
        );
    }

    #[tokio::test]
    async fn a_dry_run_refuses_an_escaping_template_the_same_way_a_real_apply_does() {
        // Containment is decidable from the path alone, so the preview can and must
        // apply it. Same rule as the relative-target case above: a dry run that says
        // it "would run 1 command(s)" for an entry a real apply refuses outright is
        // describing something that will never happen.
        let dirs = TestDirs::new();
        let outside = dirs._temp.path().join("outside.tpl");
        std::fs::write(&outside, "STOLEN: {{ v }}\n").unwrap();

        let target = dirs.target_dir.join("credentials");
        let yaml = format!(
            "name: creds\nenvironments:\n  test:\n    install: \"echo i\"\ndotfiles:\n  \
             - source: \"../outside.tpl\"\n    target: \"{}\"\n    vars:\n      v: \"op read x\"\n",
            target.display()
        );
        std::fs::write(dirs.package_dir.join("creds.yml"), yaml).unwrap();

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner.clone());

        let options = ApplyOptions {
            dry_run: true,
            ..Default::default()
        };
        let events = collect_events(service.apply_all(options).await).await;

        assert_eq!(runner.call_count(), 0);
        assert!(
            format!("{events:?}").contains("escapes the package directory"),
            "a dry run should report the same refusal a real apply would, got: {events:?}"
        );
        assert!(
            !format!("{events:?}").contains("would run"),
            "must not claim it would run commands for an entry that can never deploy"
        );
    }

    #[tokio::test]
    async fn stopping_on_error_still_records_what_was_already_deployed() {
        // An abort must not discard the deploy state for files already written in
        // the same run: the files are on disk, so dropping their record would make
        // the next drift check report correctly-deployed files as untracked.
        let dirs = TestDirs::new();

        // Relies on packages being enumerated in sorted path order, so "aaa"
        // is processed before "zzz" and the ordinary dotfile deploys before the
        // provider fails. That ordering is a guarantee of the repository, pinned
        // by `list_yaml_files_returns_them_in_sorted_order` — it is not an
        // assumption about the filesystem. It was exactly that before, and CI on
        // ext4 (hash order, unlike APFS) deployed "zzz" first and failed here.
        let source_dir = dirs.package_dir.join("aaa");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();
        let ok_target = dirs.target_dir.join("config.toml");
        create_package_with_dotfiles(
            &dirs.package_dir,
            "aaa",
            &[("aaa/config.toml", ok_target.to_str().unwrap())],
        );

        let bad_target = dirs.target_dir.join("credentials");
        let yaml = format!(
            "name: zzz\nenvironments:\n  test:\n    install: \"echo i\"\ndotfiles:\n  \
             - command: \"op read x\"\n    target: \"{}\"\n",
            bad_target.display()
        );
        std::fs::write(dirs.package_dir.join("zzz.yml"), yaml).unwrap();

        let runner = FakeCommandRunner::new().failing("op read x", b"not logged in");
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert!(
            matches!(
                get_operation_result(&events),
                Some(OperationResult::Failure(_))
            ),
            "the run should still report failure"
        );
        assert_eq!(
            std::fs::read_to_string(&ok_target).unwrap(),
            "key = \"value\"",
            "the earlier dotfile really was deployed"
        );

        let state = std::fs::read_to_string(state_file(&dirs)).unwrap_or_default();
        assert!(
            state.contains("aaa/config.toml"),
            "the successful deployment must still be recorded, got: {state}"
        );
    }

    #[tokio::test]
    async fn a_misspelled_dotfile_key_is_skipped_with_a_warning_while_the_package_still_applies() {
        // `var:` for `vars:` leaves a template indistinguishable from a plain
        // repository file. Deploying it would write the *unrendered* template —
        // literal `{{ api_key }}` — over the credentials target and record that
        // content in deploy state, so the entry has to be refused outright.
        //
        // The template file really exists and really contains a placeholder: if
        // the entry were treated as a repository file the target would be written
        // with that body, so `exists=false` can only mean the entry was skipped.
        // A missing template would make this pass for the wrong reason.
        let dirs = TestDirs::new();
        std::fs::create_dir_all(dirs.package_dir.join("creds")).unwrap();
        std::fs::write(
            dirs.package_dir.join("creds/t.tpl"),
            "api_key = \"{{ api_key }}\"\n",
        )
        .unwrap();

        let bad_target = dirs.target_dir.join("credentials");
        let good_target = dirs.target_dir.join("bat.conf");
        std::fs::write(dirs.package_dir.join("bat.conf"), "fine\n").unwrap();

        let yaml = format!(
            "name: creds\nenvironments:\n  test:\n    install: \"echo i\"\ndotfiles:\n  \
             - source: \"creds/t.tpl\"\n    target: \"{}\"\n    var:\n      api_key: \"op read x\"\n  \
             - source: \"bat.conf\"\n    target: \"{}\"\n",
            bad_target.display(),
            good_target.display()
        );
        std::fs::write(dirs.package_dir.join("creds.yml"), yaml).unwrap();

        let service = dirs.service();
        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert!(
            !bad_target.exists(),
            "the typo'd entry must not deploy, got: {:?}",
            std::fs::read_to_string(&bad_target)
        );
        assert_eq!(
            std::fs::read_to_string(&good_target).unwrap(),
            "fine\n",
            "the package's other dotfile must still deploy"
        );

        let rendered = format!("{events:?}");
        assert!(
            rendered.contains(bad_target.to_str().unwrap()),
            "the warning must name the skipped target, got: {events:?}"
        );
    }

    #[tokio::test]
    async fn an_unparsable_package_is_named_rather_than_silently_dropped() {
        // `valid_packages()` drops parse failures. Without a warning, a package
        // directory holding exactly one unparsable package produces a successful
        // apply that deployed nothing — and the user's credentials dotfile
        // quietly stops deploying, surfacing later as an auth failure nobody
        // traces back to the package file.
        //
        // The fixture is malformed YAML rather than a schema violation on
        // purpose: this test was defanged once already when the schema changed
        // under it, and a syntax error cannot stop being a parse failure.
        let dirs = TestDirs::new();
        std::fs::write(
            dirs.package_dir.join("creds.yml"),
            "name: creds\ndotfiles:\n  - [unclosed\n",
        )
        .unwrap();

        let service = dirs.service();
        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        // The event, not its rendering. Apply's job here is to report the file at
        // all; what a reader sees is asserted where a reader sees it, in
        // `crates/cli/tests/skipped_spec_tests.rs`, against real output.
        //
        // Never assert on `format!("{events:?}")` here. A Debug of a typed payload
        // resembles nothing a user reads, so such an assertion passes while the CLI
        // prints something else entirely.
        let skipped: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                PackageEvent::SpecSkipped { error, .. } => Some(error),
                _ => None,
            })
            .collect();

        assert_eq!(skipped.len(), 1, "got: {events:?}");
        assert_eq!(
            skipped[0].package_path(),
            dirs.package_dir.join("creds.yml")
        );
        assert!(
            matches!(
                skipped[0].kind(),
                selfie::package::port::PackageParseKind::Yaml { .. }
            ),
            "the reason must survive, not just the fact: got {:?}",
            skipped[0].kind()
        );
    }

    #[tokio::test]
    async fn a_valid_package_still_applies_alongside_an_unparsable_one() {
        // The warning must not become an abort: one bad file should not stop the
        // rest of the directory deploying.
        //
        // Malformed YAML, not a schema violation: the previous fixture
        // (`- nope: 1`) was unparsable only incidentally, because it omitted the
        // required `target` — so it kept passing while testing something other
        // than what its name claims.
        let dirs = TestDirs::new();
        std::fs::write(
            dirs.package_dir.join("broken.yml"),
            "name: broken\ndotfiles:\n  - [unclosed\n",
        )
        .unwrap();

        let good_target = dirs.target_dir.join("credentials");
        provider_package(
            &dirs.package_dir,
            good_target.to_str().unwrap(),
            "op read x",
        );

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert_eq!(std::fs::read_to_string(&good_target).unwrap(), SECRET);
        assert!(
            format!("{events:?}").contains("broken.yml"),
            "the unparsable file must still be named: {events:?}"
        );
        assert_no_event_mentions(&events, SECRET);
    }

    // ─── Leak regression: the failure path ──────────────────────────────────

    #[tokio::test]
    async fn a_failing_provider_does_not_leak_its_stdout() {
        // A provider's stdout IS the secret. Two separate things keep it out of
        // the event stream: this path reports failures with its own error type,
        // which carries the command and its stderr and never the output, and
        // `CommandFailure::ExecutionFailed` has no `stdout` field for a failure to
        // be routed into. This test covers the first — prefer the resolve path's
        // own variants over `OperationFailure::from` regardless, since those name
        // the entry and the var rather than only saying a command failed.
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().failing_with_stdout(
            "op read x",
            SECRET.as_bytes(),
            b"error: vault sealed",
        );
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert_no_event_mentions(&events, SECRET);

        // Positive control: the failure really was reported, so this is not
        // passing because nothing happened.
        assert!(
            format!("{events:?}").contains("vault sealed"),
            "the failure must stay diagnosable: {events:?}"
        );
    }

    #[tokio::test]
    async fn a_zero_length_output_failure_does_not_leak_stderr() {
        // Empty stdout is an error, and on that path stderr was never a failure
        // signal — the command exited zero — so it must not be forwarded either.
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding_noisy(
            "op read x",
            b"",
            format!("debug: retrieved token={SECRET}").as_bytes(),
        );
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert_no_event_mentions(&events, SECRET);
        assert!(
            format!("{events:?}").contains("produced no output"),
            "the empty-output error must still be reported: {events:?}"
        );
    }

    #[tokio::test]
    async fn a_failing_binding_does_not_leak_its_stdout() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        template_package(
            &dirs.package_dir,
            target.to_str().unwrap(),
            "key: {{ api_key }}\n",
            &[("api_key", "op read a")],
        );

        let runner = FakeCommandRunner::new().failing_with_stdout(
            "op read a",
            SECRET.as_bytes(),
            b"error: not logged in",
        );
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert_no_event_mentions(&events, SECRET);
        assert!(format!("{events:?}").contains("not logged in"));
    }

    // A resolver that records what it was handed and always accepts.
    #[derive(Default)]
    struct RecordingResolver {
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl ConflictResolver for RecordingResolver {
        fn resolve(&self, _target: &str, detail: ConflictDetail<'_>) -> ConflictResolution {
            // Stands in for an interactive adapter that offers `[r]eveal`.
            if let ConflictDetail::Secret {
                incoming, current, ..
            } = detail
            {
                let mut seen = self.seen.lock().unwrap();
                seen.push(String::from_utf8_lossy(incoming).into_owned());
                seen.push(String::from_utf8_lossy(current).into_owned());
            }
            ConflictResolution::Accept
        }
    }

    #[tokio::test]
    async fn a_resolver_receives_the_values_but_events_still_do_not() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::fs::write(&target, "previous-credential").unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let resolver = Arc::new(RecordingResolver::default());
        let options = ApplyOptions {
            conflict_resolver: Some(resolver.clone()),
            ..Default::default()
        };
        let events = collect_events(service.apply_all(options).await).await;

        let seen = resolver.seen.lock().unwrap().clone();
        assert!(
            seen.iter().any(|v| v.contains(SECRET)),
            "the resolver is the one place the values may go, but it saw: {seen:?}"
        );
        assert!(
            seen.iter().any(|v| v.contains("previous-credential")),
            "the resolver should see both sides, saw: {seen:?}"
        );

        assert_no_event_mentions(&events, SECRET);
    }

    // ─── Entries refused before anything runs (selfie-n310/3c5a/kj5y) ────────
    //
    // A consumer can drop a refused entry and still compile — `let Ok(..) else
    // { continue }` builds — which would leave the user a dotfile that never
    // deploys and no diagnostic. One test per consumer path asserts the refusal
    // is reported, so a consumer that swallows it fails a test.

    // A template whose var name cannot be substituted, plus its template file.
    //
    // The template really exists and really contains the placeholder: if the
    // entry were treated as any kind of deployable entry the target would be
    // written, so `!target.exists()` can only mean it was refused.
    fn bad_var_name_package(package_dir: &std::path::Path, target: &str, var: &str) {
        std::fs::create_dir_all(package_dir.join("creds")).unwrap();
        std::fs::write(
            package_dir.join("creds/credentials.tpl"),
            format!("api_key: {{{{ {var} }}}}\n"),
        )
        .unwrap();

        let yaml = format!(
            "name: creds\nenvironments:\n  test:\n    install: \"echo i\"\ndotfiles:\n  \
             - source: \"creds/credentials.tpl\"\n    target: \"{target}\"\n    vars:\n      \
             \"{var}\": \"op read x\"\n"
        );
        std::fs::write(package_dir.join("creds.yml"), yaml).unwrap();
    }

    #[tokio::test]
    async fn a_var_name_that_cannot_be_substituted_runs_no_command() {
        // selfie-3c5a. `template::render` cannot substitute `not-a-name`, so this
        // entry provably cannot produce the file it describes. Nothing may run for
        // it: `op read x` is a REAL credential fetch that can raise a biometric or
        // password prompt, and the value would be fetched, discarded, and the
        // placeholder deployed verbatim over the credentials target.
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        bad_var_name_package(&dirs.package_dir, target.to_str().unwrap(), "not-a-name");

        // Scripted to succeed: the assertion is that the fetch never happens, not
        // that it fails.
        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner.clone());

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert_eq!(
            runner.call_count(),
            0,
            "no command may run for a binding that cannot be substituted: {:?}",
            runner.calls()
        );
        assert!(!target.exists(), "the entry must not deploy");
        assert!(
            format!("{events:?}").contains("not-a-name"),
            "the refusal must name the var, got: {events:?}"
        );
        assert_no_event_mentions(&events, SECRET);
    }

    #[tokio::test]
    async fn a_valid_var_name_still_runs_its_command_and_renders_it() {
        // The control for the test above. `FakeCommandRunner` records every call
        // unconditionally, before it looks a response up, so a call whose output
        // was discarded would still show in `call_count()` — but only if this path
        // reaches the runner at all. This proves it does, on a fixture differing
        // by one character. Asserting the *rendered* content matters too:
        // "a command ran" alone could hold while the entry took another branch.
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        bad_var_name_package(&dirs.package_dir, target.to_str().unwrap(), "not_a_name");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner.clone());

        collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert_eq!(runner.call_count(), 1, "got: {:?}", runner.calls());
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            format!("api_key: {SECRET}\n"),
            "the binding must actually be substituted"
        );
    }

    #[tokio::test]
    async fn an_anchor_colliding_with_a_dotfile_field_is_refused_before_deploying() {
        // selfie-kj5y. `_vars:` was read as a YAML anchor definition and dropped,
        // leaving a template that looked like a plain repository file — so it
        // deployed *unrendered*, literal `{{ api_key }}`, over the credentials
        // target, with the bindings silently absent and `selfie spec validate`
        // reporting nothing at all.
        //
        // Every colliding name is exercised, not `_vars` alone: a fix hard-coded
        // to one of them passes a single-name test.
        for field in ["vars", "source", "command", "target"] {
            let dirs = TestDirs::new();
            std::fs::create_dir_all(dirs.package_dir.join("creds")).unwrap();
            std::fs::write(
                dirs.package_dir.join("creds/credentials.tpl"),
                "api_key: {{ api_key }}\n",
            )
            .unwrap();

            let target = dirs.target_dir.join("credentials");
            let yaml = format!(
                "name: creds\nenvironments:\n  test:\n    install: \"echo i\"\ndotfiles:\n  \
                 - source: \"creds/credentials.tpl\"\n    target: \"{}\"\n    _{field}:\n      \
                 api_key: \"op read x\"\n",
                target.display()
            );
            std::fs::write(dirs.package_dir.join("creds.yml"), yaml).unwrap();

            let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
            let service = dirs.service_with_runner(runner.clone());

            let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

            assert!(
                !target.exists(),
                "'_{field}' must not deploy, got: {:?}",
                std::fs::read_to_string(&target)
            );
            assert_eq!(runner.call_count(), 0, "for _{field}");
            assert!(
                format!("{events:?}").contains("cannot be told apart from a misspelling"),
                "the refusal must name the ambiguity, got: {events:?}"
            );
        }
    }

    #[tokio::test]
    async fn an_anchor_not_named_like_a_dotfile_field_still_deploys() {
        // The control that keeps YAML anchors working, and the reason the rule is
        // a *collision* rule rather than "no underscore keys inside an entry".
        // Consuming the alias proves the key was really parsed, not just
        // tolerated.
        let dirs = TestDirs::new();
        std::fs::write(dirs.package_dir.join("bat.conf"), "fine\n").unwrap();

        let target = dirs.target_dir.join("bat.conf");
        let yaml = format!(
            "name: bat\nenvironments:\n  test:\n    install: \"echo i\"\ndotfiles:\n  \
             - _anchor: &a \"bat.conf\"\n    source: *a\n    target: \"{}\"\n",
            target.display()
        );
        std::fs::write(dirs.package_dir.join("bat.yml"), yaml).unwrap();

        let service = dirs.service();
        collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "fine\n");
    }

    #[tokio::test]
    async fn a_drift_check_reports_a_refused_entry_rather_than_calling_it_unverifiable() {
        // The drift consumer. A refused entry reported as "provider-sourced (not
        // verifiable without resolving)" would be filed under the one status a
        // user is trained to ignore, hiding a dotfile that can never deploy.
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        bad_var_name_package(&dirs.package_dir, target.to_str().unwrap(), "not-a-name");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner.clone());

        let events = collect_events(service.check_drift().await).await;

        assert_eq!(runner.call_count(), 0, "got: {:?}", runner.calls());
        assert!(
            format!("{events:?}").contains("not-a-name"),
            "drift must name the refused entry, got: {events:?}"
        );
        assert!(
            !events.iter().any(|e| matches!(
                e,
                PackageEvent::DotfileSkipped { reason, .. } if reason.contains("provider-sourced")
            )),
            "an undeployable entry is not merely unverifiable, got: {events:?}"
        );
    }

    // Ctrl+C during `selfie apply`.
    //
    // Apply gained the ability to run commands after the cancellation token was
    // threaded through the rest of the service layer, and did not inherit it:
    // the resolve path built a fresh `CancellationToken` nobody held, so an `op
    // read` blocked on a biometric prompt could only be escaped by waiting out
    // `command_timeout`.
    mod cancellation {
        use super::*;
        use selfie::commands::{
            CommandError, CommandOutput, CommandRunner, ContentOutput, OutputChunk,
        };
        use std::path::Path;
        use std::time::Duration;

        // What the runner should do on a given call.
        #[derive(Clone, Copy, Debug, Default)]
        struct CallEffect {
            // Cancel the token the *service* holds, as a signal handler would
            // while this command is in flight.
            cancel: bool,
            // Report the command as killed, which is what `ShellCommandRunner`
            // does once the token fires.
            fail_as_cancelled: bool,
        }

        // A runner that records the token it is **handed** and cancels the token
        // the service was **built with**.
        //
        // Those being two different things is the entire mechanism.
        // `resolve_content` reuses one `&CancellationToken` for every command of
        // an entry, so a fake that cancelled the token it was handed would cancel
        // a placeholder just as readily, and every assertion below would hold with
        // the bug in place. An independent clone of the real token makes
        // `observed` differ between the two worlds.
        #[derive(Clone, Debug)]
        struct TokenObservingRunner {
            // The token the service was constructed with.
            real_token: CancellationToken,
            // `is_cancelled()` of the token handed to each call, in call order.
            observed: Arc<Mutex<Vec<bool>>>,
            // Commands seen, in call order.
            calls: Arc<Mutex<Vec<String>>>,
            // Indexed by call number.
            effects: Vec<CallEffect>,
            // Refuse any call handed an already-cancelled token, as
            // `ShellCommandRunner::run_buffered` does in its pre-spawn check.
            //
            // Off by default so the other tests observe the token without the
            // runner acting on it. On, it makes this fake faithful to the
            // production runner on the one behavior that closes the
            // between-bindings window.
            refuse_when_handed_cancelled: bool,
            // Returned as stdout by any call that is not failing.
            stdout: Vec<u8>,
        }

        impl TokenObservingRunner {
            fn new(real_token: &CancellationToken, effects: Vec<CallEffect>) -> Self {
                Self {
                    real_token: real_token.clone(),
                    observed: Arc::new(Mutex::new(Vec::new())),
                    calls: Arc::new(Mutex::new(Vec::new())),
                    effects,
                    refuse_when_handed_cancelled: false,
                    stdout: SECRET.as_bytes().to_vec(),
                }
            }

            // Behave like the real shell runner: refuse a pre-cancelled token.
            fn refusing_a_cancelled_token(mut self) -> Self {
                self.refuse_when_handed_cancelled = true;
                self
            }

            // Cancel while the *first* command is in flight, and report that
            // command as killed — a faithful Ctrl+C.
            fn cancelling_and_failing(real_token: &CancellationToken) -> Self {
                Self::new(
                    real_token,
                    vec![CallEffect {
                        cancel: true,
                        fail_as_cancelled: true,
                    }],
                )
            }

            fn observed(&self) -> Vec<bool> {
                self.observed.lock().unwrap().clone()
            }

            fn calls(&self) -> Vec<String> {
                self.calls.lock().unwrap().clone()
            }

            fn answer(
                &self,
                command: &str,
                handed: &CancellationToken,
            ) -> Result<CommandOutput, CommandError> {
                let index = {
                    let mut calls = self.calls.lock().unwrap();
                    calls.push(command.to_string());
                    calls.len() - 1
                };
                // Recorded before this call's own effect fires, so the value is
                // what the caller handed over rather than what this runner just
                // did.
                self.observed.lock().unwrap().push(handed.is_cancelled());

                // Before this call's own effect, and before any scripted answer:
                // the real runner checks the token before it spawns anything.
                if self.refuse_when_handed_cancelled && handed.is_cancelled() {
                    return Err(CommandError::Cancelled {
                        command: command.to_string(),
                        working_directory: Path::new(".").to_path_buf(),
                    });
                }

                let effect = self.effects.get(index).copied().unwrap_or_default();
                if effect.cancel {
                    self.real_token.cancel();
                }
                if effect.fail_as_cancelled {
                    return Err(CommandError::Cancelled {
                        command: command.to_string(),
                        working_directory: Path::new(".").to_path_buf(),
                    });
                }
                Ok(command_output(self.stdout.clone()))
            }
        }

        impl CommandRunner for TokenObservingRunner {
            async fn is_command_available(&self, _command: &str) -> bool {
                true
            }

            async fn execute(
                &self,
                command: &str,
                token: &CancellationToken,
            ) -> Result<CommandOutput, CommandError> {
                self.answer(command, token)
            }

            async fn execute_with_timeout(
                &self,
                command: &str,
                _timeout: Duration,
                token: &CancellationToken,
            ) -> Result<CommandOutput, CommandError> {
                self.answer(command, token)
            }

            async fn execute_in_dir(
                &self,
                command: &str,
                _working_dir: &Path,
                _timeout: Duration,
                token: &CancellationToken,
            ) -> Result<CommandOutput, CommandError> {
                self.answer(command, token)
            }

            async fn execute_streaming(
                &self,
                command: &str,
                _timeout: Duration,
                _output_sender: tokio::sync::mpsc::Sender<OutputChunk>,
                token: &CancellationToken,
            ) -> Result<CommandOutput, CommandError> {
                self.answer(command, token)
            }

            // The path resolve actually takes, so this is the one these tests
            // observe the token through.
            async fn execute_for_content(
                &self,
                command: &str,
                _working_dir: &Path,
                _timeout: Duration,
                token: &CancellationToken,
            ) -> Result<ContentOutput, CommandError> {
                let output = self.answer(command, token)?;
                let success = output.is_success();
                let stderr = output.stderr().to_vec();
                Ok(ContentOutput::from_parts(
                    success,
                    output.into_stdout(),
                    stderr,
                    0,
                    true,
                ))
            }
        }

        // A successful `CommandOutput` carrying `stdout`.
        fn command_output(stdout: Vec<u8>) -> CommandOutput {
            use std::os::unix::process::ExitStatusExt as _;
            CommandOutput::from_parts(
                std::process::ExitStatus::from_raw(0),
                stdout,
                Vec::new(),
                Duration::ZERO,
            )
        }

        // Assert the run ended in a failure whose message names cancellation.
        #[track_caller]
        fn assert_cancelled(events: &[PackageEvent]) {
            let result = get_operation_result(events)
                .expect("a cancelled run still has to emit its Completed event");
            let OperationResult::Failure(failure) = result else {
                panic!("expected a failure, got: {result:?}");
            };
            let rendered = failure.to_string();
            assert!(
                rendered.to_lowercase().contains("cancel"),
                "the run must report cancellation, got: {rendered}"
            );
        }

        // ── The token has to reach the runner ────────────────────────────────

        // The discriminating test: proves the *service's* token is what a provider
        // command is run with.
        //
        // Two `vars` rather than two entries on purpose. `resolve_content` loops
        // over an entry's bindings with no cancellation check between them, so the
        // second command is dispatched with whatever token was threaded. Two
        // entries would be caught by the between-entries guard instead.
        //
        // If a guard is ever added inside the `vars` loop, this stops
        // discriminating and starts passing for the wrong reason -- rewrite it.
        #[tokio::test]
        async fn the_live_token_reaches_the_command_runner() {
            let dirs = TestDirs::new();
            let target = dirs.target_dir.join("credentials");
            template_package(
                &dirs.package_dir,
                target.to_str().unwrap(),
                "one={{ alpha }}\ntwo={{ beta }}\n",
                &[("alpha", "op read a"), ("beta", "op read b")],
            );

            let token = CancellationToken::new();
            // Cancels during the first binding's command, succeeds for both.
            let runner = TokenObservingRunner::new(
                &token,
                vec![CallEffect {
                    cancel: true,
                    fail_as_cancelled: false,
                }],
            );
            let service = dirs.service_with_runner_and_token(runner.clone(), token);

            let _ = collect_events(service.apply_all(ApplyOptions::default()).await).await;

            // Control: both bindings really ran, so the observation below is of
            // two real dispatches and not of an empty vector.
            assert_eq!(
                runner.calls(),
                vec!["op read a".to_string(), "op read b".to_string()],
                "both bindings must run for this test to observe anything"
            );
            assert_eq!(
                runner.observed(),
                vec![false, true],
                "the second command must be handed the token the first one cancelled; \
                 [false, false] means a fresh token was passed to the resolve path"
            );
        }

        // The third window: cancellation landing *between two bindings*.
        //
        // Neither guard in `handle_apply` covers this. The between-entries guard
        // has already run, the `Failed`-arm guard has not been reached, and
        // `resolve_content` iterates `vars` with no cancellation check.
        //
        // What closes it is the runner: `run_buffered` checks the token before
        // spawning and returns `Cancelled`, which becomes a resolve failure and
        // lands in the `Failed` arm. The fake models that pre-spawn refusal
        // explicitly, so it cannot show the window closed when it is not.
        #[tokio::test]
        async fn a_cancellation_between_two_bindings_is_reported_honestly() {
            let dirs = TestDirs::new();
            let target = dirs.target_dir.join("credentials");
            template_package(
                &dirs.package_dir,
                target.to_str().unwrap(),
                "one={{ alpha }}\ntwo={{ beta }}\n",
                &[("alpha", "op read a"), ("beta", "op read b")],
            );

            let token = CancellationToken::new();
            // Cancels after the first binding succeeds; the second is then handed
            // an already-cancelled token and is refused, as the real runner would.
            let runner = TokenObservingRunner::new(
                &token,
                vec![CallEffect {
                    cancel: true,
                    fail_as_cancelled: false,
                }],
            )
            .refusing_a_cancelled_token();
            let service = dirs.service_with_runner_and_token(runner.clone(), token);

            let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

            // Control: the run really did reach the second binding. Without this,
            // the assertions below would also hold if it had stopped at the first.
            assert_eq!(
                runner.calls(),
                vec!["op read a".to_string(), "op read b".to_string()],
                "the cancellation has to land between the two bindings"
            );

            assert_cancelled(&events);

            let rendered = format!("{events:?}");
            assert!(
                !rendered.contains("stop_on_error"),
                "a mid-bindings cancellation must not be blamed on the spec: {rendered}"
            );
            assert!(
                !target.exists(),
                "a half-resolved template must never be written"
            );
        }

        // ── Between entries ──────────────────────────────────────────────────

        #[tokio::test]
        async fn a_cancelled_token_stops_apply_before_running_a_provider_command() {
            let dirs = TestDirs::new();
            let target = dirs.target_dir.join("credentials");
            provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

            let token = CancellationToken::new();
            token.cancel();
            let runner = TokenObservingRunner::new(&token, Vec::new());
            let service = dirs.service_with_runner_and_token(runner.clone(), token);

            let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

            assert_eq!(
                runner.calls(),
                Vec::<String>::new(),
                "a cancelled run must not start a provider command"
            );
            assert!(
                !target.exists(),
                "nothing may be written after cancellation"
            );
            assert_cancelled(&events);
        }

        // The positive control for the test above.
        //
        // Without it, that test passes just as well if apply were failing for
        // some reason having nothing to do with the token.
        #[tokio::test]
        async fn an_uncancelled_token_runs_the_provider_command() {
            let dirs = TestDirs::new();
            let target = dirs.target_dir.join("credentials");
            provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

            let token = CancellationToken::new();
            let runner = TokenObservingRunner::new(&token, Vec::new());
            let service = dirs.service_with_runner_and_token(runner.clone(), token);

            let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

            assert_eq!(runner.calls(), vec!["op read x".to_string()]);
            assert_eq!(std::fs::read_to_string(&target).unwrap(), SECRET);
            assert!(
                matches!(
                    get_operation_result(&events),
                    Some(OperationResult::Success(_))
                ),
                "got: {events:?}"
            );
        }

        // ── Mid-command ──────────────────────────────────────────────────────

        // A command killed by Ctrl+C must not be blamed on the package file.
        //
        // `stop_on_error` defaults to **true**, and a cancelled command fails —
        // so the failure arm reaches `stop_on_error`'s explanation first and the
        // run reports "Stopped after failing to apply dotfile 'X' (stop_on_error
        // is enabled)". That names the user's own interrupt as a spec problem
        // and sends them looking for one. The between-entries guard cannot help
        // here: the break happens in the same iteration the command died in.
        #[tokio::test]
        async fn a_command_killed_by_cancellation_is_not_reported_as_a_spec_failure() {
            let dirs = TestDirs::new();
            let target = dirs.target_dir.join("credentials");
            provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

            let token = CancellationToken::new();
            let runner = TokenObservingRunner::cancelling_and_failing(&token);
            let service = dirs.service_with_runner_and_token(runner.clone(), token);

            let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

            // The command really was dispatched and really did die, so what
            // follows is about attribution and not about an entry that never ran.
            assert_eq!(runner.calls(), vec!["op read x".to_string()]);

            // The terminal event exists and names cancellation — this is also the
            // assertion that a cancelled run still completes its stream rather
            // than dropping it.
            assert_cancelled(&events);

            let rendered = format!("{events:?}");
            assert!(
                !rendered.contains("stop_on_error"),
                "cancellation must not be attributed to stop_on_error, got: {rendered}"
            );
        }

        // The fourth window: cancellation arriving once the last entry has
        // started, where the loop's guard can never run again.
        //
        // Distinct from the other three because nothing fails here -- the command
        // succeeds despite the cancellation, so no failure arm is reached and no
        // guard is left to run. Without the post-loop check the run reports
        // `Success` for a run the user interrupted, which for a provider entry
        // means a credential on disk with nothing mentioning Ctrl+C.
        //
        // The target is asserted present on purpose: cancelling does not unwrite.
        #[tokio::test]
        async fn a_cancellation_during_the_last_entry_is_still_reported() {
            let dirs = TestDirs::new();
            let target = dirs.target_dir.join("credentials");
            provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

            let token = CancellationToken::new();
            // Cancels while the command runs, but the command still succeeds —
            // so the entry deploys and the loop then ends with nothing left to
            // check.
            let runner = TokenObservingRunner::new(
                &token,
                vec![CallEffect {
                    cancel: true,
                    fail_as_cancelled: false,
                }],
            );
            let service = dirs.service_with_runner_and_token(runner.clone(), token);

            let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

            // Control: the entry really did run and really did deploy, so this is
            // the silent-success window and not an entry that never started.
            assert_eq!(runner.calls(), vec!["op read x".to_string()]);
            assert!(
                target.exists(),
                "the write happened before the cancellation was noticed"
            );

            assert_cancelled(&events);
        }

        // ── What a cancelled run leaves behind ───────────────────────────────

        // Cancelling must not discard the record of what was already deployed.
        //
        // The run breaks out of the loop rather than returning, so deploy state
        // still reaches disk. Returning early would leave files in place with no
        // record, and the next drift check would report them as untracked.
        //
        // Three entries, because the invariant needs one *after* the cancelling
        // entry. Not a dry run, because dry runs skip saving state entirely --
        // which would make the assertion pass for an unrelated reason.
        #[tokio::test]
        async fn a_cancelled_apply_still_records_what_it_already_deployed() {
            let dirs = TestDirs::new();
            let first = dirs.target_dir.join("first.conf");
            let secret_target = dirs.target_dir.join("credentials");
            let third = dirs.target_dir.join("third.conf");

            std::fs::create_dir_all(dirs.package_dir.join("creds")).unwrap();
            std::fs::write(dirs.package_dir.join("creds/first"), "first\n").unwrap();
            std::fs::write(dirs.package_dir.join("creds/third"), "third\n").unwrap();
            let yaml = format!(
                "name: creds\nenvironments:\n  test:\n    install: \"echo i\"\ndotfiles:\n  \
                 - source: \"creds/first\"\n    target: \"{}\"\n  \
                 - command: \"op read x\"\n    target: \"{}\"\n  \
                 - source: \"creds/third\"\n    target: \"{}\"\n",
                first.display(),
                secret_target.display(),
                third.display(),
            );
            std::fs::write(dirs.package_dir.join("creds.yml"), yaml).unwrap();

            let token = CancellationToken::new();
            // Cancels during the provider command but lets it succeed, so the
            // run reaches the third entry's guard rather than the failure arm.
            let runner = TokenObservingRunner::new(
                &token,
                vec![CallEffect {
                    cancel: true,
                    fail_as_cancelled: false,
                }],
            );
            let service = dirs.service_with_runner_and_token(runner, token);

            let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

            assert_cancelled(&events);
            assert!(
                first.exists(),
                "the entry before the cancellation must have deployed"
            );
            assert!(
                !third.exists(),
                "the entry after the cancellation must not deploy"
            );

            let state = std::fs::read_to_string(state_file(&dirs))
                .expect("deploy state must survive a cancelled run");
            assert!(
                state.contains("creds/first"),
                "what was deployed before the cancellation has to stay recorded, \
                 or the next drift check calls it untracked: {state}"
            );
            assert!(
                !state.contains("creds/third"),
                "nothing may be recorded for an entry that never deployed: {state}"
            );
        }
    }

    // What a noisy login shell puts in a credentials file (selfie-evf9).
    //
    // The unit tests in `commands::shell` assert what the runner returns. These
    // assert the thing that actually matters: the bytes on disk, through the
    // whole service, with a real shell process in the middle.
    //
    // The shell is a script standing in for the user's, so nothing here reads or
    // writes the developer's own configuration. See `commands::shell`'s
    // `content_tests` for why that is a faithful stand-in.
    mod noisy_shell {
        use super::*;
        use selfie::commands::ShellCommandRunner;
        use std::time::Duration;

        // A shell that writes before, during and after the command it is given.
        fn noisy_runner(dir: &std::path::Path) -> ShellCommandRunner {
            let path = dir.join("noisy-shell");
            test_common::write_executable(
                &path,
                "#!/bin/sh\n\
                 printf '%s' 'LEADBANNER'\n\
                 /bin/sh -c 'sleep 0.3; printf BACKGROUNDNOISE' &\n\
                 trap 'printf TRAILCHATTER' EXIT\n\
                 shift $(($# - 1)); eval \"$1\"\n",
            );

            ShellCommandRunner::new(path.to_str().unwrap(), Duration::from_secs(30))
        }

        // Where the command reads the credential from.
        //
        // The command must not *contain* the secret: a package file's command
        // string is quoted back in events and errors on purpose, so a fixture
        // that spells the credential into it fails the leak scan for a reason
        // that has nothing to do with what is under test. A real provider is
        // `op read …`, which names a credential without being one.
        fn secret_source(dir: &std::path::Path) -> PathBuf {
            let path = dir.join("vault-stand-in");
            std::fs::write(&path, SECRET).unwrap();
            path
        }

        #[tokio::test]
        async fn a_noisy_shell_puts_nothing_of_its_own_in_the_deployed_file() {
            let dirs = TestDirs::new();
            let target = dirs.target_dir.join("credentials");
            // Slow enough for the backgrounded writer to land mid-command. A
            // command that returns instantly wins the race by accident, and the
            // test would pass without separating anything.
            let source = secret_source(dirs.package_dir.as_path());
            provider_package(
                &dirs.package_dir,
                target.to_str().unwrap(),
                &format!("sleep 0.6; cat {}", source.display()),
            );
            let service = dirs.service_with_runner_and_token(
                noisy_runner(dirs.package_dir.as_path()),
                CancellationToken::new(),
            );

            let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

            // The positive control: the credential really was produced and
            // written, so the scans below are not passing over an empty file.
            let deployed = std::fs::read_to_string(&target).unwrap();
            assert_eq!(deployed, SECRET);
            for noise in ["LEADBANNER", "BACKGROUNDNOISE", "TRAILCHATTER"] {
                assert!(
                    !deployed.contains(noise),
                    "the shell's own {noise} reached the credentials file: {deployed:?}"
                );
            }
            assert!(
                events
                    .iter()
                    .any(|e| matches!(e, PackageEvent::DotfileDeployed { .. })),
                "the entry did not deploy: {events:?}"
            );
            assert_no_event_mentions(&events, SECRET);
        }

        #[tokio::test]
        async fn a_command_that_installs_an_exit_trap_deploys_and_warns() {
            // selfie cannot find the end of this command's output, so what the
            // command's own trap prints is appended to the credential. It still
            // deploys — the command works and the user asked for it — but the run
            // says so rather than reporting a clean success.
            let dirs = TestDirs::new();
            let target = dirs.target_dir.join("credentials");
            let source = secret_source(dirs.package_dir.as_path());
            provider_package(
                &dirs.package_dir,
                target.to_str().unwrap(),
                &format!("trap 'printf MYCLEANUP' EXIT; cat {}", source.display()),
            );
            let service = dirs.service_with_runner_and_token(
                noisy_runner(dirs.package_dir.as_path()),
                CancellationToken::new(),
            );

            let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

            assert_eq!(
                std::fs::read_to_string(&target).unwrap(),
                format!("{SECRET}MYCLEANUP"),
                "the appended bytes are what the warning is about"
            );
            let warned = events
                .iter()
                .any(|event| format!("{event:?}").contains("could not establish where the output"));
            assert!(warned, "appending foreign bytes silently: {events:?}");
            assert_no_event_mentions(&events, SECRET);
        }
    }
}

// Deploying a repository file onto a symlinked target, and the permissions of the
// deploy state file.
//
// Unix-only: symlink following and permission bits are not observable through
// `MockFileSystem`, whose writers are stubs with no filesystem behind them. The
// assertions that matter here — where the bytes actually landed, and what mode the
// state file carries — cannot be expressed against it. Everything runs inside a
// `TempDir`, so nothing outside it is touched.
mod symlinked_targets {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;

    fn repo_source(dirs: &TestDirs, relative: &str, content: &str) {
        let path = dirs.package_dir.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn is_symlink(path: &Path) -> bool {
        std::fs::symlink_metadata(path)
            .unwrap()
            .file_type()
            .is_symlink()
    }

    fn warnings(events: &[PackageEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                PackageEvent::Warning { message, .. } => Some(message.clone()),
                _ => None,
            })
            .collect()
    }

    // `(deployed, skipped, conflict, refused)` — every bucket, because the
    // point of each is that it is not one of the others. A refusal is not a
    // conflict, and since selfie-c28 it is not a skip either.
    fn deploy_counts(events: &[PackageEvent]) -> (usize, usize, usize, usize) {
        match get_operation_result(events).expect("no Completed event") {
            OperationResult::Success(OperationSuccess::DotfilesApplied {
                deployed_count,
                skipped_count,
                conflict_count,
                refused_count,
                ..
            }) => (
                *deployed_count,
                *skipped_count,
                *conflict_count,
                *refused_count,
            ),
            other => panic!("expected DotfilesApplied, got {other:?}"),
        }
    }

    // The refusal must not fire on a target apply had no reason to write to.
    //
    // Deploying by copying does not forbid a symlinked target; it declines to
    // *write through* one. Someone who keeps their dotfiles symlinked from
    // elsewhere and is already in sync should see nothing at all. Without this,
    // `a_symlinked_target_is_refused_on_a_routine_repo_update` would also pass
    // against an implementation that refused every symlinked target outright —
    // confirmed by mutation.
    #[tokio::test]
    async fn an_in_sync_symlinked_target_is_left_alone_and_not_reported() {
        let dirs = TestDirs::new();
        repo_source(&dirs, "myapp/config.toml", "SAME");
        let destination = dirs.target_dir.join("destination");
        std::fs::write(&destination, "SAME").unwrap();
        let target = dirs.target_dir.join("config.toml");
        std::os::unix::fs::symlink(&destination, &target).unwrap();
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );

        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        assert!(is_symlink(&target), "an untouched target must stay a link");
        assert_eq!(std::fs::read_to_string(&destination).unwrap(), "SAME");
        assert!(
            warnings(&events).is_empty(),
            "nothing was written, so nothing should be reported: {:?}",
            warnings(&events)
        );
    }

    // An untracked, already-matching symlinked target is not recorded as deployed.
    //
    // selfie never wrote it and never will, so an entry claiming it did is a
    // promise the refusal guarantees it can never keep: the entry can never
    // advance, and `detect_drift` would answer `None` for it forever (selfie-phnh).
    //
    // Two entries, identical but for the link, because the axis under test is the
    // symlink and nothing else — both are already in sync, so both take the same
    // `Skip` branch. The plain one is the control: a fix that simply stopped
    // recording would fail on it.
    #[tokio::test]
    async fn an_untracked_matching_symlinked_target_records_no_deployment() {
        let dirs = TestDirs::new();
        repo_source(&dirs, "myapp/plain.toml", "SAME");
        repo_source(&dirs, "myapp/linked.toml", "SAME");

        let plain = dirs.target_dir.join("plain.toml");
        std::fs::write(&plain, "SAME").unwrap();
        let destination = dirs.target_dir.join("destination");
        std::fs::write(&destination, "SAME").unwrap();
        let linked = dirs.target_dir.join("linked.toml");
        std::os::unix::fs::symlink(&destination, &linked).unwrap();

        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[
                ("myapp/plain.toml", plain.to_str().unwrap()),
                ("myapp/linked.toml", linked.to_str().unwrap()),
            ],
        );

        let _ = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        // Deserialized rather than matched as text: the load-bearing assertion is
        // the negative one, and a substring of one source key can occur inside
        // another.
        let written =
            std::fs::read_to_string(dirs.state_dir.join("deploy-state.yml")).expect("state file");
        let state: DeployState = selfie::yaml::parse(&written).expect("state file parses");

        assert!(
            state.get(plain.to_str().unwrap()).is_some(),
            "control: an ordinary already-in-sync target is still recorded: {written}"
        );
        assert!(
            state.get(linked.to_str().unwrap()).is_none(),
            "a target selfie never wrote to was recorded as deployed: {written}"
        );
    }

    // The routine path, and the reason a refusal beats replacing the link.
    //
    // `RepoChanged` routes to `DeployDecision::Deploy` with no conflict and no
    // `--yes`, so this is the ordinary apply after any edit to a repository file
    // — not a rare case. Replacing the link here would silently discard it and
    // orphan its destination on the first apply after any edit.
    #[tokio::test]
    async fn a_symlinked_target_is_refused_on_a_routine_repo_update() {
        let dirs = TestDirs::new();
        repo_source(&dirs, "myapp/config.toml", "V1");
        let target = dirs.target_dir.join("config.toml");
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );

        // Deploy to a plain file first, then migrate it to a link — the stow-style
        // layout a user adopts after using selfie. The first apply has to genuinely
        // *write*, because that is what records the state the second apply needs to
        // classify the entry `RepoChanged`. Starting from an already-symlinked
        // matching target would record nothing (selfie-phnh), and the second apply
        // would reach the refusal as a `Conflict` instead — passing every assertion
        // below while testing a different branch.
        let first = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;
        assert_eq!(
            deploy_counts(&first),
            (1, 0, 0, 0),
            "the fixture depends on this apply really writing: {first:?}"
        );
        let destination = dirs.target_dir.join("destination");
        std::fs::rename(&target, &destination).unwrap();
        std::os::unix::fs::symlink(&destination, &target).unwrap();
        repo_source(&dirs, "myapp/config.toml", "V2");

        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        assert_eq!(
            std::fs::read_to_string(&destination).unwrap(),
            "V1",
            "the link's destination must not be written through"
        );
        assert!(is_symlink(&target), "the link itself must be left in place");
        let warnings = warnings(&events);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("is a symlink")
                    && w.contains(&destination.display().to_string())),
            "the refusal must name where the link points: {warnings:?}"
        );
        assert_eq!(
            deploy_counts(&events),
            (0, 0, 0, 1),
            "a refusal is counted as refused, not as a deploy"
        );
        // Which branch the refusal was reached from is invisible in the counts —
        // `Deploy` and `Conflict` both land in the same bucket behind it. Drift
        // reads the same state through the same classifier, so it is where the
        // fixture's `RepoChanged` becomes observable rather than assumed.
        let drift = collect_events(dirs.service().check_drift().await).await;
        assert_eq!(
            drift
                .iter()
                .filter_map(|e| match e {
                    PackageEvent::DotfileDriftDetected { drift_type, .. } =>
                        Some(drift_type.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec!["repo changed"],
            "the entry must reach the refusal as a repository update, not as a conflict"
        );
    }

    // A dangling link is refused too, and its destination is not created.
    //
    // This is the case a caller cannot detect by inspecting the target
    // afterwards: `path_exists` follows the link and reports false, so apply
    // decides to deploy with no conflict, and `fs::write` would then create the
    // file at whatever path the link names.
    #[tokio::test]
    async fn a_dangling_symlinked_target_does_not_create_its_destination() {
        let dirs = TestDirs::new();
        repo_source(&dirs, "myapp/config.toml", "REPO");
        let never_created = dirs.target_dir.join("never-created");
        let target = dirs.target_dir.join("config.toml");
        std::os::unix::fs::symlink(&never_created, &target).unwrap();
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );

        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        assert!(
            !never_created.exists(),
            "the link's destination was created"
        );
        assert!(is_symlink(&target));
        assert_eq!(deploy_counts(&events), (0, 0, 0, 1));
    }

    // `--yes` resolves a conflict; it does not authorize writing through a link.
    //
    // The two are independent decisions, and the flag speaks only to the first.
    #[tokio::test]
    async fn auto_accept_does_not_override_the_refusal() {
        let dirs = TestDirs::new();
        repo_source(&dirs, "myapp/config.toml", "REPO");
        let destination = dirs.target_dir.join("destination");
        std::fs::write(&destination, "USER EDITED").unwrap();
        let target = dirs.target_dir.join("config.toml");
        std::os::unix::fs::symlink(&destination, &target).unwrap();
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );

        let options = ApplyOptions {
            auto_accept: true,
            ..Default::default()
        };
        let events = collect_events(dirs.service().apply_all(options).await).await;

        assert_eq!(
            std::fs::read_to_string(&destination).unwrap(),
            "USER EDITED"
        );
        assert!(is_symlink(&target));
        assert_eq!(deploy_counts(&events), (0, 0, 0, 1));
    }

    // A preview must report the refusal, not promise a deploy that will not happen.
    //
    // The same ordering rule the secret path already follows: checks that a real
    // apply would refuse on come before the dry-run short-circuit, so a preview
    // describes the run you are about to perform rather than a different one.
    #[tokio::test]
    async fn a_dry_run_reports_the_refusal_rather_than_previewing_a_deploy() {
        let dirs = TestDirs::new();
        repo_source(&dirs, "myapp/config.toml", "V1");
        let target = dirs.target_dir.join("config.toml");
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );

        // Same setup as the routine-update test, and for the same reason: the entry
        // has to reach the *deploy* decision rather than be reported as a conflict,
        // which means deploying to a plain file first and linking it aside
        // afterwards. Starting from an already-symlinked matching target records
        // nothing (selfie-phnh), leaving the second apply a `Conflict` — which
        // satisfies every assertion below while testing a different branch.
        let first = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;
        assert_eq!(
            deploy_counts(&first),
            (1, 0, 0, 0),
            "the fixture depends on this apply really writing: {first:?}"
        );
        let destination = dirs.target_dir.join("destination");
        std::fs::rename(&target, &destination).unwrap();
        std::os::unix::fs::symlink(&destination, &target).unwrap();
        repo_source(&dirs, "myapp/config.toml", "V2");

        let options = ApplyOptions {
            dry_run: true,
            ..Default::default()
        };
        let events = collect_events(dirs.service().apply_all(options).await).await;

        let warnings = warnings(&events);
        assert!(
            warnings.iter().any(|w| w.contains("is a symlink")),
            "a dry run must say the entry would be refused: {warnings:?}"
        );
        assert!(
            !events.iter().any(
                |e| matches!(e, PackageEvent::DotfileSkipped { reason, .. } if reason == "dry run")
            ),
            "the entry must not also be previewed as a deploy: {events:?}"
        );
        assert_eq!(std::fs::read_to_string(&destination).unwrap(), "V1");
        // Neither assertion above can see *which* branch reached the refusal: the
        // check at the top of the loop precedes `match decision`, so `Deploy` and
        // `Conflict` both warn and both skip, and a resolver-less conflict emits a
        // conflict event rather than a "dry run" skip either way. Reverting the
        // fixture to an already-symlinked target therefore left this test green
        // while it tested the conflict path. Drift reads the same state through the
        // same classifier, so it is what pins the fixture.
        let drift = collect_events(dirs.service().check_drift().await).await;
        assert_eq!(
            drift
                .iter()
                .filter_map(|e| match e {
                    PackageEvent::DotfileDriftDetected { drift_type, .. } =>
                        Some(drift_type.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec!["repo changed"],
            "the dry run must preview a deploy decision, not a conflict"
        );
    }

    // The user is never asked a question whose answer cannot be honored.
    //
    // The refusal for a symlinked target is settled before the resolver is
    // consulted, so a prompt that could not be honored either way never happens.
    #[tokio::test]
    async fn a_conflicting_symlinked_target_is_refused_without_prompting() {
        use selfie::dotfile_service::port::{ConflictDetail, ConflictResolution, ConflictResolver};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        struct RecordsBeingAsked(Arc<AtomicBool>);
        impl ConflictResolver for RecordsBeingAsked {
            fn resolve(&self, _target: &str, _detail: ConflictDetail<'_>) -> ConflictResolution {
                self.0.store(true, Ordering::SeqCst);
                ConflictResolution::Accept
            }
        }

        let dirs = TestDirs::new();
        repo_source(&dirs, "myapp/config.toml", "REPO");
        let destination = dirs.target_dir.join("destination");
        std::fs::write(&destination, "USER EDITED").unwrap();
        let target = dirs.target_dir.join("config.toml");
        std::os::unix::fs::symlink(&destination, &target).unwrap();
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );

        let asked = Arc::new(AtomicBool::new(false));
        let options = ApplyOptions {
            conflict_resolver: Some(Arc::new(RecordsBeingAsked(Arc::clone(&asked)))),
            ..Default::default()
        };
        let events = collect_events(dirs.service().apply_all(options).await).await;

        assert!(
            !asked.load(Ordering::SeqCst),
            "the resolver was asked to settle a conflict that would be refused anyway"
        );
        assert_eq!(
            std::fs::read_to_string(&destination).unwrap(),
            "USER EDITED"
        );
        assert!(is_symlink(&target));
        // Counted as refused, NOT as a conflict, though the content differs and the
        // entry would otherwise have been one. A conflict is a question for the
        // user; this one is already settled, so it is not asked. Asserted so the
        // bucket cannot move back without someone deciding to.
        //
        // Not `skipped` either, since selfie-c28: an entry selfie declined to write
        // is not one there was nothing to do for.
        assert_eq!(
            deploy_counts(&events),
            (0, 0, 0, 1),
            "a refused entry is counted as refused: not a conflict, and not a skip"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, PackageEvent::DotfileConflict { .. })),
            "no conflict should be reported for a refused entry: {events:?}"
        );
    }

    // The writer refuses on its own, with the hoisted check taken out of the way.
    //
    // This is the TOCTOU defense, and the half of the fix nothing else observes:
    // with the `handle_apply` check in place, reverting `perform_deploy` to a
    // following write fails no other test in the workspace. Without this test a
    // reader can find the writer redundant, delete it, and see a green suite.
    //
    // Blinding `symlink_refusal` reproduces the race deterministically.
    // `auto_accept` is load-bearing: without it a differing target takes the
    // conflict branch and never reaches `perform_deploy`.
    #[tokio::test]
    async fn the_writer_refuses_even_when_the_check_is_blinded() {
        use selfie::config::SelfieConfigBuilder;
        use selfie::dotfile_service::service::DotfileServiceImpl;
        use selfie::fs::{FileSystem, FileSystemError, RealFileSystem, TargetPath};
        use selfie::package::repository::YamlPackageRepository;
        use std::path::PathBuf;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        // `RealFileSystem` with the symlink *report* suppressed and nothing else
        // changed — an attacker who wins the race between the check and the write.
        //
        // Counts writes so the test can prove the writer was actually reached.
        // Without that, a refactor that detects the link by some route other than
        // `symlink_refusal` would leave this decorator blinding nothing, the test
        // would pass having exercised none of what it is named for, and the
        // writer's own symlink check would become deletable again — the exact
        // hole this guards.
        #[derive(Clone, Debug)]
        struct BlindToSymlinks(RealFileSystem, Arc<AtomicUsize>);

        impl FileSystem for BlindToSymlinks {
            fn is_directory(&self, path: &TargetPath) -> Result<bool, FileSystemError> {
                self.0.is_directory(path)
            }

            // Delegated: this decorator blinds the symlink check only.
            fn directory_state(&self, path: &std::path::Path) -> selfie::fs::DirectoryState {
                self.0.directory_state(path)
            }

            fn symlink_refusal(&self, _path: &TargetPath) -> Option<FileSystemError> {
                None
            }

            // Deliberately **not** blinded: this decorator blinds one check, the
            // symlink one, so that the writer's own symlink check is the only
            // thing left to refuse. Blinding the irregular check too would widen what
            // this test claims to cover and hide a real regression in it.
            fn irregular_target_refusal(&self, path: &TargetPath) -> Option<FileSystemError> {
                self.0.irregular_target_refusal(path)
            }

            fn read_file(&self, path: &Path) -> Result<String, FileSystemError> {
                self.0.read_file(path)
            }
            fn read_file_bytes(&self, path: &Path) -> Result<Vec<u8>, FileSystemError> {
                self.0.read_file_bytes(path)
            }
            fn write_file_private(
                &self,
                path: &TargetPath,
                data: &[u8],
            ) -> Result<(), FileSystemError> {
                self.0.write_file_private(path, data)
            }
            fn write_file_no_follow(
                &self,
                path: &TargetPath,
                data: &[u8],
            ) -> Result<(), FileSystemError> {
                self.1.fetch_add(1, Ordering::SeqCst);
                self.0.write_file_no_follow(path, data)
            }
            fn is_owner_only(&self, path: &TargetPath) -> Result<bool, FileSystemError> {
                self.0.is_owner_only(path)
            }
            fn remove_file(&self, path: &Path) -> Result<(), FileSystemError> {
                self.0.remove_file(path)
            }
            fn path_exists(&self, path: &Path) -> bool {
                self.0.path_exists(path)
            }
            fn expand_path(&self, path: &Path) -> Result<PathBuf, FileSystemError> {
                self.0.expand_path(path)
            }
            fn list_directory(&self, path: &Path) -> Result<Vec<PathBuf>, FileSystemError> {
                self.0.list_directory(path)
            }
            fn canonicalize(&self, path: &Path) -> Result<PathBuf, FileSystemError> {
                self.0.canonicalize(path)
            }
            fn config_dir(&self) -> Result<PathBuf, FileSystemError> {
                self.0.config_dir()
            }
        }

        let dirs = TestDirs::new();
        repo_source(&dirs, "myapp/config.toml", "ATTACKER_PAYLOAD");
        let destination = dirs.target_dir.join("destination");
        std::fs::write(&destination, "ORIGINAL").unwrap();
        let target = dirs.target_dir.join("config.toml");
        std::os::unix::fs::symlink(&destination, &target).unwrap();
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );

        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory(&dirs.package_dir)
            .dotfiles_directory(dirs.dotfiles_dir.clone())
            .state_directory(dirs.state_dir.clone())
            .build();
        let repo = YamlPackageRepository::new(
            RealFileSystem,
            config.package_directory().clone(),
            SpecOrigin::PackageDirectory,
        );
        let writes = Arc::new(AtomicUsize::new(0));
        let service = DotfileServiceImpl::new(
            repo,
            BlindToSymlinks(RealFileSystem, Arc::clone(&writes)),
            FakeCommandRunner::new(),
            config,
            CancellationToken::new(),
            SudoPolicy::new(RunningAs(Elevation::Unprivileged)),
        );

        let options = ApplyOptions {
            // Without this the entry is a conflict and never reaches the write.
            auto_accept: true,
            ..Default::default()
        };
        let events = collect_events(service.apply_all(options).await).await;

        // The control. Everything below is about what the writer did, so the test
        // has to establish that the writer ran at all — otherwise a green result
        // could mean the refusal came from somewhere else entirely.
        assert!(
            writes.load(Ordering::SeqCst) > 0,
            "the write site was never reached, so this test proved nothing about it"
        );
        assert_eq!(
            std::fs::read_to_string(&destination).unwrap(),
            "ORIGINAL",
            "the writer let the content through to the link's destination"
        );
        assert!(is_symlink(&target));
        let warnings = warnings(&events);
        assert!(
            warnings.iter().any(|w| w.contains("is a symlink")),
            "the writer's own refusal must still be reported: {warnings:?}"
        );
        assert_eq!(deploy_counts(&events), (0, 0, 0, 1));
    }

    // An ordinary target is unaffected by any of the above.
    #[tokio::test]
    async fn an_ordinary_target_still_deploys() {
        let dirs = TestDirs::new();
        repo_source(&dirs, "myapp/config.toml", "REPO");
        let target = dirs.target_dir.join("config.toml");
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );

        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "REPO");
        assert_eq!(deploy_counts(&events), (1, 0, 0, 0));
    }

    // The deploy state file names every path selfie manages here, so it must not
    // be readable by anyone but its owner.
    #[tokio::test]
    async fn the_deploy_state_file_is_owner_only() {
        let dirs = TestDirs::new();
        repo_source(&dirs, "myapp/config.toml", "REPO");
        let target = dirs.target_dir.join("config.toml");
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );

        let _ = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        // The control says what an ordinary write produces in this environment.
        // Under a umask of 077 an owner-only assertion passes whatever the code
        // does, so without this the test could be green while proving nothing.
        let control = dirs.state_dir.join("control");
        std::fs::write(&control, b"x").unwrap();
        let control_mode = std::fs::metadata(&control).unwrap().permissions().mode();
        if control_mode & 0o077 == 0 {
            let message = "the ambient umask makes ordinary writes owner-only, so this \
                           cannot tell an owner-only write from a default one";
            assert!(
                std::env::var_os("CI").is_none(),
                "the_deploy_state_file_is_owner_only: {message}"
            );
            eprintln!("SKIP the_deploy_state_file_is_owner_only: {message}");
            return;
        }

        let state_file = dirs.state_dir.join("deploy-state.yml");
        let mode = std::fs::metadata(&state_file).unwrap().permissions().mode();
        // `& 0o077`, not `& 0o007`: group-readable exposes the map to exactly the
        // people it is being kept from on a shared machine.
        assert_eq!(
            mode & 0o077,
            0,
            "group/other bits set on the deploy state file: {:04o}",
            mode & 0o777
        );
    }

    // A state file left world-readable by an earlier version is corrected.
    #[tokio::test]
    async fn an_existing_world_readable_state_file_is_tightened() {
        let dirs = TestDirs::new();
        repo_source(&dirs, "myapp/config.toml", "REPO");
        let target = dirs.target_dir.join("config.toml");
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );
        let state_file = dirs.state_dir.join("deploy-state.yml");
        std::fs::write(&state_file, "deployed: {}\n").unwrap();
        std::fs::set_permissions(&state_file, std::fs::Permissions::from_mode(0o644)).unwrap();

        let _ = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        // An implementation that opened the existing file and truncated it would
        // leave the old mode in place, since a creation mode applies only when the
        // file is created.
        let mode = std::fs::metadata(&state_file).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o077,
            0,
            "a pre-existing state file kept its permissive mode: {:04o}",
            mode & 0o777
        );
    }
}

// Consequences of expanding a target without canonicalizing it.
//
// These are behavior changes rather than fixes, asserted so they are deliberate
// and visible rather than discovered later.
mod target_expansion {
    use selfie::fs::{RealFileSystem, expand_target_path};
    use tempfile::TempDir;

    // The final component is never resolved, which is what lets the writers see a
    // symlink at all. See `expand_target_path`'s documentation.
    #[test]
    fn a_symlinked_target_keeps_its_own_path() {
        let temp = TempDir::new().unwrap();
        let destination = temp.path().join("destination");
        std::fs::write(&destination, "x").unwrap();
        let target = temp.path().join("link");
        std::os::unix::fs::symlink(&destination, &target).unwrap();

        let expanded = expand_target_path(&RealFileSystem, target.to_str().unwrap());

        assert_eq!(
            expanded.path(),
            target,
            "the target was resolved to its destination"
        );
    }

    // Two spellings of one file that differ through a symlinked directory no
    // longer compare equal.
    //
    // Duplicate detection in `dotfiles track` compares expanded paths, so it
    // misses this case. Recorded because it is what tempts someone into putting
    // `expand_path` back, which would reopen selfie-4m9. Fix it by comparing
    // differently, not by resolving here.
    #[test]
    fn paths_differing_through_a_symlinked_directory_no_longer_match() {
        let temp = TempDir::new().unwrap();
        let real = temp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let linked = temp.path().join("linked");
        std::os::unix::fs::symlink(&real, &linked).unwrap();
        std::fs::write(real.join("config"), "x").unwrap();

        let via_link = expand_target_path(&RealFileSystem, linked.join("config").to_str().unwrap());
        let via_real = expand_target_path(&RealFileSystem, real.join("config").to_str().unwrap());

        assert_ne!(via_link, via_real);
    }
}

// What `dotfiles drift` and `dotfiles track` say about a symlinked target, and
// what the lexical containment guard does not say about a symlinked source.
//
// `apply` refuses to write through a symlinked target (selfie-4m9). These cover
// the other two commands and the guard's documented limit. Unix-only:
// `MockFileSystem` has no filesystem behind it, so none of this is observable
// through it. Everything runs inside a `TempDir`.
mod symlink_consistency {
    use super::*;
    use std::path::Path;

    fn repo_source(dirs: &TestDirs, relative: &str, content: &str) {
        let path = dirs.package_dir.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn warnings(events: &[PackageEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                PackageEvent::Warning { message, .. } => Some(message.clone()),
                _ => None,
            })
            .collect()
    }

    fn drift_types(events: &[PackageEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                PackageEvent::DotfileDriftDetected { drift_type, .. } => Some(drift_type.clone()),
                _ => None,
            })
            .collect()
    }

    fn symlink_warnings(events: &[PackageEvent]) -> Vec<String> {
        warnings(events)
            .into_iter()
            .filter(|w| w.contains("is a symlink"))
            .collect()
    }

    // A package whose one entry targets `target`, with `content` in the repository.
    fn package_targeting(dirs: &TestDirs, content: &str, target: &Path) {
        repo_source(dirs, "myapp/config.toml", content);
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );
    }

    // Deploy normally, then migrate the target to a stow-style link: move the
    // deployed file aside and symlink to it. Leaves the recorded deploy state
    // matching the link's destination exactly, which is the state a user reaches by
    // adopting a symlink layout after using selfie.
    async fn deploy_then_link_aside(dirs: &TestDirs, target: &Path) -> PathBuf {
        collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;
        let destination = dirs.target_dir.join("destination");
        std::fs::rename(target, &destination).unwrap();
        std::os::unix::fs::symlink(&destination, target).unwrap();
        destination
    }

    // ── drift ───────────────────────────────────────────────────────────────

    // D1. The case in selfie-qvwq's title: a repository edit that can never reach
    // the target, reported as `repo changed` on every run forever because the
    // deploy state can never advance. Drift now names the symlink alongside it.
    #[tokio::test]
    async fn drift_names_the_symlink_when_it_reports_drift() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        package_targeting(&dirs, "V1", &target);
        let destination = deploy_then_link_aside(&dirs, &target).await;
        repo_source(&dirs, "myapp/config.toml", "V2");

        let events = collect_events(dirs.service().check_drift().await).await;

        assert_eq!(drift_types(&events), vec!["repo changed"]);
        let named = symlink_warnings(&events);
        assert_eq!(named.len(), 1, "expected one refusal, got {named:?}");
        assert!(
            named[0].contains(&*target.to_string_lossy())
                && named[0].contains(&*destination.to_string_lossy()),
            "the refusal must name both the target and where the link points: {named:?}"
        );
    }

    // D2. Parity in the silent direction: a symlinked target already in sync is one
    // apply has no reason to write to, and apply says nothing about it
    // (`an_in_sync_symlinked_target_is_left_alone_and_not_reported`). Drift must not
    // invent a complaint apply does not make.
    //
    // Without this, D1 would also pass against an implementation that warned about
    // every symlinked target it saw.
    #[tokio::test]
    async fn drift_says_nothing_about_an_in_sync_symlinked_target() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        package_targeting(&dirs, "V1", &target);
        deploy_then_link_aside(&dirs, &target).await;

        let events = collect_events(dirs.service().check_drift().await).await;

        assert_eq!(drift_types(&events), Vec::<String>::new());
        assert_eq!(
            warnings(&events),
            Vec::<String>::new(),
            "nothing is out of sync, so there is nothing to report"
        );
    }

    // D4. What the state file does not claim, seen from the command that reads it.
    //
    // The fresh-machine sequence in selfie-phnh: a config already symlinked into
    // place by another tool, matching the repository file, and `apply` run once.
    // The entry stays `not tracked` because nothing was recorded. Settling to
    // `none` would report the target as in sync on a machine selfie has never
    // deployed to and cannot deploy to.
    //
    // Still no refusal *reason*, which is the parity D5 pins: `apply` is silent
    // about this entry, so drift is too.
    #[tokio::test]
    async fn drift_no_longer_calls_a_never_deployed_symlinked_target_in_sync() {
        let dirs = TestDirs::new();
        let destination = dirs.target_dir.join("destination");
        std::fs::write(&destination, "SAME BYTES").unwrap();
        let target = dirs.target_dir.join("config.toml");
        std::os::unix::fs::symlink(&destination, &target).unwrap();
        package_targeting(&dirs, "SAME BYTES", &target);

        collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;
        let events = collect_events(dirs.service().check_drift().await).await;

        assert_eq!(
            drift_types(&events),
            vec!["not tracked"],
            "apply recorded a deployment for a target it never wrote to"
        );
        assert_eq!(
            symlink_warnings(&events),
            Vec::<String>::new(),
            "control: drift stays as silent as apply about this entry"
        );
    }

    // D3 (row 1a). The fresh-machine case: a target already symlinked into place by
    // another tool, never deployed by selfie, whose destination differs from the
    // repository file. Drift classifies it `not tracked` rather than `repo changed`,
    // so a fix that only handled `RepoChanged` would leave this silent — which is
    // why the fixture varies along the drift-type axis.
    #[tokio::test]
    async fn drift_names_the_symlink_on_a_never_deployed_target() {
        let dirs = TestDirs::new();
        let destination = dirs.target_dir.join("destination");
        std::fs::write(&destination, "SOMETHING ELSE").unwrap();
        let target = dirs.target_dir.join("config.toml");
        std::os::unix::fs::symlink(&destination, &target).unwrap();
        package_targeting(&dirs, "V1", &target);

        let events = collect_events(dirs.service().check_drift().await).await;

        assert_eq!(drift_types(&events), vec!["not tracked"]);
        assert_eq!(
            symlink_warnings(&events).len(),
            1,
            "a never-deployed symlinked target is refused too: {:?}",
            warnings(&events)
        );
    }

    // D5 (row 1b). The fixture that separates `deploy_decision` from `drift != None`.
    //
    // Never deployed, so drift is `NotTracked` and the entry is reported as drifted
    // — but the destination's contents already match the repository file, so
    // `deploy_decision` returns `Skip` and **apply is silent**. Gating the refusal on
    // the drift type instead of on apply's own decision would warn here, recreating
    // the drift-vs-apply disagreement in the opposite direction.
    //
    // The apply half is asserted in the same test on purpose: the property is that
    // the two commands agree, and a test that only looked at drift could not see it.
    #[tokio::test]
    async fn drift_is_silent_where_apply_is_silent_on_an_untracked_matching_link() {
        let dirs = TestDirs::new();
        let destination = dirs.target_dir.join("destination");
        std::fs::write(&destination, "SAME BYTES").unwrap();
        let target = dirs.target_dir.join("config.toml");
        std::os::unix::fs::symlink(&destination, &target).unwrap();
        package_targeting(&dirs, "SAME BYTES", &target);

        let drift = collect_events(dirs.service().check_drift().await).await;
        let apply = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        assert_eq!(
            symlink_warnings(&apply),
            Vec::<String>::new(),
            "control: apply must be silent here, or this fixture proves nothing"
        );
        assert_eq!(
            symlink_warnings(&drift),
            Vec::<String>::new(),
            "drift warned where apply did not"
        );
        assert_eq!(
            drift_types(&drift),
            vec!["not tracked"],
            "control: the entry is still reported as drifted, so a `drift != None` \
             gate really would have fired here"
        );
    }

    // D4. Drift and apply describe the same refusal with the same sentence, because
    // they call the same `refusal_warning`. A user who runs one then the other must
    // not have to work out whether two different messages mean the same thing.
    #[tokio::test]
    async fn drift_and_apply_word_the_refusal_identically() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        package_targeting(&dirs, "V1", &target);
        deploy_then_link_aside(&dirs, &target).await;
        repo_source(&dirs, "myapp/config.toml", "V2");

        let drift = collect_events(dirs.service().check_drift().await).await;
        let apply = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        let from_drift = symlink_warnings(&drift);
        let from_apply = symlink_warnings(&apply);
        assert_eq!(from_drift.len(), 1, "drift said nothing: {from_drift:?}");
        assert_eq!(from_apply.len(), 1, "apply said nothing: {from_apply:?}");
        assert_eq!(from_drift[0], from_apply[0]);
    }

    // ── track ───────────────────────────────────────────────────────────────

    // T1. Tracking a symlinked target is refused rather than recorded.
    //
    // There is no configuration in which tracking one does what the user asked:
    // apply refuses to write through it, so the entry is either permanently inert or
    // permanently broken. The refusal names the destination, because deciding what
    // to do about it needs to know where the link goes.
    #[tokio::test]
    async fn tracking_a_symlinked_target_is_refused() {
        let dirs = TestDirs::new();
        let destination = dirs.target_dir.join("real_config");
        std::fs::write(&destination, "content").unwrap();
        let target = dirs.target_dir.join("config.toml");
        std::os::unix::fs::symlink(&destination, &target).unwrap();

        let events = collect_events(
            dirs.service_with_dotfiles()
                .track_standalone("myapp", target.to_str().unwrap())
                .await,
        )
        .await;

        let message = failure_message(&events);
        assert!(
            message.contains("is a symlink") && message.contains(&*destination.to_string_lossy()),
            "the refusal must say symlink and name the destination: {message}"
        );
    }

    // T2. The refusal lands before every write, which is the part that matters.
    //
    // Tracking reads *through* a link, so a refusal placed after any of the three
    // writes would already have copied the destination's contents into the dotfiles
    // directory — a file the user never named, and one `selfie sync push` would
    // commit — written a spec, and recorded a deploy state entry for a deployment
    // that never happened. T1 cannot see any of that; it only sees the verdict.
    #[tokio::test]
    async fn a_refused_track_writes_nothing() {
        let dirs = TestDirs::new();
        let destination = dirs.target_dir.join("real_config");
        std::fs::write(&destination, "NEVER COPIED ANYWHERE").unwrap();
        let target = dirs.target_dir.join("config.toml");
        std::os::unix::fs::symlink(&destination, &target).unwrap();

        let events = collect_events(
            dirs.service_with_dotfiles()
                .track_standalone("myapp", target.to_str().unwrap())
                .await,
        )
        .await;

        assert!(matches!(
            get_operation_result(&events).expect("no Completed event"),
            OperationResult::Failure(_)
        ));
        assert!(
            !dirs.dotfiles_dir.join("myapp/config.toml").exists(),
            "the link's destination was copied into the dotfiles directory"
        );
        assert!(
            !dirs.dotfiles_dir.join("myapp.yml").exists(),
            "a spec was written for an entry that cannot deploy"
        );
        assert!(
            !dirs.state_dir.join("deploy-state.yml").exists(),
            "deploy state was recorded for a deployment that never happened"
        );
        assert_eq!(
            std::fs::read_to_string(&destination).unwrap(),
            "NEVER COPIED ANYWHERE",
            "control: the destination itself must be untouched"
        );
    }

    // T3. A dangling link is refused as a symlink, not reported as a missing file.
    //
    // `path_exists` follows the link, so the existence check answers "no" for a path
    // the user can see in their own shell. This is the only fixture on which the
    // refusal's position relative to that check is observable.
    #[tokio::test]
    async fn tracking_a_dangling_symlink_says_symlink_not_missing() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        std::os::unix::fs::symlink(dirs.target_dir.join("never_created"), &target).unwrap();

        let events = collect_events(
            dirs.service_with_dotfiles()
                .track_standalone("myapp", target.to_str().unwrap())
                .await,
        )
        .await;

        let message = failure_message(&events);
        assert!(
            message.contains("is a symlink"),
            "a dangling link is still a link: {message}"
        );
        assert!(
            !message.contains("does not exist"),
            "the path the user typed does exist; calling it missing sends them \
             looking for the wrong problem: {message}"
        );
    }

    // T4. The other track handler refuses too.
    //
    // `track_for_package` is a separate function with its own copy of the read, the
    // write and the state record, so a fix applied to one handler leaves the other
    // exfiltrating the destination exactly as before.
    #[tokio::test]
    async fn tracking_a_symlinked_target_for_a_package_is_refused() {
        let dirs = TestDirs::new();
        std::fs::write(
            dirs.package_dir.join("myapp.yml"),
            "name: myapp\nenvironments:\n  test:\n    install: \"echo installed\"\n",
        )
        .unwrap();
        let destination = dirs.target_dir.join("real_config");
        std::fs::write(&destination, "NEVER COPIED ANYWHERE").unwrap();
        let target = dirs.target_dir.join("config.toml");
        std::os::unix::fs::symlink(&destination, &target).unwrap();

        let events = collect_events(
            dirs.service()
                .track_for_package("myapp", target.to_str().unwrap())
                .await,
        )
        .await;

        let message = failure_message(&events);
        assert!(
            message.contains("is a symlink"),
            "track_for_package accepted a symlinked target: {message}"
        );
        assert!(
            !dirs.package_dir.join("myapp/config.toml").exists(),
            "the link's destination was copied into the package directory"
        );
    }

    // ── the lexical containment guard (selfie-86o) ──────────────────────────

    // The containment guard is lexical, so a symlink inside the package directory
    // escapes it. Recorded as an executable fact rather than only as prose.
    //
    // **This test asserts a limitation and is meant to fail if the limitation is
    // removed.** Anyone making the guard symlink-aware should delete it together
    // with the `crate::paths::is_within` paragraph it pins.
    //
    // Both forms are covered because a symlinked **directory** puts the escape
    // off the final component: `symlink_metadata` on the full source path reports
    // a regular file, so a final-component guard would let it through.
    #[tokio::test]
    async fn a_symlinked_source_escapes_the_containment_guard() {
        // A symlinked file inside the package directory.
        {
            let dirs = TestDirs::new();
            let outside = dirs.state_dir.join("outside.txt");
            std::fs::write(&outside, "OUTSIDE THE PACKAGE DIRECTORY").unwrap();
            std::fs::create_dir_all(dirs.package_dir.join("myapp")).unwrap();
            std::os::unix::fs::symlink(&outside, dirs.package_dir.join("myapp/config.toml"))
                .unwrap();
            let target = dirs.target_dir.join("config.toml");
            create_package_with_dotfiles(
                &dirs.package_dir,
                "myapp",
                &[("myapp/config.toml", target.to_str().unwrap())],
            );

            let events =
                collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

            assert!(
                warnings(&events).is_empty(),
                "the guard is lexical; if it started refusing this, update \
                 `is_within`'s documentation too: {:?}",
                warnings(&events)
            );
            assert_eq!(
                std::fs::read_to_string(&target).unwrap(),
                "OUTSIDE THE PACKAGE DIRECTORY"
            );
        }

        // A symlinked directory inside the package directory.
        {
            let dirs = TestDirs::new();
            let outside = dirs.state_dir.join("outside_dir");
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(
                outside.join("config.toml"),
                "OUTSIDE VIA A LINKED DIRECTORY",
            )
            .unwrap();
            std::os::unix::fs::symlink(&outside, dirs.package_dir.join("myapp")).unwrap();
            let target = dirs.target_dir.join("config.toml");
            create_package_with_dotfiles(
                &dirs.package_dir,
                "myapp",
                &[("myapp/config.toml", target.to_str().unwrap())],
            );

            assert!(
                !std::fs::symlink_metadata(dirs.package_dir.join("myapp/config.toml"))
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "the source path's final component is a regular file, which is why \
                 checking only the final component would not catch this"
            );

            let events =
                collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

            assert!(warnings(&events).is_empty(), "{:?}", warnings(&events));
            assert_eq!(
                std::fs::read_to_string(&target).unwrap(),
                "OUTSIDE VIA A LINKED DIRECTORY"
            );
        }
    }
}

// The one target rule, as each command applies it.
//
// Four beads: `spec validate` accepting what apply refuses (selfie-jlum), track
// accepting what apply refuses (selfie-q9t3), apply's refusal describing a rule
// the input satisfies (selfie-hkhb), and two entry-level refusals returning
// different outcomes (selfie-m5dv). `deploy_target` is the reconciled rule.
//
// Unix-only because everything runs against a real filesystem in a `TempDir`.
// Nothing creates a CWD-relative fixture -- a relative target is refused before
// anything stats it, which is the property under test.
mod target_rule {
    use super::*;

    // A package with one entry whose source exists, so an error can only come
    // from the target.
    fn package_targeting(dirs: &TestDirs, target: &str) {
        let source = dirs.package_dir.join("myapp/config.toml");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(source, "content").unwrap();
        create_package_with_dotfiles(&dirs.package_dir, "myapp", &[("myapp/config.toml", target)]);
    }

    // selfie-q9t3: track had no absoluteness guard, so a relative target resolved
    // against the process working directory, was recorded, and was refused by
    // every later apply.
    //
    // Deleting the guard makes this fail with "Target file does not exist", so
    // the negative assertion discriminates rather than passing vacuously.
    //
    // It does not prove the guard's position relative to `symlink_refusal`, which
    // returns `None` for any path that does not exist. The guard sits ahead on
    // argument, not because a test holds it there.
    #[tokio::test]
    async fn tracking_a_relative_target_is_refused() {
        let dirs = TestDirs::new();

        let events = collect_events(
            dirs.service_with_dotfiles()
                .track_standalone("myapp", "relative/config.toml")
                .await,
        )
        .await;

        let message = failure_message(&events);
        assert!(
            message.contains("is not absolute"),
            "the refusal must name the rule: {message}"
        );
        assert!(
            !message.contains("does not exist"),
            "reporting a missing file sends the user looking for the wrong problem: {message}"
        );
    }

    // The refusal lands before every write, as the symlink refusal does.
    //
    // **This test did not fail under any mutation run against it** -- moving the
    // guard below the writes, deleting it, and moving it below `symlink_refusal`
    // all left it green. The fixture is why: a relative target does not exist, so
    // `path_exists` returns early and nothing is written either way.
    //
    // Kept as documentation rather than enforcement: it names the spec, source
    // copy and deploy-state record a refusal must not leave behind. Do not read
    // it as proof that it does not.
    #[tokio::test]
    async fn a_refused_relative_track_writes_nothing() {
        let dirs = TestDirs::new();

        let events = collect_events(
            dirs.service_with_dotfiles()
                .track_standalone("myapp", "relative/config.toml")
                .await,
        )
        .await;

        assert!(matches!(
            get_operation_result(&events).expect("no Completed event"),
            OperationResult::Failure(_)
        ));
        assert!(
            !dirs.dotfiles_dir.join("myapp/config.toml").exists(),
            "a source file was copied for an entry that cannot deploy"
        );
        assert!(
            !dirs.dotfiles_dir.join("myapp.yml").exists(),
            "a spec was written for an entry that cannot deploy"
        );
        assert!(
            !dirs.state_dir.join("deploy-state.yml").exists(),
            "deploy state was recorded for a deployment that never happened"
        );
    }

    // The diagnostic for `~user/…` must not restate the absoluteness rule, for a
    // path that visibly starts with `~`.
    #[tokio::test]
    async fn tracking_a_named_user_target_is_refused() {
        let dirs = TestDirs::new();

        let events = collect_events(
            dirs.service_with_dotfiles()
                .track_standalone("myapp", "~alice/config.toml")
                .await,
        )
        .await;

        let message = failure_message(&events);
        assert!(
            message.contains("~user"),
            "the refusal must name the unsupported form: {message}"
        );
        assert!(
            !message.contains("does not exist"),
            "the path the user typed is not the problem: {message}"
        );
    }

    // The other track handler is a separate copy of the read, the write and the
    // state record, so a fix applied to one leaves the other recording entries
    // that can never deploy.
    #[tokio::test]
    async fn tracking_a_relative_target_for_a_package_is_refused() {
        let dirs = TestDirs::new();
        std::fs::write(
            dirs.package_dir.join("myapp.yml"),
            "name: myapp\nenvironments:\n  test:\n    install: \"echo installed\"\n",
        )
        .unwrap();

        let events = collect_events(
            dirs.service()
                .track_for_package("myapp", "relative/config.toml")
                .await,
        )
        .await;

        let message = failure_message(&events);
        assert!(
            message.contains("is not absolute"),
            "track_for_package accepted a relative target: {message}"
        );
    }

    // The target guard sits ahead of the already-tracked short-circuit, so an
    // entry the rule refuses is not reported as tracked.
    //
    // Without this the short-circuit answers first and the command succeeds,
    // telling the user selfie is managing a target no apply will ever deploy.
    #[tokio::test]
    async fn tracking_an_already_recorded_bad_target_is_still_refused() {
        let dirs = TestDirs::new();
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", "~alice/config.toml")],
        );

        let events = collect_events(
            dirs.service()
                .track_for_package("myapp", "~alice/config.toml")
                .await,
        )
        .await;

        let message = failure_message(&events);
        assert!(
            message.contains("~user"),
            "an already-recorded entry that cannot deploy must not be reported as tracked: \
             {message}"
        );
    }

    // selfie-m5dv: a relative target skipped while an escaping template aborted,
    // though neither ran a command, and the documentation described the
    // opposite.
    //
    // Two entries, the refused one first, because the counters cannot tell these
    // two apart on their own: `Skipped` and `Failed` differ in *which* bucket
    // they increment, but this test is about `stop_on_error`, and what
    // distinguishes an aborted run from a continued one is whether the second
    // entry is reached at all. A one-entry package has no second entry, so it
    // would pass whether the run stopped or carried on.
    #[tokio::test]
    async fn a_refused_target_stops_the_run_like_an_escaping_template_does() {
        async fn run(stop_on_error: bool) -> (Vec<PackageEvent>, PathBuf, TestDirs) {
            let dirs = TestDirs::new();
            let second = dirs.target_dir.join("second-credentials");
            let yaml = format!(
                "name: creds\nenvironments:\n  test:\n    install: \"echo i\"\ndotfiles:\n  \
                 - command: \"op read first\"\n    target: \"relative/credentials\"\n  \
                 - command: \"op read second\"\n    target: \"{}\"\n",
                second.display()
            );
            std::fs::write(dirs.package_dir.join("creds.yml"), yaml).unwrap();

            let runner = FakeCommandRunner::new()
                .succeeding("op read first", b"FIRST")
                .succeeding("op read second", b"SECOND");
            let service = dirs.service_with_runner_and_stop_on_error(runner, stop_on_error);
            let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;
            (events, second, dirs)
        }

        let (events, second, _dirs) = run(true).await;
        assert!(
            matches!(
                get_operation_result(&events).expect("no Completed event"),
                OperationResult::Failure(_)
            ),
            "a refused entry must end the run under stop_on_error, got: {:?}",
            get_operation_result(&events)
        );
        assert!(
            !second.exists(),
            "the run continued past a refusal it should have stopped on"
        );

        // The control. Without a second, deployable entry this asserts nothing:
        // "the run did not stop" reads identically whether the first entry was
        // Skipped or Failed.
        let (events, second, _dirs) = run(false).await;
        assert!(
            second.exists(),
            "stop_on_error: false must still reach the second entry, got: {events:?}"
        );
    }

    // Drift refuses in apply's words, so one spec defect does not read as two
    // problems depending on which command found it.
    #[tokio::test]
    async fn drift_refuses_a_target_in_applies_words() {
        let dirs = TestDirs::new();
        package_targeting(&dirs, "~alice/config.toml");

        let applied = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;
        let drifted = collect_events(dirs.service().check_drift().await).await;

        let refusal = |events: &[PackageEvent]| {
            warning_messages(events)
                .into_iter()
                .find(|w| w.contains("~user"))
                .unwrap_or_else(|| panic!("no target refusal in {events:?}"))
        };

        assert_eq!(refusal(&applied), refusal(&drifted));
    }
}

// What every command does with a deploy-state file it cannot use.
//
// `load_deploy_state`'s own branches are covered at the unit layer in
// `dotfile_service::state_file`. What these add is the wiring: each caller
// decides separately whether to refuse or to warn, so one observing test per
// call site, each asserting what the command left on disk as well as what it
// said.
mod deploy_state_diagnostics {
    use super::*;

    const CORRUPT: &[u8] = b"{{{{not valid yaml!!! garbage $$$";

    fn warnings(events: &[PackageEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                PackageEvent::Warning { message, .. } => Some(message.clone()),
                _ => None,
            })
            .collect()
    }

    // Everything a run said about the state file: the warnings, and the
    // failure a refusing command ends with.
    fn state_reports(events: &[PackageEvent]) -> Vec<String> {
        let failure = match get_operation_result(events) {
            Some(OperationResult::Failure(failure)) => Some(failure.to_string()),
            _ => None,
        };
        warnings(events)
            .into_iter()
            .chain(failure)
            .filter(|report| report.contains("deploy-state.yml"))
            .collect()
    }

    fn state_file(dirs: &TestDirs) -> PathBuf {
        dirs.state_dir.join("deploy-state.yml")
    }

    fn write_state_file(dirs: &TestDirs, contents: &[u8]) {
        std::fs::write(state_file(dirs), contents).unwrap();
    }

    fn corrupt_the_state_file(dirs: &TestDirs) {
        write_state_file(dirs, CORRUPT);
    }

    fn a_package_with_one_dotfile(dirs: &TestDirs) {
        let source_dir = dirs.package_dir.join("myapp");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[(
                "myapp/config.toml",
                dirs.target_dir.join("config.toml").to_str().unwrap(),
            )],
        );
    }

    // A command that writes ended as a failure that names the parse problem and
    // the file, and the file is still what it was.
    #[track_caller]
    fn assert_refused_over_the_corrupt_file(dirs: &TestDirs, events: &[PackageEvent]) {
        let message = failure_message(events);
        assert!(
            message.contains("Cannot parse") && message.contains("deploy-state.yml"),
            "the refusal must say the file could not be parsed, and name it: {message}"
        );
        assert_eq!(
            std::fs::read(state_file(dirs)).unwrap(),
            CORRUPT,
            "the unusable state file was written over"
        );
    }

    // Call site 1 of 4: `handle_apply`, which refuses.
    #[tokio::test]
    async fn apply_refuses_a_corrupt_state_file() {
        let dirs = TestDirs::new();
        a_package_with_one_dotfile(&dirs);
        corrupt_the_state_file(&dirs);

        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        assert_refused_over_the_corrupt_file(&dirs, &events);
        assert!(
            !dirs.target_dir.join("config.toml").exists(),
            "a dotfile was deployed by a run that could not record it"
        );
    }

    // Call site 2 of 4: `handle_check_drift`, which warns and carries on.
    //
    // The one command that only reads. It must still say what it ignored, and
    // it must leave the file exactly as it found it.
    #[tokio::test]
    async fn drift_warns_over_a_corrupt_state_file_and_writes_nothing() {
        let dirs = TestDirs::new();
        a_package_with_one_dotfile(&dirs);
        corrupt_the_state_file(&dirs);

        let events = collect_events(dirs.service().check_drift().await).await;

        let named = state_reports(&events);
        assert_eq!(
            named.len(),
            1,
            "expected one report naming the state file, got {:?}",
            warnings(&events)
        );
        assert!(
            named[0].contains("Cannot parse")
                && named[0].contains("continuing as though nothing had been deployed"),
            "drift must say the file could not be parsed and that it carried on: {named:?}"
        );
        assert!(
            matches!(
                get_operation_result(&events),
                Some(OperationResult::Success(_))
            ),
            "drift must not refuse: {events:?}"
        );
        assert_eq!(
            std::fs::read(state_file(&dirs)).unwrap(),
            CORRUPT,
            "drift wrote the state file"
        );
    }

    // Call site 3 of 4: `handle_track_standalone`, which refuses before it
    // copies anything. The three negatives are the copy, the spec, and the
    // state file itself.
    #[tokio::test]
    async fn track_standalone_refuses_a_corrupt_state_file_before_copying() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("starship.toml");
        std::fs::write(&target, "format = \"$all\"").unwrap();
        corrupt_the_state_file(&dirs);

        let events = collect_events(
            dirs.service_with_dotfiles()
                .track_standalone("starship", target.to_str().unwrap())
                .await,
        )
        .await;

        assert_refused_over_the_corrupt_file(&dirs, &events);
        assert!(
            !dirs.dotfiles_dir.join("starship/starship.toml").exists(),
            "the target was copied into the dotfiles directory by a run that could not record it"
        );
        assert!(
            !dirs.dotfiles_dir.join("starship.yml").exists(),
            "a spec was written by a run that could not record it"
        );
    }

    // Call site 4 of 4: `handle_track_for_package`, the same three negatives
    // against the package directory and the package's own spec.
    #[tokio::test]
    async fn track_for_package_refuses_a_corrupt_state_file_before_copying() {
        let dirs = TestDirs::new();
        let spec = dirs.package_dir.join("alacritty.yml");
        let spec_text =
            "name: alacritty\nenvironments:\n  test:\n    install: \"echo installed\"\n";
        std::fs::write(&spec, spec_text).unwrap();
        let target = dirs.target_dir.join("alacritty.toml");
        std::fs::write(&target, "[font]\nsize = 12").unwrap();
        corrupt_the_state_file(&dirs);

        let events = collect_events(
            dirs.service()
                .track_for_package("alacritty", target.to_str().unwrap())
                .await,
        )
        .await;

        assert_refused_over_the_corrupt_file(&dirs, &events);
        assert!(
            !dirs.package_dir.join("alacritty/alacritty.toml").exists(),
            "the target was copied beside the spec by a run that could not record it"
        );
        assert_eq!(
            std::fs::read_to_string(&spec).unwrap(),
            spec_text,
            "the spec was rewritten by a run that could not record it"
        );
    }

    // The report must carry none of the file by the time it reaches an event
    // or a result.
    //
    // **`Debug` is not a superset of what egresses, so do not read the sweep below
    // as a boundary guarantee.** This crate ships deliberately redacting `Debug`
    // impls, so state-file text arriving behind one would render redacted and the
    // sweep would pass. The real exits are the typed `message` of a warning and
    // the rendered failure, which is what `state_reports` collects.

    // The assertion that matches the egress is the one on the reports
    // themselves, and the fixture is a duplicated key -- the one class whose
    // text quotes the file.
    #[tokio::test]
    async fn no_event_carries_the_state_file_contents() {
        const MARKER: &str = "zzz-recon-marker/id_rsa.conf";

        let dirs = TestDirs::new();
        a_package_with_one_dotfile(&dirs);
        write_state_file(
            &dirs,
            format!(
                "deployed:\n  {MARKER}:\n    source: s\n    checksum: a\n    \
                 deployed_at: b\n  {MARKER}:\n    source: s\n    \
                 checksum: c\n    deployed_at: d\n"
            )
            .as_bytes(),
        );

        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        // Controls: the file really was parsed, the refusal really names it --
        // so scanning the failure below is not vacuous -- and it was reported as
        // a *duplicate key*, since the comment above is only true while that is
        // the class this fixture produces.
        let reports = state_reports(&events);
        assert_eq!(reports.len(), 1, "expected one report: {reports:?}");
        assert!(
            reports[0].contains("Cannot parse") && reports[0].contains("a key is listed twice"),
            "this fixture no longer produces the one class whose text quotes the \
             file, so it would pass against any implementation: {reports:?}"
        );
        assert!(
            failure_message(&events).contains("deploy-state.yml"),
            "the refusal must name the file, or scanning it proves nothing"
        );

        // The ones that match the egress: every warning and the failure, as the
        // fields the adapters read.
        for report in reports.iter().chain(warnings(&events).iter()) {
            assert!(
                !report.contains(MARKER),
                "a report carried the state file's contents: {report}"
            );
        }
        assert!(
            !failure_message(&events).contains(MARKER),
            "the failure carried the state file's contents"
        );

        // The cheap net over everything else, with the limits above understood.
        for event in &events {
            let rendered = format!("{event:?}");
            assert!(
                !rendered.contains(MARKER),
                "an event carried the state file's contents: {rendered}"
            );
        }
    }

    // The silence that must survive: a first run has no state file and says so
    // about nothing. A warning here would fire on every fresh machine.
    #[tokio::test]
    async fn a_first_run_reports_nothing_about_the_state_file() {
        let dirs = TestDirs::new();
        a_package_with_one_dotfile(&dirs);

        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        assert_eq!(
            state_reports(&events),
            Vec::<String>::new(),
            "a first run must not report an absent state file"
        );
    }

    // A configured state directory that is not there is created by the run that
    // needs it, and the deploy is recorded in it. It is selfie's own directory
    // whether the user named the path or took the default, which is what ADR-0005
    // decision 8.2 settles.
    //
    // Refusing here cost a working apply: a user who set `state_directory` had
    // every dotfile command refuse until they created the directory by hand, and
    // the refusal named a setting rather than saying what to do with it.
    #[tokio::test]
    async fn a_configured_state_directory_that_is_not_there_is_created_by_the_run() {
        let dirs = TestDirs::new();
        a_package_with_one_dotfile(&dirs);
        std::fs::remove_dir(&dirs.state_dir).unwrap();

        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        assert_eq!(refused_count(&events), 0, "events: {events:?}");
        assert!(
            dirs.target_dir.join("config.toml").exists(),
            "the dotfile must be deployed"
        );
        assert!(
            state_file(&dirs).exists(),
            "the deploy must be recorded, which is what the directory is for"
        );

        // The second run reads what the first wrote. A directory created but not
        // recorded into would leave drift reporting a first run forever.
        let events = collect_events(dirs.service().check_drift().await).await;
        let warnings = warning_messages(&events);
        assert!(
            !warnings.iter().any(|w| w.contains("state_directory")),
            "nothing is wrong with the state directory now: {warnings:?}"
        );
    }

    // What still refuses, and why the change above is a narrowing rather than a
    // removal: creating the directory is the remedy for nothing being there and no
    // remedy at all for a file in the way. Refused before the deploy, so the run
    // does not write dotfiles it cannot record.
    #[tokio::test]
    async fn a_file_where_the_state_directory_belongs_refuses_apply() {
        let dirs = TestDirs::new();
        a_package_with_one_dotfile(&dirs);
        std::fs::remove_dir(&dirs.state_dir).unwrap();
        std::fs::write(&dirs.state_dir, "not a directory").unwrap();

        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        let message = failure_message(&events);
        assert!(
            message.contains(dirs.state_dir.to_str().unwrap())
                && message.contains("is a regular file"),
            "the refusal must name the path and what is there: {message}"
        );
        assert!(
            !dirs.target_dir.join("config.toml").exists(),
            "a dotfile was deployed by a run that could not record it"
        );
    }

    // A fifo at the state path is refused rather than opened. Opening a fifo to
    // read blocks until a writer arrives, so without the guard every dotfile
    // command hangs before doing any work; the deadline turns that hang into a
    // failure. Drift is exercised because it only reads, so the refusal cannot
    // be the save's.
    //
    // The run lives on a detached thread with its own runtime, and the test
    // waits on a channel with a deadline. `tokio::time::timeout` cannot do this:
    // the open blocks a runtime thread, and dropping the runtime joins that
    // thread, so a missing guard would hang the suite rather than fail the test.
    #[test]
    fn a_fifo_at_the_state_path_does_not_hang_drift() {
        let dirs = TestDirs::new();
        a_package_with_one_dotfile(&dirs);
        let status = std::process::Command::new("mkfifo")
            .arg(state_file(&dirs))
            .status()
            .expect("mkfifo runs");
        assert!(status.success(), "mkfifo failed to create the fixture");

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a runtime for the drift check");
            let events = runtime
                .block_on(async { collect_events(dirs.service().check_drift().await).await });
            // Nothing listens once the deadline has passed; a failed send is
            // not this thread's problem.
            let _ = tx.send(events);
        });
        let events = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("drift hung on the fifo at the state path");

        let named = state_reports(&events);
        assert_eq!(
            named.len(),
            1,
            "expected one report: {:?}",
            warnings(&events)
        );
        assert!(
            named[0].contains("named pipe (fifo)"),
            "the report must say what sits at the path: {named:?}"
        );
    }

    // A file that exists and holds nothing is not a first run. Reading it as one
    // would make a state lost to an interrupted write look like a fresh machine,
    // and the next apply would re-prompt for every dotfile with no explanation.
    #[tokio::test]
    async fn an_empty_state_file_is_refused_not_read_as_a_first_run() {
        let dirs = TestDirs::new();
        a_package_with_one_dotfile(&dirs);
        write_state_file(&dirs, b"");

        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        let message = failure_message(&events);
        assert!(
            message.contains("is empty") && message.contains("deploy-state.yml"),
            "the refusal must say the file is empty and name it: {message}"
        );
        assert!(
            !dirs.target_dir.join("config.toml").exists(),
            "a dotfile was deployed by a run that could not record it"
        );
        assert_eq!(
            std::fs::read(state_file(&dirs)).unwrap(),
            b"",
            "the empty state file was written over"
        );
    }

    // A state file that cannot be *read* is refused with a different sentence
    // from one that cannot be *parsed*, through the result and not just in
    // isolation.
    //
    // The unreadable file is a directory at the state file's path: `path_exists`
    // answers true and the read then fails, without depending on the runner's
    // privileges the way a `chmod 000` file would. The directory is a regular
    // file's absence rather than an irregular file, so the wording is the read's.
    #[tokio::test]
    async fn apply_refuses_an_unreadable_state_file() {
        let dirs = TestDirs::new();
        a_package_with_one_dotfile(&dirs);
        std::fs::create_dir(state_file(&dirs)).unwrap();

        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        let message = failure_message(&events);
        assert!(
            message.contains("Cannot read deploy state") && message.contains("deploy-state.yml"),
            "the refusal must say the file could not be read, and name it: {message}"
        );
        assert!(
            state_file(&dirs).is_dir(),
            "the directory at the state path was replaced"
        );
        assert!(
            !dirs.target_dir.join("config.toml").exists(),
            "a dotfile was deployed by a run that could not record it"
        );
    }
}

// What `selfie apply` reports when it refuses.
//
// A refusal used to land in `skipped_count` beside "already in sync", so a
// caller — a script reading the exit code, or an assistant reading the MCP
// envelope — could not tell a run that deployed nothing from a run that had
// nothing to deploy (selfie-c28). These pin the buckets apart.
mod refusal_accounting {
    use super::*;

    // `(deployed, skipped, conflict, refused)`.
    fn counts(events: &[PackageEvent]) -> (usize, usize, usize, usize) {
        match get_operation_result(events).expect("no Completed event") {
            OperationResult::Success(OperationSuccess::DotfilesApplied {
                deployed_count,
                skipped_count,
                conflict_count,
                refused_count,
                ..
            }) => (
                *deployed_count,
                *skipped_count,
                *conflict_count,
                *refused_count,
            ),
            other => panic!("expected DotfilesApplied, got {other:?}"),
        }
    }

    fn steps(events: &[PackageEvent]) -> (usize, usize) {
        match get_operation_result(events).expect("no Completed event") {
            OperationResult::Success(OperationSuccess::DotfilesApplied {
                steps_completed, ..
            }) => (steps_completed.completed, steps_completed.total),
            other => panic!("expected DotfilesApplied, got {other:?}"),
        }
    }

    // The two buckets are told apart, on a fixture that varies along that axis
    // and nothing else.
    //
    // Both entries are repository files with an existing target; the only
    // difference is that one carries an unrecognized key and is therefore
    // refused. A fixture with only the refused entry would pass against an
    // implementation that renamed `skipped_count` to `refused_count` wholesale,
    // which is the change this test exists to reject.
    #[tokio::test]
    async fn a_refused_entry_is_counted_apart_from_an_in_sync_one() {
        let dirs = TestDirs::new();
        std::fs::create_dir_all(dirs.package_dir.join("myapp")).unwrap();
        std::fs::write(dirs.package_dir.join("myapp/insync.toml"), "SAME").unwrap();

        let in_sync = dirs.target_dir.join("insync.toml");
        std::fs::write(&in_sync, "SAME").unwrap();
        let refused = dirs.target_dir.join("refused.toml");

        write_package_yaml(
            &dirs.package_dir,
            "myapp",
            &format!(
                r#"name: myapp
environments:
  test:
    install: "echo installed"
dotfiles:
  - source: "myapp/insync.toml"
    target: "{}"
  - source: "myapp/typo.toml"
    target: "{}"
    var: oops
"#,
                in_sync.display(),
                refused.display()
            ),
        );

        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        assert_eq!(counts(&events), (0, 1, 0, 1));
        assert!(
            !refused.exists(),
            "the refused entry must not have been deployed"
        );
    }

    // A conflict the user accepted, which selfie then could not write.
    //
    // This is `perform_deploy`'s *second* failure site — the one inside the
    // conflict branch, which looks identical to the first and was missed when
    // this fix was planned as "six sites". A readable, owner read-only target
    // reaches it: the target differs, so the entry is a `Conflict`; `auto_accept`
    // settles it; and the in-place write then fails with `EACCES`. An unreadable
    // target would not do, because it is refused before the decision.
    #[tokio::test]
    async fn a_write_that_fails_after_an_accepted_conflict_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;

        let dirs = TestDirs::new();
        std::fs::create_dir_all(dirs.package_dir.join("myapp")).unwrap();
        std::fs::write(dirs.package_dir.join("myapp/config.toml"), "REPO").unwrap();

        // Owner read-only, in an owner read-only directory: an in-place open
        // for writing fails on the file, and a write-then-rename fails on the
        // directory, so the accepted write fails under either writer.
        let target = dirs.target_dir.join("config.toml");
        std::fs::write(&target, "TARGET").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o444)).unwrap();
        std::fs::set_permissions(&dirs.target_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let _restore = RestoreMode(dirs.target_dir.clone(), 0o755);
        // Root writes through the mode bits, so the write this test needs to
        // fail would succeed.
        if std::fs::OpenOptions::new()
            .write(true)
            .open(&target)
            .is_ok()
            || std::fs::File::create(dirs.target_dir.join("probe")).is_ok()
        {
            eprintln!(
                "SKIP a_write_that_fails_after_an_accepted_conflict_is_refused: running as root"
            );
            return;
        }

        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );

        let options = ApplyOptions {
            auto_accept: true,
            ..Default::default()
        };
        let events = collect_events(dirs.service().apply_all(options).await).await;

        assert_eq!(
            counts(&events),
            (0, 0, 0, 1),
            "an accepted conflict that could not be written is a refusal, not a skip"
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "TARGET",
            "the target must be left alone"
        );
    }

    // Every outcome is still a step.
    //
    // Moving refusals out of `skipped_count` shrinks the step total unless
    // `refused_count` is added back into it, and nothing else observes that
    // arithmetic. Without this, a run refusing two of three entries would
    // report `(1/1)`.
    #[tokio::test]
    async fn refused_entries_still_count_toward_the_step_total() {
        let dirs = TestDirs::new();
        std::fs::create_dir_all(dirs.package_dir.join("myapp")).unwrap();
        std::fs::write(dirs.package_dir.join("myapp/deployed.toml"), "NEW").unwrap();
        std::fs::write(dirs.package_dir.join("myapp/insync.toml"), "SAME").unwrap();

        let deployed = dirs.target_dir.join("deployed.toml");
        let in_sync = dirs.target_dir.join("insync.toml");
        std::fs::write(&in_sync, "SAME").unwrap();
        let refused = dirs.target_dir.join("refused.toml");

        write_package_yaml(
            &dirs.package_dir,
            "myapp",
            &format!(
                r#"name: myapp
environments:
  test:
    install: "echo installed"
dotfiles:
  - source: "myapp/deployed.toml"
    target: "{}"
  - source: "myapp/insync.toml"
    target: "{}"
  - source: "myapp/typo.toml"
    target: "{}"
    var: oops
"#,
                deployed.display(),
                in_sync.display(),
                refused.display()
            ),
        );

        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        assert_eq!(counts(&events), (1, 1, 0, 1));
        assert_eq!(
            steps(&events),
            (3, 3),
            "three entries were processed, so three steps happened"
        );
    }

    // A run with nothing to refuse says so, and a run with a refusal says so.
    //
    // The control half matters: `had_refusals` returning `true` unconditionally
    // would satisfy every other test here.
    #[tokio::test]
    async fn had_refusals_answers_both_ways() {
        let dirs = TestDirs::new();
        std::fs::create_dir_all(dirs.package_dir.join("myapp")).unwrap();
        std::fs::write(dirs.package_dir.join("myapp/config.toml"), "REPO").unwrap();
        let target = dirs.target_dir.join("config.toml");
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );

        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;
        let clean = match get_operation_result(&events).unwrap() {
            OperationResult::Success(s) => s.had_refusals(),
            other => panic!("expected success, got {other:?}"),
        };
        assert!(!clean, "a clean deploy reports no refusals");

        // Same package, now with an entry that cannot be deployed.
        write_package_yaml(
            &dirs.package_dir,
            "myapp",
            &format!(
                r#"name: myapp
environments:
  test:
    install: "echo installed"
dotfiles:
  - source: "myapp/typo.toml"
    target: "{}"
    var: oops
"#,
                dirs.target_dir.join("other.toml").display()
            ),
        );
        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;
        let refused = match get_operation_result(&events).unwrap() {
            OperationResult::Success(s) => s.had_refusals(),
            other => panic!("expected success, got {other:?}"),
        };
        assert!(refused, "a refusal is reported by the same predicate");
    }

    // `--dry-run` exits non-zero for a refusal it only previewed.
    //
    // A deliberate contract decision rather than a side effect: a preview whose
    // job is to say what `apply` would do must not report success for a run
    // that would refuse. The refusal fires on the dry-run path because it is
    // decided from the entry alone, before anything is written.
    #[tokio::test]
    async fn a_dry_run_reports_a_refusal_it_only_previewed() {
        let dirs = TestDirs::new();
        write_package_yaml(
            &dirs.package_dir,
            "myapp",
            &format!(
                r#"name: myapp
environments:
  test:
    install: "echo installed"
dotfiles:
  - source: "myapp/typo.toml"
    target: "{}"
    var: oops
"#,
                dirs.target_dir.join("config.toml").display()
            ),
        );

        let options = ApplyOptions {
            dry_run: true,
            ..Default::default()
        };
        let events = collect_events(dirs.service().apply_all(options).await).await;

        assert_eq!(counts(&events), (0, 0, 0, 1));
        match get_operation_result(&events).unwrap() {
            OperationResult::Success(s) => assert!(s.had_refusals()),
            other => panic!("expected success, got {other:?}"),
        }
    }
}

// A package file's top-level keys, and what `selfie apply` does about the ones a
// package does not accept.
//
// `_dotfiles:` as an anchor leaves the package no dotfiles, and a plain
// `configs:` does the same. Apply never runs validation, so it has to refuse
// them itself (selfie-g199, selfie-jt6m).
//
// Checking rereads the file, and one that parses can still fail that read. Apply
// refuses that file too, whatever it appears to have left to deploy: the key it
// may hide is what decides whether those entries are the right ones (selfie-5j5j).
mod top_level_key_refusals {
    use super::*;

    // Parses as a package, and not at all into the map the key check reads: a
    // mapping keyed by a sequence has no `serde_json::Value` to be read into.
    // The `dotfiles:` entry is what makes the refusal cost something visible:
    // an entry that would otherwise have deployed.
    fn package_that_cannot_be_re_read(dirs: &TestDirs, target: &std::path::Path) {
        std::fs::create_dir_all(dirs.package_dir.join("myapp")).unwrap();
        std::fs::write(dirs.package_dir.join("myapp/config.toml"), "REPO").unwrap();

        write_package_yaml(
            &dirs.package_dir,
            "myapp",
            &format!(
                r#"name: myapp
extra:
  ? [a, b]
  : v
dotfiles:
  - source: "myapp/config.toml"
    target: "{}"
environments:
  test:
    install: "echo installed"
"#,
                target.display()
            ),
        );
    }

    fn warning_messages(events: &[PackageEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                PackageEvent::Warning { message, .. } => Some(message.clone()),
                _ => None,
            })
            .collect()
    }

    // selfie-ty9n, the same hazard one level down.
    //
    // `_dotfiles:` inside the environment being applied leaves that environment's
    // list empty, so `dotfiles_for_environment` falls back to the shared entry and
    // deploys the file this machine was meant to override -- reporting success.
    //
    // The shared target is asserted **absent**: this is what separates a refusal
    // from the old behavior, where the wrong file was written and the run exited
    // zero. Asserting only the warning would pass against a version that warned
    // and deployed anyway.
    #[tokio::test]
    async fn apply_refuses_an_environment_carrying_a_shadowing_key() {
        let dirs = TestDirs::new();
        std::fs::create_dir_all(dirs.package_dir.join("myapp")).unwrap();
        std::fs::write(dirs.package_dir.join("myapp/shared.toml"), "SHARED").unwrap();
        let target = dirs.target_dir.join("config.toml");

        write_package_yaml(
            &dirs.package_dir,
            "myapp",
            &format!(
                r#"name: myapp
dotfiles:
  - source: "myapp/shared.toml"
    target: "{}"
environments:
  test:
    install: "echo installed"
    _dotfiles:
      - source: "myapp/work.toml"
        target: "{}"
"#,
                target.display(),
                target.display()
            ),
        );

        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        assert_eq!(refused_count(&events), 1);
        assert!(
            !target.exists(),
            "the shared entry was deployed over the override"
        );

        let warnings = warning_messages(&events);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("myapp") && w.contains("test") && w.contains("_dotfiles")),
            "the refusal must name the package, the environment and the key: {warnings:?}"
        );
    }

    // An environment this run does not apply cannot affect what it deploys, so a
    // typo there is not a reason to refuse.
    #[tokio::test]
    async fn apply_ignores_a_shadowing_key_in_another_environment() {
        let dirs = TestDirs::new();
        std::fs::create_dir_all(dirs.package_dir.join("myapp")).unwrap();
        std::fs::write(dirs.package_dir.join("myapp/shared.toml"), "SHARED").unwrap();
        let target = dirs.target_dir.join("config.toml");

        write_package_yaml(
            &dirs.package_dir,
            "myapp",
            &format!(
                r#"name: myapp
dotfiles:
  - source: "myapp/shared.toml"
    target: "{}"
environments:
  test:
    install: "echo installed"
  other:
    install: "echo other"
    _dotfiles: []
"#,
                target.display()
            ),
        );

        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        assert_eq!(refused_count(&events), 0);
        assert!(target.exists(), "the shared entry should still deploy");
    }

    // Apply refuses the package and says why.
    //
    // The target must not exist afterwards: the entry under `_dotfiles:` is not
    // deployed, which is the pre-existing behavior — what changes is that
    // selfie now says so instead of reporting success.
    #[tokio::test]
    async fn apply_refuses_a_package_whose_dotfiles_key_is_shadowed() {
        let dirs = TestDirs::new();
        std::fs::create_dir_all(dirs.package_dir.join("myapp")).unwrap();
        std::fs::write(dirs.package_dir.join("myapp/config.toml"), "REPO").unwrap();
        let target = dirs.target_dir.join("config.toml");

        write_package_yaml(
            &dirs.package_dir,
            "myapp",
            &format!(
                r#"name: myapp
environments:
  test:
    install: "echo installed"
_dotfiles:
  - source: "myapp/config.toml"
    target: "{}"
"#,
                target.display()
            ),
        );

        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        assert_eq!(refused_count(&events), 1);
        assert!(!target.exists(), "nothing should have been deployed");

        let warnings = warning_messages(&events);
        assert!(
            warnings.iter().any(|w| w.contains("myapp")
                && w.contains("_dotfiles")
                && w.contains("cannot be told apart from a misspelling")),
            "the refusal must name the package and the key: {warnings:?}"
        );
    }

    // The documented top-level anchor still deploys.
    //
    // The control, and the reason the refusal is scoped to keys that shadow a
    // *package* field: `docs/package-files.md` documents `_target: &target …`,
    // and a check that refused every `_`-prefixed top-level key would pass the
    // test above and break every file using the documented pattern.
    #[tokio::test]
    async fn apply_deploys_a_package_using_the_documented_target_anchor() {
        let dirs = TestDirs::new();
        std::fs::create_dir_all(dirs.package_dir.join("myapp")).unwrap();
        std::fs::write(dirs.package_dir.join("myapp/config.toml"), "REPO").unwrap();
        let target = dirs.target_dir.join("config.toml");

        write_package_yaml(
            &dirs.package_dir,
            "myapp",
            &format!(
                r#"_brew: &brew "echo installed"
_target: &target "{}"
name: myapp
environments:
  test:
    install: *brew
dotfiles:
  - source: "myapp/config.toml"
    target: *target
"#,
                target.display()
            ),
        );

        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        assert_eq!(refused_count(&events), 0, "{:?}", warning_messages(&events));
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "REPO",
            "the anchored target must still be deployed to"
        );
    }

    // Having entries to deploy does not make an unread top level safe. The key
    // it may hide is what decides whether those entries are the right ones: a
    // shadowed `environments:` costs the mapping, and the shared entry then
    // lands on the target an override was written for.
    #[tokio::test]
    async fn apply_refuses_a_file_it_cannot_re_read_even_with_dotfiles() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        package_that_cannot_be_re_read(&dirs, &target);

        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        assert_eq!(
            refused_count(&events),
            1,
            "a top level nothing looked at is a refusal: {:?}",
            warning_messages(&events)
        );
        assert!(
            !target.exists(),
            "a refused package must deploy nothing, but '{}' was written",
            target.display()
        );

        let warnings = warning_messages(&events);
        assert!(
            warnings.iter().any(
                |w| w.contains("Skipping package 'myapp'") && w.contains("cannot be ruled out")
            ),
            "the refusal must name the package and what could not be ruled out: {warnings:?}"
        );
    }

    // The harm the refusal exists for, which no count can show: the file carries
    // a shared entry, an override hidden behind `_environments:`, and a decoy
    // `environments:` that keeps every check keyed on that mapping quiet. Deployed,
    // the shared content lands on the override's own target.
    #[tokio::test]
    async fn apply_does_not_deploy_shared_content_over_a_hidden_override() {
        let dirs = TestDirs::new();
        std::fs::create_dir_all(dirs.package_dir.join("myapp")).unwrap();
        std::fs::write(dirs.package_dir.join("myapp/shared.conf"), "SHARED").unwrap();
        std::fs::write(dirs.package_dir.join("myapp/work.conf"), "WORK").unwrap();
        let target = dirs.target_dir.join("config.toml");

        write_package_yaml(
            &dirs.package_dir,
            "myapp",
            &format!(
                r#"name: myapp
extra:
  ? [a, b]
  : v
dotfiles:
  - source: "myapp/shared.conf"
    target: "{0}"
_environments:
  test:
    dotfiles:
      - source: "myapp/work.conf"
        target: "{0}"
environments:
  test: {{ install: "echo installed" }}
"#,
                target.display()
            ),
        );

        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        // The harm first, so a regression reports the content that landed rather
        // than a count that only implies it.
        assert!(
            !target.exists(),
            "the shared entry must not deploy over the target the override names, but '{}' \
             contains {:?}",
            target.display(),
            std::fs::read_to_string(&target).ok()
        );
        assert_eq!(refused_count(&events), 1, "{:?}", warning_messages(&events));
    }

    // The same unread file with nothing left to deploy. Kept alongside the one
    // above so the pair pins that what a package appears to have to deploy does
    // not enter the decision: both are refused, and for the same reason.
    #[tokio::test]
    async fn apply_refuses_an_unchecked_file_that_would_deploy_nothing() {
        let dirs = TestDirs::new();

        write_package_yaml(
            &dirs.package_dir,
            "myapp",
            r#"name: myapp
extra:
  ? [a, b]
  : v
environments:
  test:
    install: "echo installed"
"#,
        );

        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        assert_eq!(
            refused_count(&events),
            1,
            "a package that deploys nothing and could not be checked is a refusal, not a quiet \
             skip: {:?}",
            warning_messages(&events)
        );

        let warnings = warning_messages(&events);
        assert!(
            warnings.iter().any(
                |w| w.contains("Skipping package 'myapp'") && w.contains("cannot be ruled out")
            ),
            "the refusal must name the package and what could not be ruled out: {warnings:?}"
        );
    }

    // One file, and both surfaces speak about it -- neither may go quiet on a
    // check that never ran.
    //
    // They speak at the same strength and in the same words. Apply refuses,
    // because it would otherwise deploy content the unread key may have changed,
    // and validate calls the file invalid for the same reason: no key of the top
    // level was examined, so an unrecognized one cannot be ruled out. One
    // sentence, so `sync push` reports the problem once rather than in two
    // wordings a reader cannot connect.
    #[tokio::test]
    async fn apply_and_validate_both_report_a_file_that_cannot_be_re_read() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        package_that_cannot_be_re_read(&dirs, &target);

        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;
        assert!(
            warning_messages(&events)
                .iter()
                .any(|w| w.contains("cannot be ruled out")),
            "apply stayed quiet: {:?}",
            warning_messages(&events)
        );

        use selfie::package::port::PackageRepository;

        let repo = YamlPackageRepository::new(
            RealFileSystem,
            dirs.package_dir.clone(),
            SpecOrigin::PackageDirectory,
        );
        let package = repo.get_package("myapp").expect("fixture must load");
        let result = package.package().validate("test");
        // The same clause apply's warning carries, asserted on both sides above.
        // Wording the two separately is what made one problem arrive twice.
        assert!(
            result
                .issues()
                .errors()
                .iter()
                .any(|issue| issue.message().contains("cannot be ruled out")),
            "validate must report the refusal's own sentence: {:?}",
            result.issues()
        );
    }

    // A plain misspelling, no anchor involved: `configs:` for `dotfiles:`.
    //
    // `selfie spec validate` has always called this an error and the writer has
    // always refused to rewrite over it, while apply deployed what was left and
    // exited zero -- so the file's own author was told three different things by
    // three commands (selfie-jt6m). The entry under the real `dotfiles:` key is
    // what makes the refusal observable: it would deploy if the package were not
    // refused.
    #[tokio::test]
    async fn apply_refuses_a_plain_unrecognized_top_level_key() {
        let dirs = TestDirs::new();
        std::fs::create_dir_all(dirs.package_dir.join("myapp")).unwrap();
        std::fs::write(dirs.package_dir.join("myapp/config.toml"), "REPO").unwrap();
        let target = dirs.target_dir.join("config.toml");

        write_package_yaml(
            &dirs.package_dir,
            "myapp",
            &format!(
                r#"name: myapp
configs:
  - source: "myapp/other.toml"
    target: "{}"
dotfiles:
  - source: "myapp/config.toml"
    target: "{}"
environments:
  test:
    install: "echo installed"
"#,
                dirs.target_dir.join("other.toml").display(),
                target.display()
            ),
        );

        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        assert_eq!(refused_count(&events), 1);
        assert!(
            !target.exists(),
            "the refusal must precede the deploy, or the file is read on a guess about its keys"
        );

        let warnings = warning_messages(&events);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("Skipping package 'myapp'")
                    && w.contains("unknown field 'configs'")),
            "the refusal must name the package and the key: {warnings:?}"
        );
        // The anchor wording belongs to a key that collides with a field name.
        // `configs` collides with nothing, and telling its author to rename an
        // anchor they did not write sends them looking for one.
        assert!(
            !warnings
                .iter()
                .any(|w| w.contains("cannot be told apart from a misspelling")),
            "a plain unknown key must not be described as an anchor collision: {warnings:?}"
        );
    }

    // Any top-level field counts, not just `dotfiles`.
    //
    // `homepage` is a package field and not an environment one, so a check
    // wired to the wrong level leaves this key unrefused. The `_dotfiles:` test
    // above cannot see that: `dotfiles` is a field at both levels.
    #[tokio::test]
    async fn apply_refuses_a_top_level_anchor_named_like_any_package_field() {
        let dirs = TestDirs::new();
        std::fs::create_dir_all(dirs.package_dir.join("myapp")).unwrap();
        std::fs::write(dirs.package_dir.join("myapp/config.toml"), "REPO").unwrap();
        let target = dirs.target_dir.join("config.toml");

        write_package_yaml(
            &dirs.package_dir,
            "myapp",
            &format!(
                r#"name: myapp
_homepage: &h "https://example.com"
homepage: *h
dotfiles:
  - source: "myapp/config.toml"
    target: "{}"
environments:
  test:
    install: "echo installed"
"#,
                target.display()
            ),
        );

        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        assert_eq!(refused_count(&events), 1);
        assert!(!target.exists(), "nothing should have been deployed");

        let warnings = warning_messages(&events);
        assert!(
            warnings.iter().any(|w| w.contains("_homepage")
                && w.contains("cannot be told apart from a misspelling")),
            "the refusal must name the key: {warnings:?}"
        );
    }

    // A package with genuinely no dotfiles is still silent.
    //
    // Without this, an implementation that refused every package reaching the
    // empty-dotfiles check would pass the first test. It is also the control for
    // the unchecked-file warning above: a build that warned about every package
    // would fail here.
    #[tokio::test]
    async fn a_package_with_no_dotfiles_at_all_is_not_refused() {
        let dirs = TestDirs::new();
        write_package_yaml(
            &dirs.package_dir,
            "myapp",
            r#"name: myapp
environments:
  test:
    install: "echo installed"
"#,
        );

        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        assert_eq!(refused_count(&events), 0);
        assert_eq!(warning_messages(&events), Vec::<String>::new());
    }

    // An unread top level is refused wherever the entries live. The two tests
    // above put theirs at the top level; this one's are the environment's, and
    // the answer does not depend on which.
    #[tokio::test]
    async fn an_unchecked_file_deploying_only_environment_entries_is_refused() {
        let dirs = TestDirs::new();
        std::fs::create_dir_all(dirs.package_dir.join("myapp")).unwrap();
        std::fs::write(dirs.package_dir.join("myapp/config.toml"), "REPO").unwrap();
        let target = dirs.target_dir.join("config.toml");

        write_package_yaml(
            &dirs.package_dir,
            "myapp",
            &format!(
                r#"name: myapp
extra:
  ? [a, b]
  : v
environments:
  test:
    install: "echo installed"
    dotfiles:
      - source: "myapp/config.toml"
        target: "{}"
"#,
                target.display()
            ),
        );

        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        assert_eq!(
            refused_count(&events),
            1,
            "an unread top level is refused wherever the entries live: {:?}",
            warning_messages(&events)
        );
        assert!(
            !target.exists(),
            "the environment's entry must not deploy from a file nothing checked"
        );
    }
}

// Targets that are neither absent nor a regular file.
//
// A fifo target hung `selfie apply` forever and a device node was written to
// (selfie-qwj3). The hang is why every test here runs through `within_deadline`:
// reaching the unguarded path must fail a test, not wedge it.
mod irregular_targets {
    use super::*;
    use std::path::Path;
    use std::time::Duration;

    // Long enough that a slow machine does not trip it, short enough that a real
    // hang is caught quickly.
    const DEADLINE: Duration = Duration::from_secs(10);

    fn make_fifo(path: &Path) {
        nix::unistd::mkfifo(path, nix::sys::stat::Mode::S_IRWXU).unwrap();
    }

    fn warning_messages(events: &[PackageEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                PackageEvent::Warning { message, .. } => Some(message.clone()),
                _ => None,
            })
            .collect()
    }

    // One package, one entry, whose target is `target`.
    fn package_targeting(dirs: &TestDirs, target: &Path) {
        std::fs::create_dir_all(dirs.package_dir.join("myapp")).unwrap();
        std::fs::write(dirs.package_dir.join("myapp/config.toml"), "REPO").unwrap();
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );
    }

    // Apply refuses a fifo target instead of hanging on it.
    //
    // Before the guard, the *checksum read* blocked — not the write. Opening a
    // fifo for reading waits for a writer exactly as opening it for writing waits
    // for a reader, and that read happens well before any write is attempted.
    #[test]
    fn apply_refuses_a_fifo_target_without_hanging() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        make_fifo(&target);
        package_targeting(&dirs, &target);

        let service = dirs.service();
        let events = within_deadline(DEADLINE, move || async move {
            collect_events(service.apply_all(ApplyOptions::default()).await).await
        })
        .expect("apply must not block on a fifo target");

        assert_eq!(refused_count(&events), 1);
        let warnings = warning_messages(&events);
        assert!(
            warnings.iter().any(|w| w.contains("named pipe (fifo)")),
            "the refusal must name what it found: {warnings:?}"
        );
    }

    // A symlink pointing at a fifo is refused too.
    //
    // The case a non-following stat misses. `symlink_refusal` answers "it is a
    // symlink" and returns before the fifo is ever considered, and the target
    // read then follows the link and blocks — the guard present, the hang intact.
    #[test]
    fn apply_refuses_a_symlink_to_a_fifo_without_hanging() {
        let dirs = TestDirs::new();
        let fifo = dirs.target_dir.join("real-fifo");
        make_fifo(&fifo);
        let target = dirs.target_dir.join("config.toml");
        std::os::unix::fs::symlink(&fifo, &target).unwrap();
        package_targeting(&dirs, &target);

        let service = dirs.service();
        let events = within_deadline(DEADLINE, move || async move {
            collect_events(service.apply_all(ApplyOptions::default()).await).await
        })
        .expect("apply must not block on a symlink to a fifo");

        assert_eq!(refused_count(&events), 1);
        let warnings = warning_messages(&events);
        assert!(
            warnings.iter().any(|w| w.contains("named pipe (fifo)")),
            "a link to a fifo must be refused as a fifo: {warnings:?}"
        );
    }

    // Apply refuses a character device rather than writing to it.
    #[test]
    fn apply_refuses_a_character_device_target() {
        let dirs = TestDirs::new();
        package_targeting(&dirs, Path::new("/dev/null"));

        let service = dirs.service();
        let events = within_deadline(DEADLINE, move || async move {
            collect_events(service.apply_all(ApplyOptions::default()).await).await
        })
        .expect("apply must not block on a device target");

        assert_eq!(refused_count(&events), 1);
        let warnings = warning_messages(&events);
        assert!(
            warnings.iter().any(|w| w.contains("character device")),
            "the refusal must name what it found: {warnings:?}"
        );
    }

    // Drift refuses a fifo target instead of hanging on it.
    //
    // Drift checksums the target exactly as apply does, so it hung on the same
    // open — which selfie-qwj3 does not mention and which the fix has to cover
    // for the two commands to keep agreeing.
    #[test]
    fn drift_refuses_a_fifo_target_without_hanging() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        make_fifo(&target);
        package_targeting(&dirs, &target);

        let service = dirs.service();
        let events = within_deadline(DEADLINE, move || async move {
            collect_events(service.check_drift().await).await
        })
        .expect("drift must not block on a fifo target");

        let warnings = warning_messages(&events);
        assert!(
            warnings.iter().any(|w| w.contains("named pipe (fifo)")),
            "drift must refuse it the same way apply does: {warnings:?}"
        );
    }

    // Track refuses a fifo target instead of copying it into the repository.
    #[test]
    fn track_refuses_a_fifo_target_without_hanging() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        make_fifo(&target);

        let service = dirs.service_with_dotfiles();
        let tracked = target.to_str().unwrap().to_string();
        let events = within_deadline(DEADLINE, move || async move {
            collect_events(service.track_standalone("myapp", &tracked).await).await
        })
        .expect("track must not block on a fifo target");

        match get_operation_result(&events).expect("no Completed event") {
            OperationResult::Failure(failure) => assert!(
                failure.to_string().contains("named pipe (fifo)"),
                "got: {failure}"
            ),
            other => panic!("tracking a fifo must fail, got {other:?}"),
        }
    }

    // Control: an ordinary target still deploys.
    //
    // Without this, a guard that refused every target would pass every test
    // above.
    #[test]
    fn a_regular_target_is_still_deployed() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        package_targeting(&dirs, &target);

        let service = dirs.service();
        let events = within_deadline(DEADLINE, move || async move {
            collect_events(service.apply_all(ApplyOptions::default()).await).await
        })
        .expect("a regular target must not block");

        assert_eq!(refused_count(&events), 0, "{:?}", warning_messages(&events));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "REPO");
    }
}

// A write failure names the target exactly once.
//
// The writer re-tags its IO errors with the target path, so a warning that
// prefixes the path as well prints it twice.
mod write_failure_warnings {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    // An existing target inside a directory the user cannot write to is
    // refused, because the replacement has to be created beside it, and the
    // original is left as it was.
    #[tokio::test]
    async fn an_unwritable_target_directory_is_named_once() {
        if nix::unistd::Uid::effective().is_root() {
            eprintln!("SKIP an_unwritable_target_directory_is_named_once: running as root");
            return;
        }
        let dirs = TestDirs::new();
        std::fs::create_dir_all(dirs.package_dir.join("myapp")).unwrap();
        std::fs::write(dirs.package_dir.join("myapp/config.toml"), "REPO").unwrap();
        let locked = dirs.target_dir.join("locked");
        std::fs::create_dir(&locked).unwrap();
        let target = locked.join("config.toml");
        std::fs::write(&target, "OLD").unwrap();
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();

        let options = ApplyOptions {
            // Without this the entry is a conflict and never reaches the write.
            auto_accept: true,
            ..Default::default()
        };
        let events = collect_events(dirs.service().apply_all(options).await).await;

        // Restore before asserting, so a failure still leaves a removable
        // temporary directory behind.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();

        let warnings = warning_messages(&events);
        let failure = warnings
            .iter()
            .find(|w| w.starts_with("Failed to write: "))
            .unwrap_or_else(|| panic!("no write failure was reported: {warnings:?}"));
        assert_eq!(
            failure.matches(target.to_str().unwrap()).count(),
            1,
            "the target is not named exactly once: {failure}"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "OLD");
        assert_eq!(refused_count(&events), 1);
    }
}

// The permanent `not tracked` drift line for a target selfie will never manage.
//
// An untracked dotfile whose target is a symlink and whose contents already match
// is `Skip`: apply writes nothing, refuses nothing, records nothing. So
// `detect_drift` keeps answering `NotTracked` and `dotfiles drift` keeps listing
// it forever with nothing saying why (selfie-ktha).
//
// Both commands say the same sentence in the channel each already uses, raising
// no warning that did not exist, which is what keeps the two in-sync tests
// passing unmodified.
mod unmanageable_symlink_reason {
    use super::*;
    use std::path::Path;

    // An untracked entry, already in sync, whose target is `link` or a plain
    // file — the axis under test.
    fn in_sync_entry(dirs: &TestDirs, target: &Path, symlinked: bool) {
        std::fs::create_dir_all(dirs.package_dir.join("myapp")).unwrap();
        std::fs::write(dirs.package_dir.join("myapp/config.toml"), "SAME").unwrap();

        if symlinked {
            let destination = dirs.target_dir.join("destination");
            std::fs::write(&destination, "SAME").unwrap();
            std::os::unix::fs::symlink(&destination, target).unwrap();
        } else {
            std::fs::write(target, "SAME").unwrap();
        }

        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );
    }

    fn skip_reasons(events: &[PackageEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                PackageEvent::DotfileSkipped { reason, .. } => Some(reason.clone()),
                _ => None,
            })
            .collect()
    }

    fn drift_reasons(events: &[PackageEvent]) -> Vec<Option<String>> {
        events
            .iter()
            .filter_map(|event| match event {
                PackageEvent::DotfileDriftDetected { reason, .. } => Some(reason.clone()),
                _ => None,
            })
            .collect()
    }

    // Apply says why it is leaving the entry alone.
    #[tokio::test]
    async fn apply_says_why_an_in_sync_symlinked_target_will_not_settle() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        in_sync_entry(&dirs, &target, true);

        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        let reasons = skip_reasons(&events);
        assert!(
            reasons.iter().any(|r| r.contains("already in sync")
                && r.contains("symlink")
                && r.contains("records no deployment")),
            "the skip line must carry the reason: {reasons:?}"
        );
    }

    // Drift says the same thing, on the line that keeps reappearing.
    #[tokio::test]
    async fn drift_says_why_the_not_tracked_line_never_clears() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        in_sync_entry(&dirs, &target, true);

        // Apply first: this is the state the user is actually in, having run apply
        // and found the drift line still there afterwards.
        let _ = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;
        let events = collect_events(dirs.service().check_drift().await).await;

        let reasons = drift_reasons(&events);
        assert_eq!(reasons.len(), 1, "expected one drift line: {reasons:?}");
        assert!(
            reasons[0]
                .as_deref()
                .is_some_and(|r| r.contains("symlink") && r.contains("will not manage")),
            "the drift line must carry the reason: {reasons:?}"
        );
    }

    // The control: a plain untracked in-sync target gets no reason from either
    // command.
    //
    // It settles on the first apply, so there is nothing to explain. Without
    // this, a change that attached the reason unconditionally would satisfy both
    // tests above.
    #[tokio::test]
    async fn a_plain_in_sync_target_gets_no_reason_from_either_command() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        in_sync_entry(&dirs, &target, false);

        let apply = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;
        let reasons = skip_reasons(&apply);
        assert!(
            reasons.iter().all(|r| !r.contains("symlink")),
            "a plain target has nothing to explain: {reasons:?}"
        );

        // And having been recorded, it produces no drift line at all.
        let drift = collect_events(dirs.service().check_drift().await).await;
        assert_eq!(
            drift_reasons(&drift),
            Vec::<Option<String>>::new(),
            "a plain target settles, so there is no line to annotate"
        );
    }

    // A *tracked* symlinked target gets no reason, and that boundary is deliberate.
    //
    // Found by mutation: widening the condition from `NotTracked` to include
    // `None` failed nothing, because every other fixture varies the symlink axis
    // and none varies the drift type.
    //
    // An entry deployed and later symlinked is selfie-v7py: it produces no drift
    // line at all, so there is nothing here that should speak. Whoever fixes
    // v7py has to change this test on purpose.
    #[tokio::test]
    async fn a_tracked_symlinked_target_is_outside_this_reason() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");

        // Deploy to a plain file first, so the entry is tracked.
        std::fs::create_dir_all(dirs.package_dir.join("myapp")).unwrap();
        std::fs::write(dirs.package_dir.join("myapp/config.toml"), "SAME").unwrap();
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );
        let first = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;
        assert!(
            skip_reasons(&first).is_empty(),
            "control: the first apply deploys rather than skipping"
        );

        // Now move it aside and link to it: same content, still tracked.
        let destination = dirs.target_dir.join("destination");
        std::fs::rename(&target, &destination).unwrap();
        std::os::unix::fs::symlink(&destination, &target).unwrap();

        let apply = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;
        let reasons = skip_reasons(&apply);
        assert!(
            reasons.iter().all(|r| !r.contains("symlink")),
            "a tracked entry is not what this reason is about: {reasons:?}"
        );

        let drift = collect_events(dirs.service().check_drift().await).await;
        assert_eq!(
            drift_reasons(&drift),
            Vec::<Option<String>>::new(),
            "selfie-v7py: no drift line at all, so nothing to annotate"
        );
    }

    // The drift classification stays a bare label.
    //
    // The reason travels in its own field: the MCP server serializes
    // `drift_type` as a typed value and the CLI prints it as one, so appending
    // prose to it would corrupt what a caller matches on.
    #[tokio::test]
    async fn the_reason_does_not_contaminate_the_drift_type() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        in_sync_entry(&dirs, &target, true);

        let events = collect_events(dirs.service().check_drift().await).await;

        let types: Vec<String> = events
            .iter()
            .filter_map(|event| match event {
                PackageEvent::DotfileDriftDetected { drift_type, .. } => Some(drift_type.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(types, vec!["not tracked".to_string()]);
    }
}

// The copy `track` makes *into* the repository must not follow a symlink at its
// destination either.
//
// Distinct from every refusal above, which concerns the dotfile target. These
// are about the path selfie composes for itself and writes the user's file into.
//
// Every fixture plants a **dangling** link on purpose. A link to an existing
// file is caught by the `path_exists` guard; a dangling one returns `false`
// there, passes the guard, and reaches the write. selfie-yw7i
mod repository_writes_do_not_follow_symlinks {
    use super::*;
    use std::path::{Path, PathBuf};

    // Where the planted link points: outside both managed directories, so a
    // write landing there is unambiguous evidence the link was followed.
    fn planted_link_to(dir: &Path, link: &Path) -> PathBuf {
        let destination = dir.join("planted-destination");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&destination, link).unwrap();
        assert!(
            !destination.exists(),
            "the link must dangle, or the existence check refuses it before the write"
        );
        destination
    }

    // The refusal names the repository path and does not describe it as a target.
    //
    // Asserts the absence of the two `FileSystemError` `Display` **phrases**
    // rather than of the bare word "target". A tempdir path here genuinely
    // contains that word -- `TestDirs` names one directory `target`, and the
    // planted link points into it -- so a substring check on the word alone
    // fails on the fixture's own paths while the wording is correct. The
    // phrases are what a reversion to rendering the error verbatim would
    // reintroduce, and they cannot appear in a path.
    fn assert_refused_without_calling_it_a_target(failure: &str, repository_path: &Path) {
        for phrase in ["target is a symlink", "target resolves to a"] {
            assert!(
                !failure.contains(phrase),
                "a repository path was described as a target: {failure}"
            );
        }
        let name = repository_path.file_name().unwrap().to_string_lossy();
        assert!(
            failure.contains(name.as_ref()),
            "the refusal does not name the repository path: {failure}"
        );
    }

    #[tokio::test]
    async fn track_standalone_refuses_a_symlinked_source_path() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("gemrc");
        std::fs::write(&target, "gem: --no-document").unwrap();

        // Exactly where `handle_track_standalone` will compose its copy.
        let source = dirs.dotfiles_dir.join("gemrc").join("gemrc");
        let destination = planted_link_to(&dirs.target_dir, &source);

        let events = collect_events(
            dirs.service_with_dotfiles()
                .track_standalone("gemrc", target.to_str().unwrap())
                .await,
        )
        .await;

        match get_operation_result(&events).expect("no Completed event") {
            OperationResult::Failure(failure) => {
                assert_refused_without_calling_it_a_target(&failure.to_string(), &source);
            }
            other => panic!("tracking through a symlinked source path must fail, got {other:?}"),
        }

        assert!(
            !destination.exists(),
            "the write followed the link and landed at {}",
            destination.display()
        );

        // The copy is attempted before the spec is saved, so a refused copy
        // leaves no spec at all. Reversing the two would write a spec naming a
        // file that is not there, which every later apply and drift reports.
        assert!(
            !dirs.dotfiles_dir.join("gemrc.yml").exists(),
            "a refused copy still wrote the spec"
        );
    }

    #[tokio::test]
    async fn track_for_package_refuses_a_symlinked_source_path() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("gemrc");
        std::fs::write(&target, "gem: --no-document").unwrap();

        let spec = dirs.package_dir.join("ruby.yml");
        std::fs::write(
            &spec,
            "name: ruby\nversion: 1.0.0\nenvironments:\n  test:\n    install: true\n",
        )
        .unwrap();
        let before = std::fs::read(&spec).unwrap();

        // `handle_track_for_package` composes alongside the package YAML.
        let source = dirs.package_dir.join("ruby").join("gemrc");
        let destination = planted_link_to(&dirs.target_dir, &source);

        let events = collect_events(
            dirs.service_with_dotfiles()
                .track_for_package("ruby", target.to_str().unwrap())
                .await,
        )
        .await;

        match get_operation_result(&events).expect("no Completed event") {
            OperationResult::Failure(failure) => {
                assert_refused_without_calling_it_a_target(&failure.to_string(), &source);
            }
            other => panic!("tracking through a symlinked source path must fail, got {other:?}"),
        }

        assert!(
            !destination.exists(),
            "the write followed the link and landed at {}",
            destination.display()
        );

        // The copy is attempted before the spec is saved, so a refused copy
        // leaves the spec byte-for-byte as it was. Reversing the two would add a
        // `dotfiles:` entry naming a file that is not there, which every later
        // apply and drift reports. Compared as bytes rather than for an absent
        // key, so a rewrite that changed anything at all fails here.
        assert_eq!(
            std::fs::read(&spec).unwrap(),
            before,
            "a refused copy still rewrote the spec"
        );
    }

    // The control, and the reason the two tests above are not vacuous: with no
    // link planted, the identical fixture tracks successfully and the copy lands
    // at the composed path. A guard that refused every track would pass both
    // tests above and fail here.
    #[tokio::test]
    async fn an_unobstructed_source_path_is_still_written() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("gemrc");
        std::fs::write(&target, "gem: --no-document").unwrap();

        let events = collect_events(
            dirs.service_with_dotfiles()
                .track_standalone("gemrc", target.to_str().unwrap())
                .await,
        )
        .await;

        assert!(
            matches!(
                get_operation_result(&events).expect("no Completed event"),
                OperationResult::Success(_)
            ),
            "an ordinary track must still succeed: {:?}",
            warning_messages(&events)
        );
        assert_eq!(
            std::fs::read_to_string(dirs.dotfiles_dir.join("gemrc").join("gemrc")).unwrap(),
            "gem: --no-document"
        );
    }
}

// Sources that are neither absent nor a regular file.
//
// The `irregular_targets` module above covers the dotfile *target*. This is the
// same defect on the other side of the copy: a fifo committed into the
// repository is read as a source, and reading one blocks until a writer arrives.
//
// Four reads, not one: `handle_apply`, `handle_check_drift`, `resolve_content`'s
// `Template` arm, and `read_referenced_file`. Each runs through `within_deadline`,
// so a read that blocks fails its test. selfie-lwv5
mod irregular_sources {
    use super::*;
    use std::path::Path;
    use std::time::Duration;

    const DEADLINE: Duration = Duration::from_secs(10);

    fn make_fifo(path: &Path) {
        nix::unistd::mkfifo(path, nix::sys::stat::Mode::S_IRWXU).unwrap();
    }

    fn warning_messages(events: &[PackageEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                PackageEvent::Warning { message, .. } => Some(message.clone()),
                _ => None,
            })
            .collect()
    }

    // One package, one entry, whose **source** is a fifo in the repository.
    fn package_with_fifo_source(dirs: &TestDirs, target: &Path) {
        std::fs::create_dir_all(dirs.package_dir.join("myapp")).unwrap();
        make_fifo(&dirs.package_dir.join("myapp/config.toml"));
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );
    }

    // The wording every source-side refusal shares.
    #[track_caller]
    fn assert_names_the_repository_file(warnings: &[String]) {
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("named pipe (fifo)") && w.contains("repository file")),
            "no warning named the repository file as a fifo: {warnings:?}"
        );
        // The target-side wording would send the user to inspect the wrong file:
        // the problem is in the repository they sync, not at the deploy target.
        assert!(
            !warnings.iter().any(|w| w.contains("target resolves to a")),
            "a source refusal used the target-side wording: {warnings:?}"
        );
    }

    #[test]
    fn apply_refuses_a_fifo_source_without_hanging() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        package_with_fifo_source(&dirs, &target);

        let service = dirs.service();
        let events = within_deadline(DEADLINE, move || async move {
            collect_events(service.apply_all(ApplyOptions::default()).await).await
        })
        .expect("apply must not block on a fifo source");

        assert_names_the_repository_file(&warning_messages(&events));
        assert!(
            !target.exists(),
            "nothing may be deployed from a source selfie refused to read"
        );
    }

    #[test]
    fn drift_refuses_a_fifo_source_without_hanging() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        std::fs::write(&target, "whatever").unwrap();
        package_with_fifo_source(&dirs, &target);

        let service = dirs.service();
        let events = within_deadline(DEADLINE, move || async move {
            collect_events(service.check_drift().await).await
        })
        .expect("drift must not block on a fifo source");

        assert_names_the_repository_file(&warning_messages(&events));
    }

    // The secret-bearing read: a `source:` + `vars:` template.
    //
    // The bead guessed this path was unaffected because a provider resolves by
    // running a command. That is true of `command:` entries only -- a template
    // entry reads a repository file like any other, and this is the read that
    // hangs while apply is handling a credential.
    #[test]
    fn a_fifo_template_is_refused_without_hanging() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");

        std::fs::create_dir_all(dirs.package_dir.join("creds")).unwrap();
        make_fifo(&dirs.package_dir.join("creds/credentials.tpl"));
        std::fs::write(
            dirs.package_dir.join("creds.yml"),
            format!(
                "name: creds\nenvironments:\n  test:\n    install: \"echo i\"\ndotfiles:\n  \
                 - source: \"creds/credentials.tpl\"\n    target: \"{}\"\n    vars:\n      \
                 token: \"echo secret\"\n",
                target.display()
            ),
        )
        .unwrap();

        let service = dirs.service();
        let events = within_deadline(DEADLINE, move || async move {
            collect_events(service.apply_all(ApplyOptions::default()).await).await
        })
        .expect("apply must not block on a fifo template");

        let warnings = warning_messages(&events);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("named pipe (fifo)") && w.contains("repository file")),
            "no warning named the template as a fifo: {warnings:?}"
        );
        assert!(
            !target.exists(),
            "nothing may be written from a template selfie refused to read"
        );
    }

    // The control for all three, and the reason none of them is vacuous: the
    // same fixtures with a *regular* source deploy and report drift normally. A
    // guard that refused every source would pass the three tests above.
    #[test]
    fn a_regular_source_is_still_deployed() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");

        std::fs::create_dir_all(dirs.package_dir.join("myapp")).unwrap();
        std::fs::write(dirs.package_dir.join("myapp/config.toml"), "REPO").unwrap();
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );

        let service = dirs.service();
        let events = within_deadline(DEADLINE, move || async move {
            collect_events(service.apply_all(ApplyOptions::default()).await).await
        })
        .expect("apply must not block");

        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "REPO",
            "an ordinary source must still deploy: {:?}",
            warning_messages(&events)
        );
    }
}

// selfie-tcu2: `sudo selfie apply` writes every entry as root, including the
// `~/` ones, and on a machine where sudo resets `$HOME` it writes them to
// `/root` and reports success. These pin the refusal, and — more importantly —
// that nothing was written before it fired.
mod running_under_sudo {
    use super::*;

    fn repo_source(dirs: &TestDirs, relative: &str, content: &str) {
        let path = dirs.package_dir.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    // A package with one deployable entry. Returns the target it deploys to, so
    // a test can assert on the file that must not appear.
    fn deployable(dirs: &TestDirs) -> PathBuf {
        let target = dirs.target_dir.join("config.toml");
        repo_source(dirs, "myapp/config.toml", "theme = \"dark\"\n");
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );
        target
    }

    fn state_file(dirs: &TestDirs) -> PathBuf {
        dirs.state_dir.join("deploy-state.yml")
    }

    fn was_refused(events: &[PackageEvent]) -> bool {
        matches!(
            get_operation_result(events),
            Some(OperationResult::Failure(OperationFailure::Privilege(_)))
        )
    }

    // The refusal is asserted by variant rather than by message, so rewording it
    // cannot silently turn this into a test of prose.
    #[tokio::test]
    async fn apply_all_is_refused_and_writes_nothing() {
        let dirs = TestDirs::new().running_as(Elevation::Sudo);
        let target = deployable(&dirs);

        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        assert!(
            was_refused(&events),
            "expected a privilege refusal, got: {:?}",
            get_operation_result(&events)
        );
        assert!(
            !target.exists(),
            "the target was written before the run was refused"
        );
        assert!(
            !state_file(&dirs).exists(),
            "deploy state was written as root; the next ordinary run could not read it"
        );
    }

    #[tokio::test]
    async fn apply_for_one_package_is_refused_and_writes_nothing() {
        let dirs = TestDirs::new().running_as(Elevation::Sudo);
        let target = deployable(&dirs);

        let events =
            collect_events(dirs.service().apply("myapp", ApplyOptions::default()).await).await;

        assert!(
            was_refused(&events),
            "expected a privilege refusal, got: {:?}",
            get_operation_result(&events)
        );
        assert!(!target.exists());
        assert!(!state_file(&dirs).exists());
    }

    // A dry run writes no dotfile, but it still runs the user's provider
    // commands — as root, against root's environment — so its answer is not
    // trustworthy either. One rule, no exemption.
    #[tokio::test]
    async fn a_dry_run_is_refused_too() {
        let dirs = TestDirs::new().running_as(Elevation::Sudo);
        deployable(&dirs);

        let options = ApplyOptions {
            dry_run: true,
            ..Default::default()
        };
        let events = collect_events(dirs.service().apply_all(options).await).await;

        assert!(
            was_refused(&events),
            "expected a privilege refusal, got: {:?}",
            get_operation_result(&events)
        );
    }

    // Track writes into the dotfiles repository and the state file, so it is
    // gated for the same reason apply is.
    #[tokio::test]
    async fn track_standalone_is_refused_and_writes_nothing() {
        let dirs = TestDirs::new().running_as(Elevation::Sudo);
        let target = dirs.target_dir.join("starship.toml");
        std::fs::write(&target, "format = \"$all\"\n").unwrap();

        let events = collect_events(
            dirs.service_with_dotfiles()
                .track_standalone("starship", target.to_str().unwrap())
                .await,
        )
        .await;

        assert!(
            was_refused(&events),
            "expected a privilege refusal, got: {:?}",
            get_operation_result(&events)
        );
        assert!(
            !dirs.dotfiles_dir.join("starship.yml").exists(),
            "a spec was written into the dotfiles repository as root"
        );
        assert!(!state_file(&dirs).exists());
    }

    #[tokio::test]
    async fn track_for_package_is_refused_and_leaves_the_spec_alone() {
        let dirs = TestDirs::new().running_as(Elevation::Sudo);
        create_package_with_dotfiles(&dirs.package_dir, "bat", &[]);
        let before = std::fs::read_to_string(dirs.package_dir.join("bat.yml")).unwrap();

        let target = dirs.target_dir.join("bat-config");
        std::fs::write(&target, "--theme=ansi\n").unwrap();

        let events = collect_events(
            dirs.service()
                .track_for_package("bat", target.to_str().unwrap())
                .await,
        )
        .await;

        assert!(
            was_refused(&events),
            "expected a privilege refusal, got: {:?}",
            get_operation_result(&events)
        );
        assert_eq!(
            std::fs::read_to_string(dirs.package_dir.join("bat.yml")).unwrap(),
            before,
            "the package spec was rewritten as root"
        );
        assert!(!state_file(&dirs).exists());
    }

    // The control that keeps this rule from collapsing into "refuse at any euid
    // 0". A container, a CI job, or root managing root's own dotfiles is a real
    // use, and gating it behind a flag prevents an accident that cannot happen
    // there.
    #[tokio::test]
    async fn real_root_deploys_normally() {
        let dirs = TestDirs::new().running_as(Elevation::Root);
        let target = deployable(&dirs);

        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        assert!(
            !was_refused(&events),
            "a root run with no SUDO_UID must not be refused"
        );
        assert!(
            target.exists(),
            "the entry should have deployed: {:?}",
            get_operation_result(&events)
        );
    }

    #[tokio::test]
    async fn allow_sudo_deploys_despite_sudo() {
        let dirs = TestDirs::new().running_as(Elevation::Sudo);
        let target = deployable(&dirs);

        let dirs = dirs.allowing_sudo();
        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        assert!(!was_refused(&events), "--allow-sudo must override the gate");
        assert!(
            target.exists(),
            "the entry should have deployed: {:?}",
            get_operation_result(&events)
        );
    }

    // Drift writes nothing, so gating it would be friction for no gain. This is
    // also the control proving the gate is scoped rather than global — a check
    // added to every method passes every test above and fails this one.
    //
    // Asserts drift *succeeded*, not merely that it was not refused by name. The
    // weaker form was written first and a mutation walked straight through it: a
    // gate reporting `Generic` rather than `Privilege` stops drift dead and still
    // satisfies "the failure is not a privilege refusal".
    #[tokio::test]
    async fn drift_is_not_gated() {
        let dirs = TestDirs::new().running_as(Elevation::Sudo);
        deployable(&dirs);

        let events = collect_events(dirs.service().check_drift().await).await;

        assert!(
            matches!(
                get_operation_result(&events),
                Some(OperationResult::Success(_))
            ),
            "drift is read-only and must still run under sudo, got: {:?}",
            get_operation_result(&events)
        );
    }
}

// The three surfaces that answer "is this package usable" now ask one function,
// so a reader cannot be told the file is fine by one command and refused by the
// next. Each test here deletes one surface's answer if the consult is removed --
// which is the only way to know a shared predicate is actually shared, rather
// than written once and called from one place.
mod apply_and_drift_agree {
    use super::*;

    // A top-level key that shadows a real field. Selfie cannot tell it from a
    // misspelling of `dotfiles:`, and reading it as an anchor empties the list.
    fn write_shadowed(dir: &std::path::Path, target: &std::path::Path) {
        std::fs::create_dir_all(dir.join("myapp")).unwrap();
        std::fs::write(dir.join("myapp/config.toml").as_path(), "SHARED").unwrap();
        write_package_yaml(
            dir,
            "myapp",
            &format!(
                "name: myapp\n_dotfiles:\n  - source: \"myapp/config.toml\"\n    target: \"{}\"\nenvironments:\n  test:\n    install: \"true\"\n",
                target.display()
            ),
        );
    }

    fn drift_refused(events: &[PackageEvent]) -> usize {
        drift_summary(events).2
    }

    // The state selfie-9a1h measured on main: apply refuses and names the key,
    // drift reports zero out of zero with a check mark.
    #[tokio::test]
    async fn drift_refuses_the_package_apply_refuses() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        write_shadowed(&dirs.package_dir, &target);

        let applied = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;
        let drifted = collect_events(dirs.service().check_drift().await).await;

        assert_eq!(
            refused_count(&applied),
            1,
            "apply must refuse it: {:?}",
            warning_messages(&applied)
        );
        assert_eq!(
            drift_refused(&drifted),
            1,
            "drift must refuse the same file: {:?}",
            warning_messages(&drifted)
        );
    }

    // Not just that both refuse, but that they say the same thing. Two commands
    // agreeing on the verdict and disagreeing on the reason is the shape this
    // predicate exists to stop, and only comparing the rendered text catches it.
    #[tokio::test]
    async fn both_commands_give_the_same_reason() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        write_shadowed(&dirs.package_dir, &target);

        let applied = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;
        let drifted = collect_events(dirs.service().check_drift().await).await;

        let from_apply: Vec<String> = warning_messages(&applied)
            .into_iter()
            .filter(|m| m.contains("Skipping package"))
            .collect();
        let from_drift: Vec<String> = warning_messages(&drifted)
            .into_iter()
            .filter(|m| m.contains("Skipping package"))
            .collect();

        assert_eq!(
            from_apply.len(),
            1,
            "apply must say it once: {from_apply:?}"
        );
        assert_eq!(
            from_apply, from_drift,
            "the two commands must give one reason, not two"
        );
    }

    // The rule-4 control, extended to drift. A tracked dotfile has no
    // environments by design, and refusing it here would be as wrong as
    // refusing it on apply -- with nothing else in the suite to notice.
    #[tokio::test]
    async fn drift_does_not_refuse_a_tracked_dotfile() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join(".gemrc");
        std::fs::create_dir_all(dirs.dotfiles_dir.join("gemrc")).unwrap();
        std::fs::write(dirs.dotfiles_dir.join("gemrc/.gemrc").as_path(), "GEM").unwrap();
        write_package_yaml(
            &dirs.dotfiles_dir,
            "gemrc",
            &format!(
                "name: gemrc\ndotfiles:\n  - source: \"gemrc/.gemrc\"\n    target: \"{}\"\n",
                target.display()
            ),
        );

        let events = collect_events(dirs.service_with_dotfiles().check_drift().await).await;

        assert_eq!(
            drift_refused(&events),
            0,
            "a tracked dotfile has no environments by design: {:?}",
            warning_messages(&events)
        );
    }
}

// A target that exists but cannot be read is refused: never shown as empty,
// never overwritten, and worded the same by apply and drift.
//
// The fixture is mode 0200 (owner write-only) rather than 0000. Today's
// in-place writer and a rename-based one can both replace a 0200 file, so an
// assertion that the file survived is live; a 0000 target is saved by the
// writer's own EACCES, which proves nothing about the decision under test.
mod unreadable_targets {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // A file held at mode 0200 until this is dropped. `Drop` restores 0600 so
    // the content can be read back and the temp directory removed, even when
    // an assertion panics first.
    struct UnreadableTarget(PathBuf);

    impl Drop for UnreadableTarget {
        fn drop(&mut self) {
            // Ignored on failure: panicking during an unwind aborts the test
            // binary and hides the assertion that started it.
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o600));
        }
    }

    // `None` when the mode does not bite, which is what root sees: the caller
    // prints a skip rather than asserting against a readable file.
    fn make_unreadable(path: &Path) -> Option<UnreadableTarget> {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o200)).unwrap();
        let guard = UnreadableTarget(path.to_path_buf());
        if std::fs::read(path).is_ok() {
            return None;
        }
        Some(guard)
    }

    fn skip(test: &str) {
        eprintln!("SKIP {test}: running as root, mode bits ignored");
    }

    // A package whose one entry targets `target`, with `content` in the repository.
    fn package_targeting(dirs: &TestDirs, content: &str, target: &Path) {
        let source = dirs.package_dir.join("myapp/config.toml");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(source, content).unwrap();
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );
    }

    fn count_of(events: &[PackageEvent], matcher: fn(&PackageEvent) -> bool) -> usize {
        events.iter().filter(|e| matcher(e)).count()
    }

    fn deployed(e: &PackageEvent) -> bool {
        matches!(e, PackageEvent::DotfileDeployed { .. })
    }

    fn conflicted(e: &PackageEvent) -> bool {
        matches!(e, PackageEvent::DotfileConflict { .. })
    }

    fn drifted(e: &PackageEvent) -> bool {
        matches!(e, PackageEvent::DotfileDriftDetected { .. })
    }

    // The one warning a run emitted, which must say the target could not be read
    // and name it, and must never describe it as empty.
    fn the_unreadable_warning(events: &[PackageEvent], target: &Path) -> String {
        let warnings = warning_messages(events);
        assert_eq!(warnings.len(), 1, "expected one refusal, got: {warnings:?}");
        let warning = warnings.into_iter().next().unwrap();
        assert!(
            warning.contains("could not be read") && warning.contains(&*target.to_string_lossy()),
            "the refusal must say the target could not be read and name it: {warning}"
        );
        assert!(
            !warning.to_lowercase().contains("empty"),
            "an unreadable target is not an empty one: {warning}"
        );
        warning
    }

    // A1. `--yes` accepts conflicts; it must not accept one selfie fabricated
    // from a read it could not perform.
    #[tokio::test]
    async fn apply_with_auto_accept_refuses_a_target_it_cannot_read() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        std::fs::write(&target, "EXISTING").unwrap();
        package_targeting(&dirs, "FROM REPO", &target);
        let Some(guard) = make_unreadable(&target) else {
            skip("apply_with_auto_accept_refuses_a_target_it_cannot_read");
            return;
        };

        let options = ApplyOptions {
            auto_accept: true,
            ..Default::default()
        };
        let events = collect_events(dirs.service().apply_all(options).await).await;

        assert_eq!(
            count_of(&events, deployed),
            0,
            "nothing may be written: {events:?}"
        );
        assert_eq!(
            count_of(&events, conflicted),
            0,
            "a refusal is not a conflict: {events:?}"
        );
        the_unreadable_warning(&events, &target);
        assert_eq!(refused_count(&events), 1);

        drop(guard);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "EXISTING");
    }

    // A2. The conflict prompt exists so a human can judge; it must not be
    // handed a diff that presents the target as empty.
    #[tokio::test]
    async fn an_unreadable_target_is_never_diffed_against_empty_content() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        std::fs::write(&target, "EXISTING").unwrap();
        package_targeting(&dirs, "FROM REPO", &target);
        let Some(guard) = make_unreadable(&target) else {
            skip("an_unreadable_target_is_never_diffed_against_empty_content");
            return;
        };

        let asked = Arc::new(AtomicUsize::new(0));
        let options = ApplyOptions {
            conflict_resolver: Some(Arc::new(Counting(Arc::clone(&asked)))),
            ..Default::default()
        };
        let events = collect_events(dirs.service().apply_all(options).await).await;

        assert_eq!(
            asked.load(Ordering::SeqCst),
            0,
            "the resolver must not be asked"
        );
        assert_eq!(count_of(&events, conflicted), 0, "got: {events:?}");
        assert!(
            !events
                .iter()
                .any(|e| format!("{e:?}").contains("+FROM REPO")),
            "no event may carry a diff adding the whole repository file: {events:?}"
        );

        drop(guard);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "EXISTING");
    }

    // A3. Drift cannot say whether an unreadable target changed, so it says
    // that, in apply's words, and counts the entry as one it could not check.
    #[tokio::test]
    async fn drift_reports_an_unreadable_target_rather_than_calling_it_changed() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        package_targeting(&dirs, "V1", &target);
        collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "V1");
        let Some(guard) = make_unreadable(&target) else {
            skip("drift_reports_an_unreadable_target_rather_than_calling_it_changed");
            return;
        };

        let drift = collect_events(dirs.service().check_drift().await).await;
        assert_eq!(count_of(&drift, drifted), 0, "not drift: {drift:?}");
        let from_drift = the_unreadable_warning(&drift, &target);
        // Refused and not examined: a green summary, or a total that `sync
        // status` renders as deployed, would claim more than was checked.
        assert_eq!(drift_summary(&drift), (0, 0, 1));

        let apply = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;
        let from_apply = the_unreadable_warning(&apply, &target);
        assert_eq!(from_drift, from_apply);

        drop(guard);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "V1");
    }

    // A4. A link whose destination cannot be read is a symlinked target first,
    // in both commands: that is the refusal every command already shares.
    #[tokio::test]
    async fn drift_and_apply_refuse_a_symlink_to_an_unreadable_file_as_a_symlink() {
        let dirs = TestDirs::new();
        let destination = dirs.target_dir.join("destination");
        std::fs::write(&destination, "EXISTING").unwrap();
        let target = dirs.target_dir.join("config.toml");
        std::os::unix::fs::symlink(&destination, &target).unwrap();
        package_targeting(&dirs, "FROM REPO", &target);
        let Some(guard) = make_unreadable(&destination) else {
            skip("drift_and_apply_refuse_a_symlink_to_an_unreadable_file_as_a_symlink");
            return;
        };

        let drift = warning_messages(&collect_events(dirs.service().check_drift().await).await);
        let apply = warning_messages(
            &collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await,
        );

        assert_eq!(drift.len(), 1, "drift: {drift:?}");
        assert_eq!(apply.len(), 1, "apply: {apply:?}");
        assert_eq!(drift[0], apply[0]);
        assert!(
            apply[0].contains("is a symlink") && !apply[0].contains("could not be read"),
            "a symlinked target is refused as a symlink: {}",
            apply[0]
        );

        drop(guard);
        assert_eq!(std::fs::read_to_string(&destination).unwrap(), "EXISTING");
    }

    // A5. Not UTF-8 is not unreadable. The bytes were read, they differ, and
    // that is an ordinary conflict for the user to settle.
    #[tokio::test]
    async fn a_non_utf8_target_is_a_conflict_not_an_unreadable_one() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        let binary = [0xff_u8, 0xfe, 0x00, 0x41];
        std::fs::write(&target, binary).unwrap();
        package_targeting(&dirs, "FROM REPO", &target);

        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        assert_eq!(count_of(&events, conflicted), 1, "got: {events:?}");
        assert!(
            !warning_messages(&events)
                .iter()
                .any(|w| w.contains("could not be read")),
            "the target was read: {events:?}"
        );
        // The diff shows the target's bytes, decoded lossily, as removed lines:
        // it is built from the bytes the checksum read, not from a second read
        // that defaults to empty and renders the whole file as an addition.
        let diff = events
            .iter()
            .find_map(|e| match e {
                PackageEvent::DotfileConflict { diff, .. } => Some(diff.as_str()),
                _ => None,
            })
            .expect("a conflict event");
        assert!(
            diff.lines()
                .any(|line| line.starts_with('-') && !line.starts_with("---")),
            "the diff must show what the target holds: {diff}"
        );
        assert_eq!(std::fs::read(&target).unwrap(), binary);
    }
}

// A dry run writes nothing, so a repository-file conflict there is reported
// with its diff rather than put to the interactive resolver.
mod dry_run_conflicts {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // The conflict prompt would ask the user to decide an overwrite that will
    // not happen. The conflict is reported with its diff instead, which is what
    // a real run would put to the resolver.
    #[tokio::test]
    async fn a_dry_run_never_asks_the_resolver_about_a_repo_file_conflict() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        std::fs::write(&target, "EXISTING").unwrap();
        std::fs::create_dir_all(dirs.package_dir.join("myapp")).unwrap();
        std::fs::write(dirs.package_dir.join("myapp/config.toml"), "FROM REPO").unwrap();
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );

        let asked = Arc::new(AtomicUsize::new(0));
        let options = ApplyOptions {
            dry_run: true,
            conflict_resolver: Some(Arc::new(Counting(Arc::clone(&asked)))),
            ..Default::default()
        };
        let events = collect_events(dirs.service().apply_all(options).await).await;

        assert_eq!(
            asked.load(Ordering::SeqCst),
            0,
            "a dry run prompts for nothing"
        );
        let conflicts: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                PackageEvent::DotfileConflict { diff, .. } => Some(diff.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(conflicts.len(), 1, "got: {events:?}");
        assert!(
            conflicts[0].contains("+FROM REPO"),
            "the conflict carries the diff a real run would show: {}",
            conflicts[0]
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, PackageEvent::DotfileDeployed { .. }))
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "EXISTING");
    }
}

// ── DotfileService::list ────────────────────────────────────────────────────
//
// The listing exists so both adapters stop reading the repositories themselves.
// What matters at this level is what reaches the stream, because that is all an
// adapter has: the entries, and every spec selfie could not read.

#[tokio::test]
async fn list_returns_entries_from_both_directories() {
    let dirs = TestDirs::new();
    write_package_yaml(
        &dirs.package_dir,
        "bat",
        "name: bat\nenvironments:\n  test:\n    install: \"true\"\ndotfiles:\n  - source: \
         bat.conf\n    target: ~/.config/bat/config\n",
    );
    write_package_yaml(
        &dirs.dotfiles_dir,
        "fish",
        "name: fish\ndotfiles:\n  - source: config.fish\n    target: ~/.config/fish/config.fish\n",
    );

    let events = collect_events(dirs.service_with_dotfiles().list().await).await;

    let listed = events
        .iter()
        .find_map(|e| match e {
            PackageEvent::DotfileListLoaded { dotfile_list, .. } => Some(dotfile_list),
            _ => None,
        })
        .expect("the listing must be emitted");

    let mut names: Vec<&str> = listed
        .packages
        .iter()
        .map(selfie::package::Package::name)
        .collect();
    names.sort_unstable();
    assert_eq!(names, vec!["bat", "fish"]);

    // Each package carries where it was read from, so an adapter labels a row
    // from the package rather than from whichever loop it is standing in.
    let origins: Vec<_> = listed
        .packages
        .iter()
        .map(selfie::package::Package::origin)
        .collect();
    assert!(origins.contains(&SpecOrigin::PackageDirectory));
    assert!(origins.contains(&SpecOrigin::DotfilesDirectory));

    assert!(matches!(
        get_operation_result(&events),
        Some(OperationResult::Success(_))
    ));
}

// A spec that did not parse leaves as a typed event, and the rest of the
// listing still arrives. Reporting it must not cost the caller the packages it
// could read.
#[tokio::test]
async fn list_reports_a_spec_it_could_not_read() {
    let dirs = TestDirs::new();
    write_package_yaml(
        &dirs.package_dir,
        "bat",
        "name: bat\nenvironments:\n  test:\n    install: \"true\"\ndotfiles:\n  - source: \
         bat.conf\n    target: ~/.config/bat/config\n",
    );
    write_package_yaml(&dirs.package_dir, "brokenpkg", "{{{\n");

    let events = collect_events(dirs.service_with_dotfiles().list().await).await;

    let skipped: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            PackageEvent::SpecSkipped { error, .. } => Some(error),
            _ => None,
        })
        .collect();

    assert_eq!(skipped.len(), 1, "the unreadable spec must be reported");
    assert!(
        skipped[0].package_path().ends_with("brokenpkg.yml"),
        "got: {}",
        skipped[0].package_path().display()
    );

    let listed = events
        .iter()
        .find_map(|e| match e {
            PackageEvent::DotfileListLoaded { dotfile_list, .. } => Some(dotfile_list),
            _ => None,
        })
        .expect("the listing must still arrive");
    assert_eq!(listed.packages.len(), 1);
    assert_eq!(listed.packages[0].name(), "bat");
}

// The control for the two above: a package declaring no dotfiles is not a row,
// and a clean directory reports nothing skipped. Without this, a listing that
// returned everything or reported everything would satisfy both.
#[tokio::test]
async fn list_omits_packages_with_no_dotfiles_and_reports_nothing_skipped() {
    let dirs = TestDirs::new();
    write_package_yaml(
        &dirs.package_dir,
        "ripgrep",
        "name: ripgrep\nenvironments:\n  test:\n    install: \"true\"\n",
    );

    let events = collect_events(dirs.service_with_dotfiles().list().await).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, PackageEvent::SpecSkipped { .. })),
        "a readable directory has nothing to skip"
    );

    let listed = events
        .iter()
        .find_map(|e| match e {
            PackageEvent::DotfileListLoaded { dotfile_list, .. } => Some(dotfile_list),
            _ => None,
        })
        .expect("the listing must be emitted even when empty");
    assert!(listed.packages.is_empty());
}

// A listing that fails outright is a failure, not an empty answer. Reporting it
// as success with no entries is indistinguishable, to a caller with no stderr,
// from a directory that genuinely holds no dotfiles.
#[tokio::test]
async fn list_reports_a_listing_it_could_not_perform() {
    let dirs = TestDirs::new();
    // The directory the service was pointed at is gone by the time it looks.
    std::fs::remove_dir_all(&dirs.package_dir).unwrap();

    let events = collect_events(dirs.service_with_dotfiles().list().await).await;

    match get_operation_result(&events) {
        Some(OperationResult::Failure(failure)) => {
            let rendered = failure.to_string();
            assert!(
                rendered.contains("packages"),
                "the failure must name the directory, got: {rendered}"
            );
        }
        other => panic!("a failed listing must not report success, got: {other:?}"),
    }

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, PackageEvent::DotfileListLoaded { .. })),
        "no listing should be emitted when the listing failed"
    );
}

// Test directories whose dotfiles directory is mode 0o000 until this is
// dropped. Restoring in `Drop` runs before the `TestDirs` field is dropped, so
// the temp directory can still be removed when an assertion panics first.
struct UnlistableDotfilesDirectory(TestDirs);

impl std::ops::Deref for UnlistableDotfilesDirectory {
    type Target = TestDirs;

    fn deref(&self) -> &TestDirs {
        &self.0
    }
}

impl Drop for UnlistableDotfilesDirectory {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt as _;
        // Ignored on failure: panicking in `Drop` during an unwind aborts the
        // test binary, which would hide the assertion that started the unwind.
        let _ =
            std::fs::set_permissions(&self.0.dotfiles_dir, std::fs::Permissions::from_mode(0o755));
    }
}

// A package with one deployable dotfile, and a dotfiles directory that exists
// and cannot be listed. `None` when running as root, which ignores the mode bits.
fn dirs_with_an_unlistable_dotfiles_directory() -> Option<(UnlistableDotfilesDirectory, PathBuf)> {
    use std::os::unix::fs::PermissionsExt as _;

    let dirs = TestDirs::new();
    std::fs::write(dirs.package_dir.join("bat.conf"), "theme = dark").unwrap();
    let target = dirs.target_dir.join("bat.conf");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "bat",
        &[("bat.conf", target.to_str().unwrap())],
    );
    std::fs::set_permissions(&dirs.dotfiles_dir, std::fs::Permissions::from_mode(0o000)).unwrap();
    let dirs = UnlistableDotfilesDirectory(dirs);

    if std::fs::read_dir(&dirs.dotfiles_dir).is_ok() {
        return None;
    }
    Some((dirs, target))
}

// The regression guard for ADR-0005 decision 2: a directory selfie could not read is
// refused whether or not the user configured the path. Configuredness decides whether
// an **absence** is worth a word, and nothing else.
//
// The guard exists because the refusal and the absence warning are decided a few
// lines apart from the same fact. A change routing the refusal through the
// configured-or-not rule would make an unconfigured unlistable directory report
// success having read nothing, and nothing else in the suite would notice. Both
// halves are asserted in one test on purpose: what has to hold is that the two
// answers are the same.
#[tokio::test]
async fn an_unlistable_dotfiles_directory_is_refused_whether_or_not_it_is_configured() {
    let Some((dirs, _target)) = dirs_with_an_unlistable_dotfiles_directory() else {
        eprintln!(
            "SKIP an_unlistable_dotfiles_directory_is_refused_whether_or_not_it_is_configured"
        );
        return;
    };

    let configured = collect_events(
        dirs.service_with_dotfiles()
            .apply_all(ApplyOptions::default())
            .await,
    )
    .await;
    let unconfigured = collect_events(
        dirs.service_with_default_dotfiles()
            .apply_all(ApplyOptions::default())
            .await,
    )
    .await;

    assert_eq!(
        refused_count(&configured),
        1,
        "a configured unlistable directory must refuse: {configured:?}"
    );
    assert_eq!(
        refused_count(&unconfigured),
        refused_count(&configured),
        "the refusal must not depend on whether the path was configured: {unconfigured:?}"
    );
}

// The other half of the rule, and the reason it is not simply "always warn": an
// absent default is the ordinary state of anyone who keeps no standalone dotfiles,
// so it is silent, while an absent configured path is a mistake and is named.
#[tokio::test]
async fn an_absent_dotfiles_directory_is_reported_only_when_it_was_configured() {
    let dirs = TestDirs::new();
    std::fs::write(dirs.package_dir.join("bat.conf"), "theme = dark").unwrap();
    let target = dirs.target_dir.join("bat.conf");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "bat",
        &[("bat.conf", target.to_str().unwrap())],
    );
    std::fs::remove_dir_all(&dirs.dotfiles_dir).unwrap();

    let configured = collect_events(
        dirs.service_with_dotfiles()
            .apply_all(ApplyOptions::default())
            .await,
    )
    .await;
    let unconfigured = collect_events(
        dirs.service_with_default_dotfiles()
            .apply_all(ApplyOptions::default())
            .await,
    )
    .await;

    assert_eq!(
        dotfiles_directory_warnings(&configured).len(),
        1,
        "a configured path that is not there is a mistake worth naming: {configured:?}"
    );
    assert!(
        dotfiles_directory_warnings(&unconfigured).is_empty(),
        "an absent default is the ordinary case and must stay silent: {unconfigured:?}"
    );
    assert_eq!(
        refused_count(&configured),
        0,
        "an absence holds nothing, so there is nothing to refuse: {configured:?}"
    );
    assert_eq!(refused_count(&unconfigured), 0, "events: {unconfigured:?}");
}

// Every standalone dotfile in an unlistable directory was asked for and none can
// deploy, so the run carries a refusal. The package dotfile still deploys, which
// is what keeps this a refusal rather than a failure.
#[tokio::test]
async fn apply_all_counts_an_unlistable_dotfiles_directory_as_a_refusal() {
    let Some((dirs, target)) = dirs_with_an_unlistable_dotfiles_directory() else {
        eprintln!("SKIP apply_all_counts_an_unlistable_dotfiles_directory_as_a_refusal");
        return;
    };

    let events = collect_events(
        dirs.service_with_dotfiles()
            .apply_all(ApplyOptions::default())
            .await,
    )
    .await;

    assert_eq!(refused_count(&events), 1, "events: {events:?}");
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "theme = dark",
        "the package dotfile must still deploy"
    );
}

// A named apply that matches a package outside the directory lost nothing to it,
// so the run is not refused.
#[tokio::test]
async fn apply_by_name_is_not_refused_over_an_unlistable_dotfiles_directory() {
    let Some((dirs, target)) = dirs_with_an_unlistable_dotfiles_directory() else {
        eprintln!("SKIP apply_by_name_is_not_refused_over_an_unlistable_dotfiles_directory");
        return;
    };

    let events = collect_events(
        dirs.service_with_dotfiles()
            .apply("bat", ApplyOptions::default())
            .await,
    )
    .await;

    assert_eq!(refused_count(&events), 0, "events: {events:?}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "theme = dark");
}

// A dangling symlink at the configured dotfiles directory. The user-visible half of
// the same defect: apply reported "does not exist" and offered `mkdir -p`, which
// fails with "No such file or directory" against a path where a link already sits.
//
// Asserts the sentence and the hint's absence rather than the warning's presence,
// because the merge-base binary also warns here and also carries on.
#[tokio::test]
async fn apply_all_names_a_dangling_symlink_at_the_dotfiles_directory() {
    let dirs = TestDirs::new();
    std::fs::write(dirs.package_dir.join("bat.conf"), "theme = dark").unwrap();
    let target = dirs.target_dir.join("bat.conf");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "bat",
        &[("bat.conf", target.to_str().unwrap())],
    );
    let service = dirs.service_with_dotfiles();
    std::fs::remove_dir_all(&dirs.dotfiles_dir).unwrap();
    let destination = dirs.dotfiles_dir.parent().unwrap().join("moved-away");
    std::os::unix::fs::symlink(&destination, &dirs.dotfiles_dir).unwrap();

    let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

    assert_eq!(refused_count(&events), 0, "events: {events:?}");
    let warnings = dotfiles_directory_warnings(&events);
    assert_eq!(warnings.len(), 1, "events: {events:?}");
    assert!(
        warnings[0].contains("is a symlink to nothing"),
        "got: {}",
        warnings[0]
    );
    assert!(
        warnings[0].contains(&destination.display().to_string()),
        "the destination the user has to fix must be named, got: {}",
        warnings[0]
    );
    assert!(
        !warnings[0].contains("mkdir"),
        "mkdir -p cannot create a path a link already occupies, got: {}",
        warnings[0]
    );
    assert!(
        !warnings[0].contains("does not exist"),
        "the path exists; its destination does not, got: {}",
        warnings[0]
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "theme = dark");
}

// A plain file at the configured dotfiles directory. `mkdir -p` fails with "File
// exists" here, so the sentence names what is there instead of offering it.
#[tokio::test]
async fn apply_all_names_a_plain_file_at_the_dotfiles_directory() {
    let dirs = TestDirs::new();
    std::fs::write(dirs.package_dir.join("bat.conf"), "theme = dark").unwrap();
    let target = dirs.target_dir.join("bat.conf");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "bat",
        &[("bat.conf", target.to_str().unwrap())],
    );
    let service = dirs.service_with_dotfiles();
    std::fs::remove_dir_all(&dirs.dotfiles_dir).unwrap();
    std::fs::write(&dirs.dotfiles_dir, "not a directory").unwrap();

    let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

    assert_eq!(refused_count(&events), 0, "events: {events:?}");
    let warnings = dotfiles_directory_warnings(&events);
    assert_eq!(warnings.len(), 1, "events: {events:?}");
    assert!(
        warnings[0].contains("is not a directory, it is a regular file"),
        "got: {}",
        warnings[0]
    );
    assert!(
        !warnings[0].contains("mkdir"),
        "mkdir -p fails with \"File exists\" here, got: {}",
        warnings[0]
    );
}

// A configured dotfiles directory that is not there is reported and carries on:
// it holds nothing, so there is nothing to refuse.
#[tokio::test]
async fn apply_all_is_not_refused_when_the_dotfiles_directory_is_gone() {
    let dirs = TestDirs::new();
    std::fs::write(dirs.package_dir.join("bat.conf"), "theme = dark").unwrap();
    let target = dirs.target_dir.join("bat.conf");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "bat",
        &[("bat.conf", target.to_str().unwrap())],
    );
    let service = dirs.service_with_dotfiles();
    std::fs::remove_dir_all(&dirs.dotfiles_dir).unwrap();

    let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

    assert_eq!(refused_count(&events), 0, "events: {events:?}");
    let warnings = dotfiles_directory_warnings(&events);
    assert_eq!(warnings.len(), 1, "events: {events:?}");
    assert!(
        warnings[0].starts_with("Dotfiles directory ")
            && warnings[0].contains(&format!("{} does not exist", dirs.dotfiles_dir.display()))
            && warnings[0].contains("standalone dotfiles will not be read"),
        "got: {}",
        warnings[0]
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "theme = dark");
}

#[tokio::test]
async fn drift_counts_an_unlistable_dotfiles_directory_as_a_refusal() {
    let Some((dirs, _target)) = dirs_with_an_unlistable_dotfiles_directory() else {
        eprintln!("SKIP drift_counts_an_unlistable_dotfiles_directory_as_a_refusal");
        return;
    };

    let events = collect_events(dirs.service_with_dotfiles().check_drift().await).await;

    match get_operation_result(&events) {
        Some(OperationResult::Success(OperationSuccess::DotfileDriftChecked {
            refused_count,
            total_count,
            ..
        })) => {
            assert_eq!(*refused_count, 1, "events: {events:?}");
            assert_eq!(
                *total_count, 1,
                "the package dotfile must still be checked; events: {events:?}"
            );
        }
        other => panic!("expected a drift check, got: {other:?}"),
    }
}

// A dotfiles directory that exists and cannot be listed is fatal to a LISTING,
// because the table would be missing every standalone entry. Apply and drift
// have the package dotfiles to act on, so they count it as a refusal instead.
#[tokio::test]
async fn list_fails_when_a_dotfiles_directory_cannot_be_listed() {
    let Some((dirs, _target)) = dirs_with_an_unlistable_dotfiles_directory() else {
        eprintln!("SKIP list_fails_when_a_dotfiles_directory_cannot_be_listed: still readable");
        return;
    };

    let events = collect_events(dirs.service_with_dotfiles().list().await).await;

    assert!(
        matches!(
            get_operation_result(&events),
            Some(OperationResult::Failure(_))
        ),
        "an unlistable directory must not come back as a successful listing"
    );
}

// A configured dotfiles directory that is not there is reported, and the listing
// carries on past it. Only one that exists and cannot be listed fails a listing.
#[tokio::test]
async fn list_carries_on_when_the_dotfiles_directory_is_gone() {
    let dirs = TestDirs::new();
    let service = dirs.service_with_dotfiles();
    std::fs::remove_dir_all(&dirs.dotfiles_dir).unwrap();

    let events = collect_events(service.list().await).await;

    assert!(
        matches!(
            get_operation_result(&events),
            Some(OperationResult::Success(_))
        ),
        "events: {events:?}"
    );
    let warnings = dotfiles_directory_warnings(&events);
    assert_eq!(warnings.len(), 1, "events: {events:?}");
    assert!(
        warnings[0].starts_with("Dotfiles directory ")
            && warnings[0].contains(&format!("{} does not exist", dirs.dotfiles_dir.display()))
            && warnings[0].contains("standalone dotfiles will not be read"),
        "got: {}",
        warnings[0]
    );
}

#[tokio::test]
async fn drift_reports_a_configured_dotfiles_directory_that_is_gone_without_refusing() {
    let dirs = TestDirs::new();
    let service = dirs.service_with_dotfiles();
    std::fs::remove_dir_all(&dirs.dotfiles_dir).unwrap();

    let events = collect_events(service.check_drift().await).await;

    match get_operation_result(&events) {
        Some(OperationResult::Success(OperationSuccess::DotfileDriftChecked {
            refused_count,
            ..
        })) => assert_eq!(*refused_count, 0, "events: {events:?}"),
        other => panic!("expected a drift check, got: {other:?}"),
    }
    let warnings = dotfiles_directory_warnings(&events);
    assert_eq!(warnings.len(), 1, "events: {events:?}");
    assert!(
        warnings[0].contains(&format!("{} does not exist", dirs.dotfiles_dir.display())),
        "got: {}",
        warnings[0]
    );
}

// An unset default that is not there is the ordinary state of a setup with no
// standalone dotfiles. Saying so on every run would be noise people learn to
// ignore.
#[tokio::test]
async fn apply_all_says_nothing_about_an_unset_default_that_is_not_there() {
    let dirs = TestDirs::new();
    std::fs::write(dirs.package_dir.join("bat.conf"), "theme = dark").unwrap();
    let target = dirs.target_dir.join("bat.conf");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "bat",
        &[("bat.conf", target.to_str().unwrap())],
    );
    std::fs::remove_dir_all(&dirs.dotfiles_dir).unwrap();

    let events = collect_events(
        dirs.service_with_default_dotfiles()
            .apply_all(ApplyOptions::default())
            .await,
    )
    .await;

    assert_eq!(refused_count(&events), 0, "events: {events:?}");
    assert!(
        dotfiles_directory_warnings(&events).is_empty(),
        "events: {events:?}"
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "theme = dark");
}

#[tokio::test]
async fn drift_says_nothing_about_an_unset_default_that_is_not_there() {
    let dirs = TestDirs::new();
    std::fs::remove_dir_all(&dirs.dotfiles_dir).unwrap();

    let events = collect_events(dirs.service_with_default_dotfiles().check_drift().await).await;

    match get_operation_result(&events) {
        Some(OperationResult::Success(OperationSuccess::DotfileDriftChecked {
            refused_count,
            ..
        })) => assert_eq!(*refused_count, 0, "events: {events:?}"),
        other => panic!("expected a drift check, got: {other:?}"),
    }
    assert!(
        dotfiles_directory_warnings(&events).is_empty(),
        "events: {events:?}"
    );
}

#[tokio::test]
async fn list_says_nothing_about_an_unset_default_that_is_not_there() {
    let dirs = TestDirs::new();
    std::fs::remove_dir_all(&dirs.dotfiles_dir).unwrap();

    let events = collect_events(dirs.service_with_default_dotfiles().list().await).await;

    assert!(
        matches!(
            get_operation_result(&events),
            Some(OperationResult::Success(_))
        ),
        "events: {events:?}"
    );
    assert!(
        dotfiles_directory_warnings(&events).is_empty(),
        "events: {events:?}"
    );
}

// A key that shadows `dotfiles:` leaves selfie reading an EMPTY list from a file
// that declares entries. Counting that as "this package has no dotfiles" is what
// sends a user looking for a dotfile the listing says does not exist, so the
// package is reported as refused rather than listed as bare.
#[tokio::test]
async fn list_reports_a_package_whose_dotfiles_key_is_shadowed() {
    let dirs = TestDirs::new();
    write_package_yaml(
        &dirs.package_dir,
        "bat",
        "name: bat\nenvironments:\n  test:\n    install: \"true\"\ndotfiles:\n  - source: \
         bat.conf\n    target: ~/.config/bat/config\n",
    );
    // `_dotfiles` is an anchor whose name collides with the real field, so the
    // list selfie reads is empty while the file declares an entry. A raw string
    // because YAML is indentation-sensitive and a continuation would eat it.
    write_package_yaml(
        &dirs.package_dir,
        "shadowed",
        r#"name: shadowed
_dotfiles: &d
  - source: a
    target: ~/.a
environments:
  test:
    install: "true"
"#,
    );

    let events = collect_events(dirs.service_with_dotfiles().list().await).await;

    let listed = events
        .iter()
        .find_map(|e| match e {
            PackageEvent::DotfileListLoaded { dotfile_list, .. } => Some(dotfile_list),
            _ => None,
        })
        .expect("the listing must be emitted");

    assert_eq!(
        listed.refused.len(),
        1,
        "the shadowed package must be refused"
    );
    assert_eq!(listed.refused[0].package_name, "shadowed");
    assert!(
        listed.refused[0].reason.contains("_dotfiles"),
        "the reason must name the key, got: {}",
        listed.refused[0].reason
    );

    // The control: the readable package is still listed. Refusing one must not
    // cost the caller the rest.
    assert_eq!(listed.packages.len(), 1);
    assert_eq!(listed.packages[0].name(), "bat");
}

// A name in both directories is two files, and a listing is asked what is on
// disk rather than which one would deploy. `collect_all_packages` drops the
// dotfiles/ copy for exactly that deploy question, so the listing must not reuse
// that answer.
#[tokio::test]
async fn list_keeps_both_copies_of_a_name_in_both_directories() {
    let dirs = TestDirs::new();
    write_package_yaml(
        &dirs.package_dir,
        "shared",
        r#"name: shared
environments:
  test:
    install: "true"
dotfiles:
  - source: from-packages
    target: ~/.from-packages
"#,
    );
    write_package_yaml(
        &dirs.dotfiles_dir,
        "shared",
        r#"name: shared
dotfiles:
  - source: from-dotfiles
    target: ~/.from-dotfiles
"#,
    );

    let events = collect_events(dirs.service_with_dotfiles().list().await).await;

    let listed = events
        .iter()
        .find_map(|e| match e {
            PackageEvent::DotfileListLoaded { dotfile_list, .. } => Some(dotfile_list),
            _ => None,
        })
        .expect("the listing must be emitted");

    assert_eq!(
        listed.packages.len(),
        2,
        "both files exist, so both are listed; got {:?}",
        listed
            .packages
            .iter()
            .map(selfie::package::Package::name)
            .collect::<Vec<_>>()
    );

    let origins: Vec<_> = listed
        .packages
        .iter()
        .map(selfie::package::Package::origin)
        .collect();
    assert!(origins.contains(&SpecOrigin::PackageDirectory));
    assert!(origins.contains(&SpecOrigin::DotfilesDirectory));
}

// One level down from the shadowed top level, and the same harm: a key inside an
// environment mapping empties that environment's list, so the listing would show
// the package as simply having no dotfiles there. `spec validate` already errors
// on this, so a listing that stayed silent disagreed with it.
#[tokio::test]
async fn list_reports_a_package_whose_environment_key_is_shadowed() {
    let dirs = TestDirs::new();
    write_package_yaml(
        &dirs.package_dir,
        "envshadow",
        r#"name: envshadow
environments:
  test:
    install: "true"
    _dotfiles: &d
      - source: hidden
        target: ~/.hidden
"#,
    );

    let events = collect_events(dirs.service_with_dotfiles().list().await).await;

    let listed = events
        .iter()
        .find_map(|e| match e {
            PackageEvent::DotfileListLoaded { dotfile_list, .. } => Some(dotfile_list),
            _ => None,
        })
        .expect("the listing must be emitted");

    assert_eq!(
        listed.refused.len(),
        1,
        "the shadowed environment must be refused"
    );
    assert_eq!(listed.refused[0].package_name, "envshadow");
    assert!(
        listed.refused[0].reason.contains("_dotfiles"),
        "the reason must name the key, got: {}",
        listed.refused[0].reason
    );
}

// Copies of what a target held before an apply overwrote it.
//
// The invariant: an overwrite of a target whose content differs leaves a
// recoverable copy under the state directory. Everything else here bounds it —
// what is not copied, how many copies survive, and what happens when a copy
// cannot be made.
mod backups_before_overwrite {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn accepting() -> ApplyOptions {
        ApplyOptions {
            auto_accept: true,
            ..Default::default()
        }
    }

    // Every copy under the backups tree, as (path, content).
    //
    // Sorted by path, so a test reading one of several does not depend on
    // enumeration order.
    fn copies(state_dir: &std::path::Path) -> Vec<(PathBuf, String)> {
        let root = state_dir.join("backups");
        if !root.exists() {
            return Vec::new();
        }
        let mut found: Vec<(PathBuf, String)> = std::fs::read_dir(&root)
            .unwrap()
            .flat_map(|target_dir| std::fs::read_dir(target_dir.unwrap().path()).unwrap())
            .map(|entry| {
                let path = entry.unwrap().path();
                let content = std::fs::read_to_string(&path).unwrap();
                (path, content)
            })
            .collect();
        found.sort();
        found
    }

    // What each deployment reported about a kept copy, in order.
    fn reported(events: &[PackageEvent]) -> Vec<Option<String>> {
        events
            .iter()
            .filter_map(|event| match event {
                PackageEvent::DotfileDeployed { backup, .. } => Some(backup.clone()),
                _ => None,
            })
            .collect()
    }

    // A package named `name` with one repository-file dotfile pointing at
    // `target`, and `target` seeded with `target_content` when given.
    fn one_entry(
        dirs: &TestDirs,
        name: &str,
        source_content: &str,
        target: &std::path::Path,
        target_content: Option<&str>,
    ) {
        let source_dir = dirs.package_dir.join(name);
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(source_dir.join("config.toml"), source_content).unwrap();
        let source = format!("{name}/config.toml");
        create_package_with_dotfiles(
            &dirs.package_dir,
            name,
            &[(&source, target.to_str().unwrap())],
        );
        if let Some(content) = target_content {
            std::fs::write(target, content).unwrap();
        }
    }

    #[tokio::test]
    async fn an_accepted_overwrite_keeps_a_recoverable_copy_under_the_state_directory() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        one_entry(
            &dirs,
            "myapp",
            "key = \"from-repo\"",
            &target,
            Some("key = \"hand-edited\""),
        );

        let events = collect_events(dirs.service().apply_all(accepting()).await).await;

        let kept = copies(&dirs.state_dir);
        assert_eq!(kept.len(), 1, "one copy per overwritten target: {kept:?}");
        assert_eq!(
            kept[0].1, "key = \"hand-edited\"",
            "the copy must hold what the target held"
        );
        // Control: the overwrite happened, so this is not a copy of a target
        // selfie decided to leave alone.
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "key = \"from-repo\""
        );
        // The control says what an ordinary write produces in this environment.
        // Under a umask of 077 an owner-only assertion passes whatever the code
        // does -- swapping the owner-only writer for the ordinary one goes
        // unnoticed -- so without this the test could be green while proving
        // nothing.
        let control = dirs.state_dir.join("control");
        std::fs::write(&control, b"x").unwrap();
        let control_mode = std::fs::metadata(&control).unwrap().permissions().mode();
        if control_mode & 0o077 == 0 {
            let message = "the ambient umask makes ordinary writes owner-only, so this \
                           cannot tell an owner-only write from a default one";
            assert!(
                std::env::var_os("CI").is_none(),
                "an_accepted_overwrite_keeps_a_recoverable_copy_under_the_state_directory: \
                 {message}"
            );
            eprintln!(
                "SKIP an_accepted_overwrite_keeps_a_recoverable_copy_under_the_state_directory: \
                 {message}"
            );
            return;
        }

        // Owner-only, like the state file beside it. Selfie cannot know that what
        // a user had at a target was not a credential.
        //
        // `& 0o077`, not `& 0o007`: group-readable exposes it to exactly the people
        // it is being kept from on a shared machine.
        let mode = std::fs::metadata(&kept[0].0).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o077,
            0,
            "group/other bits set on the copy: {:04o}",
            mode & 0o777
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, PackageEvent::DotfileDeployed { .. })),
            "the entry must have deployed: {events:?}"
        );
    }

    #[tokio::test]
    async fn the_deployed_event_names_the_backup() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        one_entry(&dirs, "myapp", "new", &target, Some("old"));

        let events = collect_events(dirs.service().apply_all(accepting()).await).await;

        let kept = copies(&dirs.state_dir);
        assert_eq!(kept.len(), 1, "{kept:?}");
        let named = reported(&events);
        assert_eq!(named.len(), 1, "one deployment: {named:?}");
        let named = named[0].as_ref().expect("the event must name the copy");
        assert_eq!(
            std::path::Path::new(named),
            kept[0].0,
            "the event must name the file that is on disk"
        );
    }

    // The commonest overwrite of all: the repository file changed, the target is
    // untouched, and selfie refreshes it with no prompt. Nothing asks the user,
    // so nothing else would tell them their old content had gone.
    #[tokio::test]
    async fn a_silent_refresh_of_a_tracked_target_keeps_a_copy_too() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        one_entry(&dirs, "myapp", "version-one", &target, None);

        let service = dirs.service();
        let _ = collect_events(service.apply_all(ApplyOptions::default()).await).await;
        std::fs::write(dirs.package_dir.join("myapp/config.toml"), "version-two").unwrap();

        // No `auto_accept` and no resolver: this must not be a conflict.
        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        let kept = copies(&dirs.state_dir);
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(kept[0].1, "version-one");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "version-two");
        assert!(
            reported(&events).iter().all(Option::is_some),
            "the refresh must name its copy: {events:?}"
        );
    }

    #[tokio::test]
    async fn a_new_target_is_not_backed_up() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        one_entry(&dirs, "myapp", "fresh", &target, None);

        let events = collect_events(dirs.service().apply_all(accepting()).await).await;

        assert!(
            !dirs.state_dir.join("backups").exists(),
            "nothing was at the target, so there is nothing to keep"
        );
        assert_eq!(reported(&events), vec![None]);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "fresh");
    }

    // A control for the pair below it: an in-sync target is skipped, and a skip
    // writes nothing. It cannot fail on its own under a content-based rule, since
    // an in-sync target's bytes already equal the source's — which is what
    // `an_overwrite_with_identical_content_is_not_backed_up` exists to catch.
    #[tokio::test]
    async fn an_in_sync_target_is_not_backed_up() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        one_entry(&dirs, "myapp", "same", &target, Some("same"));

        let events = collect_events(dirs.service().apply_all(accepting()).await).await;

        assert!(!dirs.state_dir.join("backups").exists());
        assert!(
            events
                .iter()
                .any(|e| matches!(e, PackageEvent::DotfileSkipped { .. })),
            "an in-sync target is skipped: {events:?}"
        );
    }

    // The write happens and there is still nothing to keep, because what is at
    // the target is already what is about to be written. A rule that copied
    // whenever a target existed and the decision was not `Skip` would keep a
    // pointless copy here, and every other test in this module would pass.
    #[tokio::test]
    async fn an_overwrite_with_identical_content_is_not_backed_up() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        one_entry(&dirs, "myapp", "version-one", &target, None);

        let service = dirs.service();
        let _ = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        // Both sides move to the same new content, so the recorded checksum
        // matches neither: `BothChanged`, which is a conflict.
        std::fs::write(dirs.package_dir.join("myapp/config.toml"), "version-two").unwrap();
        std::fs::write(&target, "version-two").unwrap();

        let events = collect_events(service.apply_all(accepting()).await).await;

        // Control: the write path really ran. Content equality alone cannot show
        // that, because the target would look the same either way.
        assert_eq!(
            reported(&events),
            vec![None],
            "the entry must have deployed and kept nothing: {events:?}"
        );
        assert!(!dirs.state_dir.join("backups").exists());
    }

    // The copy an earlier run made is the only record of content that is nowhere
    // else. A later overwrite that fails must not consume it: the copy it would
    // leave behind holds what is still sitting at the target, so trading one for
    // the other loses the user's file and gains a duplicate.
    #[tokio::test]
    async fn an_earlier_copy_survives_a_failed_overwrite() {
        let dirs = TestDirs::new();
        let holder = dirs.target_dir.join("sub");
        std::fs::create_dir_all(&holder).unwrap();
        let target = holder.join("config.toml");
        one_entry(&dirs, "myapp", "v1", &target, Some("USER-ORIGINAL"));

        let service = dirs.service();
        let _ = collect_events(service.apply_all(accepting()).await).await;
        assert_eq!(
            copies(&dirs.state_dir)
                .iter()
                .map(|(_, content)| content.as_str())
                .collect::<Vec<_>>(),
            vec!["USER-ORIGINAL"],
            "the first apply must have kept the user's content"
        );

        // Both halves, because the two writers fail on different things: writing a
        // target in place needs write permission on the file, and replacing it by
        // rename needs write permission on its directory. The target stays
        // readable and the state directory stays writable either way.
        std::fs::write(dirs.package_dir.join("myapp/config.toml"), "v2").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o400)).unwrap();
        std::fs::set_permissions(&holder, std::fs::Permissions::from_mode(0o500)).unwrap();
        let events = collect_events(service.apply_all(accepting()).await).await;
        std::fs::set_permissions(&holder, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();

        // Fails rather than passes vacuously if the write did not fail -- running
        // as root, where the mode above is not enforced.
        assert_eq!(
            refused_count(&events),
            1,
            "the target write had to fail for this test to mean anything: {events:?}"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "v1");

        let after: Vec<String> = copies(&dirs.state_dir)
            .into_iter()
            .map(|(_, content)| content)
            .collect();
        assert!(
            after.iter().any(|content| content == "USER-ORIGINAL"),
            "the only copy of the user's content must survive a failed overwrite, got {after:?}"
        );
    }

    // A resolver that edits the target before accepting, standing in for a user who
    // changes the file while the prompt waits for them.
    struct EditsThenAccepts {
        target: PathBuf,
        content: &'static str,
    }

    impl selfie::dotfile_service::port::ConflictResolver for EditsThenAccepts {
        fn resolve(
            &self,
            _target: &str,
            _detail: selfie::dotfile_service::port::ConflictDetail<'_>,
        ) -> selfie::dotfile_service::port::ConflictResolution {
            std::fs::write(&self.target, self.content).unwrap();
            selfie::dotfile_service::port::ConflictResolution::Accept
        }
    }

    // The copy has to hold what the write destroys, not what the prompt showed.
    // An interactive resolver blocks for as long as the user takes, and the target
    // can change in that window -- so a copy taken from the bytes the deploy
    // decision was made on would preserve content still reachable on disk and lose
    // the content the write is about to overwrite.
    #[tokio::test]
    async fn the_copy_holds_the_bytes_the_write_destroys_not_the_ones_the_prompt_showed() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        one_entry(&dirs, "myapp", "from-repo", &target, Some("at-prompt-time"));

        let options = ApplyOptions {
            conflict_resolver: Some(std::sync::Arc::new(EditsThenAccepts {
                target: target.clone(),
                content: "edited-while-prompting",
            })),
            ..Default::default()
        };
        let events = collect_events(dirs.service().apply_all(options).await).await;

        // Control: the resolver ran and the write went ahead, so the run really did
        // reach the point where a copy is made.
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "from-repo");
        let kept = copies(&dirs.state_dir);
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(
            kept[0].1, "edited-while-prompting",
            "the copy must hold the bytes the write destroyed"
        );
        assert!(
            reported(&events).iter().all(Option::is_some),
            "the deployment must name its copy: {events:?}"
        );
    }

    // The target became unreadable while the prompt was open. Refusing leaves it
    // alone; writing would destroy content selfie cannot copy first.
    #[tokio::test]
    async fn a_target_that_becomes_unreadable_before_the_write_is_refused() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        one_entry(&dirs, "myapp", "from-repo", &target, Some("at-prompt-time"));

        // A directory at the path reads as unreadable rather than absent, and needs
        // no permission games, so it holds for root too.
        struct ReplacesWithADirectory(PathBuf);
        impl selfie::dotfile_service::port::ConflictResolver for ReplacesWithADirectory {
            fn resolve(
                &self,
                _target: &str,
                _detail: selfie::dotfile_service::port::ConflictDetail<'_>,
            ) -> selfie::dotfile_service::port::ConflictResolution {
                std::fs::remove_file(&self.0).unwrap();
                std::fs::create_dir(&self.0).unwrap();
                selfie::dotfile_service::port::ConflictResolution::Accept
            }
        }

        let options = ApplyOptions {
            conflict_resolver: Some(std::sync::Arc::new(ReplacesWithADirectory(target.clone()))),
            ..Default::default()
        };
        let events = collect_events(dirs.service().apply_all(options).await).await;

        assert_eq!(refused_count(&events), 1, "{events:?}");
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, PackageEvent::DotfileDeployed { .. })),
            "nothing may be deployed over a target selfie cannot read: {events:?}"
        );
        assert!(target.is_dir(), "the target must be left as it was found");
        assert!(!dirs.state_dir.join("backups").exists());
    }

    // A preview writes nothing, copies included. Worth pinning separately from the
    // deploy path: the run still resolves a place to put a copy, so the only thing
    // stopping one is the dry-run return, and moving the copy above it would make
    // `--dry-run` write to the state directory.
    //
    // The fixture is a TRACKED entry whose source then changed, which is the one
    // shape that reaches the writer in a dry run with content at the target worth
    // copying. A conflicting target does not: a dry run accepts nothing, so it is
    // reported as a conflict and never reaches `perform_deploy` at all, and a test
    // built on one would pass without exercising the return it is named for.
    #[tokio::test]
    async fn a_dry_run_keeps_no_copy() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        one_entry(&dirs, "myapp", "v1", &target, None);

        // Deploy it for real, so the entry is tracked and the target holds content
        // an overwrite would otherwise copy aside.
        let _ = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "v1");

        std::fs::write(dirs.package_dir.join("myapp").join("config.toml"), "v2").unwrap();

        let options = ApplyOptions {
            dry_run: true,
            ..Default::default()
        };
        let events = collect_events(dirs.service().apply_all(options).await).await;

        assert!(
            !dirs.state_dir.join("backups").exists(),
            "a dry run must not write into the state directory"
        );
        // Control: the entry reached the writer and returned there, so the run did
        // not pass this test by skipping the entry for an unrelated reason.
        assert!(
            events.iter().any(
                |e| matches!(e, PackageEvent::DotfileSkipped { reason, .. } if reason == "dry run")
            ),
            "the entry must have reached the dry-run return in the writer: {events:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "v1",
            "a dry run must not write"
        );
    }

    #[tokio::test]
    async fn the_second_overwrite_replaces_the_first_backup() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        one_entry(&dirs, "myapp", "version-one", &target, None);
        let service = dirs.service();

        let _ = collect_events(service.apply_all(ApplyOptions::default()).await).await;
        std::fs::write(dirs.package_dir.join("myapp/config.toml"), "version-two").unwrap();
        let _ = collect_events(service.apply_all(ApplyOptions::default()).await).await;
        std::fs::write(dirs.package_dir.join("myapp/config.toml"), "version-three").unwrap();
        let _ = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        let kept = copies(&dirs.state_dir);
        assert_eq!(
            kept.len(),
            1,
            "only the most recent copy survives: {kept:?}"
        );
        assert_eq!(
            kept[0].1, "version-two",
            "the surviving copy is the one the last overwrite displaced"
        );
    }

    // Proceeding would overwrite the target with no way back, which is the one
    // outcome this whole module exists to prevent. A regular file where the
    // backups directory has to go is the failure: `create_dir_all` cannot pass
    // it, and unlike a permission fixture it also fails for root.
    #[tokio::test]
    async fn a_backup_that_cannot_be_written_refuses_the_overwrite() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        one_entry(&dirs, "myapp", "from-repo", &target, Some("hand-edited"));
        std::fs::write(dirs.state_dir.join("backups"), "not a directory").unwrap();

        let events = collect_events(dirs.service().apply_all(accepting()).await).await;

        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "hand-edited",
            "the target must be left exactly as it was"
        );
        assert_eq!(refused_count(&events), 1);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, PackageEvent::DotfileDeployed { .. })),
            "nothing was deployed: {events:?}"
        );
        let warning = warning_messages(&events)
            .into_iter()
            .find(|message| message.contains("cannot keep a copy"))
            .expect("the refusal must be reported");
        assert!(
            warning.contains(target.to_str().unwrap())
                && warning.contains("--state-directory")
                && warning.contains("The target is unchanged"),
            "{warning}"
        );
        assert!(
            !warning.contains("Failed to write"),
            "a copy that could not be made is not a failed target write: {warning}"
        );
    }

    #[tokio::test]
    async fn two_targets_with_the_same_basename_get_separate_backups() {
        let dirs = TestDirs::new();
        let first = dirs.target_dir.join("a/.npmrc");
        let second = dirs.target_dir.join("b/.npmrc");
        std::fs::create_dir_all(first.parent().unwrap()).unwrap();
        std::fs::create_dir_all(second.parent().unwrap()).unwrap();
        std::fs::write(&first, "registry-a").unwrap();
        std::fs::write(&second, "registry-b").unwrap();

        let source_dir = dirs.package_dir.join("npm");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(source_dir.join("a.npmrc"), "managed-a").unwrap();
        std::fs::write(source_dir.join("b.npmrc"), "managed-b").unwrap();
        create_package_with_dotfiles(
            &dirs.package_dir,
            "npm",
            &[
                ("npm/a.npmrc", first.to_str().unwrap()),
                ("npm/b.npmrc", second.to_str().unwrap()),
            ],
        );

        let _ = collect_events(dirs.service().apply_all(accepting()).await).await;

        let kept = copies(&dirs.state_dir);
        assert_eq!(kept.len(), 2, "one copy per target: {kept:?}");
        let mut held: Vec<&str> = kept.iter().map(|(_, content)| content.as_str()).collect();
        held.sort_unstable();
        assert_eq!(held, vec!["registry-a", "registry-b"]);
        let directories: std::collections::HashSet<&std::path::Path> = kept
            .iter()
            .map(|(path, _)| path.parent().unwrap())
            .collect();
        assert_eq!(
            directories.len(),
            2,
            "two targets sharing a file name must not share a directory: {kept:?}"
        );
    }

    // Two packages may name one target and nothing refuses it. Copying again for
    // the second entry would replace the user's content with the first entry's
    // output and then delete the user's copy, so the run would destroy the only
    // thing worth keeping.
    #[tokio::test]
    async fn two_entries_for_one_target_keep_the_copy_made_before_the_run_wrote() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("shared.toml");
        one_entry(&dirs, "first", "from-first", &target, Some("hand-edited"));
        one_entry(&dirs, "second", "from-second", &target, None);

        let events = collect_events(dirs.service().apply_all(accepting()).await).await;

        let kept = copies(&dirs.state_dir);
        assert_eq!(kept.len(), 1, "one copy per target per run: {kept:?}");
        assert_eq!(
            kept[0].1, "hand-edited",
            "the copy must hold what the target held before the run, whichever \
             package the apply reached first"
        );
        let named = reported(&events);
        assert_eq!(named.len(), 2, "both entries deployed: {events:?}");
        assert_eq!(
            named[0], named[1],
            "both deployments must name the one copy: {named:?}"
        );
        assert_eq!(
            std::path::Path::new(named[0].as_ref().expect("a copy was kept")),
            kept[0].0
        );
    }

    // The run created the target, so the honest answer is that nothing was kept.
    // Copying for the second entry would keep the first entry's own output and
    // call it the previous content.
    #[tokio::test]
    async fn a_target_this_run_created_is_not_backed_up_by_a_later_entry() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("shared.toml");
        one_entry(&dirs, "first", "from-first", &target, None);
        one_entry(&dirs, "second", "from-second", &target, None);

        let events = collect_events(dirs.service().apply_all(accepting()).await).await;

        assert!(
            !dirs.state_dir.join("backups").exists(),
            "the target did not exist when the run started"
        );
        let named = reported(&events);
        assert_eq!(named.len(), 2, "both entries deployed: {events:?}");
        assert!(named.iter().all(Option::is_none), "{named:?}");
    }
}

// `selfie dotfiles track` and `selfie package track-dotfile` run one function,
// so neither can refuse an input the other accepts, nor describe the same
// refusal differently. Each test here drives both entry points over one fixture
// and compares the rendered failure, which is the only thing that catches a
// wording forked back into a per-entry-point copy.
//
// Each also asserts what the shared message says. Comparing two strings for
// equality alone passes when both runs failed for some unrelated reason, which
// is how a parity test goes vacuous.
mod track_entry_points_agree {
    use super::*;

    // A package with no dotfile entries, so `track_for_package` reaches the same
    // checks a brand-new standalone spec does.
    fn fixture() -> TestDirs {
        let dirs = TestDirs::new();
        create_package_with_dotfiles(&dirs.package_dir, "bat", &[]);
        dirs
    }

    // Both entry points' rendered failure for `target`, standalone first.
    async fn both_failures(dirs: &TestDirs, target: &str) -> (String, String) {
        let standalone = collect_events(
            dirs.service_with_dotfiles()
                .track_standalone("fresh", target)
                .await,
        )
        .await;
        let for_package = collect_events(
            dirs.service_with_dotfiles()
                .track_for_package("bat", target)
                .await,
        )
        .await;
        (failure_message(&standalone), failure_message(&for_package))
    }

    #[tokio::test]
    async fn a_relative_target_is_refused_the_same_way() {
        let dirs = fixture();

        let (standalone, for_package) = both_failures(&dirs, "relative/config.toml").await;

        assert_eq!(standalone, for_package, "the two entry points disagree");
        assert!(
            standalone.contains("not absolute"),
            "not the target rule's refusal: {standalone}"
        );
    }

    #[tokio::test]
    async fn a_named_user_target_is_refused_the_same_way() {
        let dirs = fixture();

        let (standalone, for_package) = both_failures(&dirs, "~alice/.gemrc").await;

        assert_eq!(standalone, for_package, "the two entry points disagree");
        assert!(
            standalone.contains("~user"),
            "not the named-user refusal: {standalone}"
        );
    }

    #[tokio::test]
    async fn a_missing_target_is_refused_the_same_way() {
        let dirs = fixture();
        let absent = dirs.target_dir.join("not-there.toml");

        let (standalone, for_package) = both_failures(&dirs, absent.to_str().unwrap()).await;

        assert_eq!(standalone, for_package, "the two entry points disagree");
        assert!(
            standalone.contains("does not exist"),
            "not the missing-target refusal: {standalone}"
        );
    }

    #[tokio::test]
    async fn a_symlinked_target_is_refused_the_same_way() {
        let dirs = fixture();
        let destination = dirs.target_dir.join("real.toml");
        std::fs::write(&destination, "key = 1").unwrap();
        let link = dirs.target_dir.join("link.toml");
        std::os::unix::fs::symlink(&destination, &link).unwrap();

        let (standalone, for_package) = both_failures(&dirs, link.to_str().unwrap()).await;

        assert_eq!(standalone, for_package, "the two entry points disagree");
        assert!(
            standalone.contains("symlink"),
            "not the symlink refusal: {standalone}"
        );
    }

    // The collision remedy has to name something both entry points can do. A spec name
    // is a lever only `dotfiles track` has -- `package track-dotfile` takes the name
    // from its package argument and the basename from the target -- so advice to choose
    // a different name describes nothing at that call site.
    #[tokio::test]
    async fn a_source_collision_offers_a_remedy_both_entry_points_have() {
        let dirs = TestDirs::new();
        create_package_with_dotfiles(&dirs.package_dir, "bat", &[]);
        let target = dirs.target_dir.join("config");
        std::fs::write(&target, "--theme=ansi").unwrap();
        // Occupy exactly where each entry point composes its copy.
        std::fs::create_dir_all(dirs.package_dir.join("bat")).unwrap();
        std::fs::write(dirs.package_dir.join("bat").join("config"), "squatter").unwrap();
        std::fs::create_dir_all(dirs.dotfiles_dir.join("fresh")).unwrap();
        std::fs::write(dirs.dotfiles_dir.join("fresh").join("config"), "squatter").unwrap();

        for events in [
            collect_events(
                dirs.service_with_dotfiles()
                    .track_for_package("bat", target.to_str().unwrap())
                    .await,
            )
            .await,
            collect_events(
                dirs.service_with_dotfiles()
                    .track_standalone("fresh", target.to_str().unwrap())
                    .await,
            )
            .await,
        ] {
            let failure = failure_message(&events);
            assert!(
                failure.contains("Source file already exists"),
                "not the collision refusal: {failure}"
            );
            assert!(
                failure.contains("Remove it first, or track a different file."),
                "the remedy is not one both entry points have: {failure}"
            );
        }
    }

    // The control: with a target both entry points accept, both succeed. Without
    // it every test above could pass on an implementation that refused
    // everything.
    #[tokio::test]
    async fn a_plain_target_is_accepted_by_both() {
        let dirs = fixture();
        let target = dirs.target_dir.join("plain.toml");
        std::fs::write(&target, "key = 1").unwrap();
        let target = target.to_str().unwrap();

        for events in [
            collect_events(
                dirs.service_with_dotfiles()
                    .track_standalone("fresh", target)
                    .await,
            )
            .await,
            collect_events(
                dirs.service_with_dotfiles()
                    .track_for_package("bat", target)
                    .await,
            )
            .await,
        ] {
            assert!(
                matches!(
                    get_operation_result(&events),
                    Some(OperationResult::Success(_))
                ),
                "a plain target must track, got: {:?}",
                get_operation_result(&events)
            );
        }
    }
}

// A track copies the user's file into the repository and then saves the spec. A
// save that is refused or fails must leave no copy behind: a retry after fixing
// the cause has to not trip over "Source file already exists" for a file the user
// never knowingly created (selfie-dt22).
mod a_failed_spec_save_strands_nothing {
    use super::*;

    // A package whose existing dotfile entry carries a key selfie does not
    // model, which `save_package` refuses to rewrite because the rewrite would
    // drop the key. The cheapest reachable save failure, and the one the bug was
    // measured on.
    fn package_that_cannot_be_rewritten(dirs: &TestDirs) -> PathBuf {
        write_package_yaml(
            &dirs.package_dir,
            "creds",
            "name: creds\nenvironments:\n  test:\n    install: \"true\"\ndotfiles:\n  \
             - source: \"creds/token\"\n    target: \"~/.token\"\n    var: oops\n",
        )
    }

    #[tokio::test]
    async fn a_refused_package_save_leaves_no_copied_file() {
        let dirs = TestDirs::new();
        let spec = package_that_cannot_be_rewritten(&dirs);
        let before = std::fs::read(&spec).unwrap();
        let target = dirs.target_dir.join("newfile");
        std::fs::write(&target, "kept").unwrap();

        let events = collect_events(
            dirs.service_with_dotfiles()
                .track_for_package("creds", target.to_str().unwrap())
                .await,
        )
        .await;

        let failure = failure_message(&events);
        assert!(
            failure.contains("var"),
            "not the unknown-key refusal: {failure}"
        );

        let copy = dirs.package_dir.join("creds").join("newfile");
        assert!(
            !copy.exists(),
            "the copy was stranded at {}",
            copy.display()
        );

        // The controls. Without them this passes on a run that never copied
        // anything, or that deleted the user's file instead of the copy.
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "kept",
            "the target file was touched"
        );
        assert_eq!(
            std::fs::read(&spec).unwrap(),
            before,
            "the refused save still rewrote the spec"
        );
    }

    // The same invariant from the other entry point, where the save fails for a
    // filesystem reason rather than a refusal. The copy's own directory is
    // pre-created and writable while the directory holding the spec is not, so
    // the copy lands and only the spec save fails.
    //
    // `TestDirs` configures `state_directory`, which matters here: the deploy
    // state is loaded before either write and would otherwise be looked for
    // under the home directory.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_standalone_save_leaves_no_copied_file() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("gemrc");
        std::fs::write(&target, "gem: --no-document").unwrap();
        std::fs::create_dir_all(dirs.dotfiles_dir.join("gemrc")).unwrap();

        let Some(_restore) = made_unwritable(&dirs.dotfiles_dir) else {
            eprintln!("SKIP a_failed_standalone_save_leaves_no_copied_file: mode bits ignored");
            return;
        };

        let events = collect_events(
            dirs.service_with_dotfiles()
                .track_standalone("gemrc", target.to_str().unwrap())
                .await,
        )
        .await;

        let failure = failure_message(&events);
        let copy = dirs.dotfiles_dir.join("gemrc").join("gemrc");
        assert!(
            !copy.exists(),
            "the copy was stranded at {}: {failure}",
            copy.display()
        );
        assert!(
            !dirs.dotfiles_dir.join("gemrc.yml").exists(),
            "the spec was written after all, so this tested nothing: {failure}"
        );
        // Without these the assertions above pass on a run that never copied
        // anything, or that failed somewhere before the spec save: an absent file
        // proves nothing on its own. The second is also the only integration-level
        // check that the failure names the spec, which the message leaves to the
        // error rather than stating itself.
        assert!(
            failure.contains("was removed"),
            "the copy was never written, so its absence proves nothing: {failure}"
        );
        assert!(
            failure.contains("gemrc.yml"),
            "the failure does not name the spec that could not be saved: {failure}"
        );
    }
}

// The deploy state is recorded last, so its failure is the one that leaves both
// writes in place. Neither is rolled back -- the copy and the entry are correct
// and only the record is missing -- so the failure has to name what exists and
// what recovers it.
//
// `TestDirs` configures `state_directory`, which is what makes the chmod below
// bind: `deploy_state_path` probes only a configured directory, and an unset one
// would be looked for under the home directory instead.
#[cfg(unix)]
mod an_unrecorded_track_names_what_it_wrote {
    use super::*;

    // A state directory that lists but cannot be written to. The load succeeds
    // because an absent state file is the ordinary first run, and the save at the
    // end is the only thing that fails.
    fn unwritable_state_dir(dirs: &TestDirs) -> Option<RestoreMode> {
        made_unwritable(&dirs.state_dir)
    }

    #[tokio::test]
    async fn the_failure_names_the_copy_and_the_spec() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("starship.toml");
        std::fs::write(&target, "format = \"$all\"").unwrap();

        let Some(_restore) = unwritable_state_dir(&dirs) else {
            eprintln!("SKIP the_failure_names_the_copy_and_the_spec: mode bits ignored");
            return;
        };

        let events = collect_events(
            dirs.service_with_dotfiles()
                .track_standalone("starship", target.to_str().unwrap())
                .await,
        )
        .await;

        let failure = failure_message(&events);
        let copy = dirs.dotfiles_dir.join("starship").join("starship.toml");
        let spec = dirs.dotfiles_dir.join("starship.yml");

        // The controls first: both writes must actually have landed, or this is
        // asserting about a run that failed somewhere earlier.
        assert!(copy.exists(), "the copy was not written: {failure}");
        assert!(spec.exists(), "the spec was not written: {failure}");
        assert!(
            !dirs.state_dir.join("deploy-state.yml").exists(),
            "the state was written after all, so this tested nothing"
        );

        assert!(
            failure.contains(copy.to_str().unwrap()),
            "the copy is not named: {failure}"
        );
        assert!(
            failure.contains(spec.to_str().unwrap()),
            "the spec is not named: {failure}"
        );
    }

    // What the user is told to do, and what they must not do. Re-running track
    // hits the source-collision guard, and `selfie apply` records an untracked
    // entry whose target already matches through its in-sync skip arm.
    #[tokio::test]
    async fn the_failure_sends_the_user_to_apply_rather_than_back_to_track() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("starship.toml");
        std::fs::write(&target, "format = \"$all\"").unwrap();

        let Some(_restore) = unwritable_state_dir(&dirs) else {
            eprintln!("SKIP the_failure_sends_the_user_to_apply_rather_than_back_to_track");
            return;
        };

        let events = collect_events(
            dirs.service_with_dotfiles()
                .track_standalone("starship", target.to_str().unwrap())
                .await,
        )
        .await;

        let failure = failure_message(&events);
        assert!(
            failure.contains("selfie apply"),
            "the recovery is not named: {failure}"
        );
        // Asserted as absences, both of them remedies that do not work.
        // "Drift will report it as not tracked" is half true -- with matching
        // content drift says nothing.
        assert!(
            !failure.contains("not tracked"),
            "the failure offers a remedy drift does not provide: {failure}"
        );
        // And a claim that tracking again is refused holds at neither entry
        // point: `track_for_package` answers "already tracking" and exits 0
        // having recorded nothing, and a standalone re-run is stopped by the
        // spec-collision guard rather than by the copy.
        assert!(
            !failure.contains("Re-running track"),
            "the failure claims a refusal that does not happen: {failure}"
        );
    }
}

// `source_path` means the file in the dotfiles repository in every arm of
// `DotfileTracked`, the already-tracked answer included: one arm of one event
// carrying a different kind of path than the rest is what this pins (selfie-fbr1).
//
// No adapter renders the field today -- the event's `Display` names only the spec
// and the target, and the MCP server serializes that `Display` -- so this is
// library correctness rather than a visible defect. The first adapter to render
// it is what the consistency is for.
#[tokio::test]
async fn an_already_tracked_entry_reports_the_copy_the_spec_holds() {
    let dirs = TestDirs::new();
    let home = dirs.target_dir.clone();
    let target = home.join(".config").join("bat").join("config");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, "--theme=ansi").unwrap();
    create_package_with_dotfiles(
        &dirs.package_dir,
        "bat",
        &[("bat/config", "~/.config/bat/config")],
    );

    let events = collect_events(
        dirs.service_with_home(&home)
            .track_for_package("bat", target.to_str().unwrap())
            .await,
    )
    .await;

    match get_operation_result(&events).expect("no Completed event") {
        OperationResult::Success(OperationSuccess::DotfileTracked {
            source_path,
            was_already_tracked,
            ..
        }) => {
            assert!(was_already_tracked, "the entry was already in the spec");
            assert_eq!(
                source_path,
                &dirs.package_dir.join("bat").join("config"),
                "the copy in the repository, derived from the entry's own source"
            );
            // The two paths differ, which is what makes the assertion above
            // capable of failing.
            assert_ne!(
                source_path, &target,
                "reported the deploy target as the source"
            );
        }
        other => panic!("expected an already-tracked success, got: {other:?}"),
    }
}

// A target selfie cannot write to, already recorded in the spec, was the one
// track answer nobody heard about. Track reported it as tracked and said nothing;
// drift said nothing either, because with matching content the drift type is
// `None` and there is no line to carry a reason. So a user who ran both commands
// was told nothing by either (selfie-ykfc).
//
// Nothing is written on this path, so it is reported rather than refused:
// refusing an idempotent no-op helps nobody, and the entry genuinely is tracked.
#[cfg(unix)]
mod an_already_tracked_target_selfie_cannot_write {
    use super::*;
    use std::path::Path;

    // A package tracking `~/.config/bat/config`, with the target in whatever
    // shape the caller plants, and the repository copy holding the same content.
    fn tracked(dirs: &TestDirs, home: &Path) -> PathBuf {
        let target = home.join(".config").join("bat").join("config");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::create_dir_all(dirs.package_dir.join("bat")).unwrap();
        std::fs::write(dirs.package_dir.join("bat").join("config"), "--theme=ansi").unwrap();
        create_package_with_dotfiles(
            &dirs.package_dir,
            "bat",
            &[("bat/config", "~/.config/bat/config")],
        );
        target
    }

    async fn track_again(dirs: &TestDirs, home: &Path, target: &Path) -> Vec<PackageEvent> {
        collect_events(
            dirs.service_with_home(home)
                .track_for_package("bat", target.to_str().unwrap())
                .await,
        )
        .await
    }

    #[tokio::test]
    async fn a_symlinked_target_is_reported_without_being_refused() {
        let dirs = TestDirs::new();
        let home = dirs.target_dir.clone();
        let target = tracked(&dirs, &home);

        let destination = dirs.state_dir.join("real-config");
        std::fs::write(&destination, "--theme=ansi").unwrap();
        std::os::unix::fs::symlink(&destination, &target).unwrap();

        let events = track_again(&dirs, &home, &target).await;

        assert!(
            matches!(
                get_operation_result(&events),
                Some(OperationResult::Success(OperationSuccess::DotfileTracked {
                    was_already_tracked: true,
                    ..
                }))
            ),
            "an idempotent track must not be refused, got: {:?}",
            get_operation_result(&events)
        );

        let warnings = warning_messages(&events);
        assert_eq!(warnings.len(), 1, "expected one warning, got: {warnings:?}");
        assert!(
            warnings[0].contains("symlink"),
            "the warning does not say what is wrong: {}",
            warnings[0]
        );
        // Not the refusal's remedy: the entry already exists, so "track the path
        // it points to" describes something the user cannot now do.
        assert!(
            !warnings[0].contains("track the path it points to"),
            "the warning offers the refusal's remedy: {}",
            warnings[0]
        );
    }

    // The control. Without it the warning could be unconditional and the test
    // above would still pass.
    #[tokio::test]
    async fn a_plain_target_is_reported_in_silence() {
        let dirs = TestDirs::new();
        let home = dirs.target_dir.clone();
        let target = tracked(&dirs, &home);
        std::fs::write(&target, "--theme=ansi").unwrap();

        let events = track_again(&dirs, &home, &target).await;

        assert!(
            matches!(
                get_operation_result(&events),
                Some(OperationResult::Success(_))
            ),
            "the idempotent track must succeed"
        );
        assert!(
            warning_messages(&events).is_empty(),
            "an ordinary already-tracked file warned: {:?}",
            warning_messages(&events)
        );
    }
}
