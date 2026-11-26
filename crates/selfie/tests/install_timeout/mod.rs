// What the install-timeout test binaries share: the secret, and an install of
// one package over real `/bin/sh` that goes quiet past a 2-second silence limit.

use std::{path::Path, time::Duration};

use selfie::{
    commands::ShellCommandRunner,
    config::SelfieConfigBuilder,
    package::{
        event::PackageEvent,
        service::{InstallOptions, PackageService as _},
    },
};

/// The secret each fixture writes to `secret.txt` and the tests look for.
pub const SECRET: &str = "7Qx9Lm2Vb8Rt4Kp6Zw3NfHj";

/// Install `command` as the package `name` from `dir`, with `secret.txt`
/// beside its spec, and give back every event.
///
/// # Panics
///
/// If the spec or the secret cannot be written, or if the install neither times
/// out nor finishes within 20 seconds.
pub async fn install(dir: &Path, name: &str, command: &str) -> Vec<PackageEvent> {
    std::fs::write(dir.join("secret.txt"), format!("{SECRET}\n")).unwrap();
    std::fs::write(
        dir.join(format!("{name}.yml")),
        format!("name: {name}\nenvironments:\n  test:\n    install: {command:?}\n"),
    )
    .unwrap();
    let config = SelfieConfigBuilder::default()
        .environment("test")
        .package_directory(dir)
        .command_timeout_unchecked(2)
        .build();
    let runner =
        ShellCommandRunner::new(ShellCommandRunner::default_shell(), Duration::from_secs(2));
    let service =
        test_common::create_service_with_fs_and_runner(config, selfie::fs::RealFileSystem, runner);
    tokio::time::timeout(
        Duration::from_secs(20),
        test_common::collect_events(service.install(name, InstallOptions::default()).await),
    )
    .await
    .expect("the install neither timed out nor finished")
}
