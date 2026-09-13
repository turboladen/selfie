pub mod common;

use common::{SELFIE_ENV, add_package, get_command, sandboxed_command, setup_default_test_config};
use predicates::prelude::*;
use selfie::package::PackageBuilder;

#[test]
fn test_cli_help() {
    let mut cmd = get_command();
    cmd.arg("--help");
    cmd.assert().success().stdout(predicate::str::contains(
        "Selfie - A personal package manager",
    ));
}

#[test]
fn test_cli_version() {
    let mut cmd = get_command();
    cmd.arg("--version");
    cmd.assert().success();
}

#[test]
fn test_cli_invalid_command() {
    let mut cmd = get_command();
    cmd.arg("invalid-command");
    cmd.assert().failure();
}

#[test]
fn test_cli_invalid_subcommand() {
    let mut cmd = get_command();
    cmd.args(["package", "invalid-subcommand"]);
    cmd.assert().failure();
}

#[test]
fn test_cli_missing_required_arg() {
    let mut cmd = get_command();
    cmd.args(["package", "install"]); // Missing package_name
    cmd.assert().failure();
}

#[test]
fn test_cli_with_environment() {
    let mut cmd = get_command();
    // Just test that the arg is accepted, not that it does anything yet
    cmd.args(["-e", SELFIE_ENV, "help"]);
    cmd.assert().success();
}

#[test]
fn test_cli_with_package_directory() {
    let mut cmd = get_command();
    // Just test that the arg is accepted, not that it does anything yet
    cmd.args(["-p", "/test/path", "help"]);
    cmd.assert().success();
}

#[test]
fn test_cli_verbose_flag() {
    let mut cmd = get_command();
    cmd.args(["-v", "help"]);
    cmd.assert().success();
}

#[test]
fn test_cli_no_color() {
    let temp_dir = setup_default_test_config();
    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["--no-color", "config", "validate"]);
    cmd.assert().success();
}

// The following tests just check that the CLI accepts these commands,
// but they don't verify actual functionality since that's not implemented yet

#[test]
fn test_cli_config_validate() {
    let temp_dir = setup_default_test_config();
    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["config", "validate"]);
    cmd.assert().success();
}

#[test]
fn test_cli_package_list() {
    let temp_dir = setup_default_test_config();
    let package = PackageBuilder::default()
        .name("test-package")
        .environment(SELFIE_ENV, |builder| builder.install("echo 'hi'"))
        .build();

    add_package(&temp_dir, &package);

    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["package", "list"]);
    cmd.assert().success();
}

#[test]
fn test_cli_spec_info() {
    let temp_dir = setup_default_test_config();
    let package = PackageBuilder::default()
        .name("test-package")
        .environment(SELFIE_ENV, |builder| builder.install("echo 'hi'"))
        .build();

    add_package(&temp_dir, &package);

    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["spec", "info", "test-package"]);
    cmd.assert().success();
}

#[test]
fn test_cli_package_check() {
    let temp_dir = setup_default_test_config();
    let package = PackageBuilder::default()
        .name("test-package")
        .environment(SELFIE_ENV, |builder| {
            builder
                .install("echo 'hi'")
                .check_some("echo 'package is installed'")
        })
        .build();

    add_package(&temp_dir, &package);

    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["package", "check", "test-package"]);
    cmd.assert().success();
}

#[test]
fn test_cli_package_install() {
    let temp_dir = setup_default_test_config();
    let package = PackageBuilder::default()
        .name("test-package")
        .environment(SELFIE_ENV, |builder| {
            builder.install("echo 'installing test-package'")
        })
        .build();

    add_package(&temp_dir, &package);

    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["package", "install", "test-package"]);
    cmd.assert().success();
}

#[test]
fn test_cli_package_status() {
    let temp_dir = setup_default_test_config();
    let package = PackageBuilder::default()
        .name("test-package")
        .environment(SELFIE_ENV, |builder| {
            builder
                .install("echo 'hi'")
                .check_some("echo 'package is installed'")
        })
        .build();

    add_package(&temp_dir, &package);

    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["package", "status", "test-package"]);
    cmd.assert().success();
}

#[test]
fn test_cli_spec_create() {
    let temp_dir = setup_default_test_config();
    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["spec", "create", "test-package"]);
    cmd.assert().success();
}

#[test]
fn test_cli_spec_remove_not_found() {
    let temp_dir = setup_default_test_config();
    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["spec", "remove", "nonexistent-package"]);
    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("not found"));
}

// The twin of the control above, differing in one way: the file is there. A
// user looking at it is told what selfie could not do with it, and where the
// two cases share a message they cannot act on either.
#[test]
fn test_cli_spec_remove_does_not_call_an_unreadable_file_missing() {
    let temp_dir = setup_default_test_config();
    let path = temp_dir.path().join("packages").join("brokenpkg.yaml");
    std::fs::write(&path, "{{{\n").unwrap();

    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["spec", "remove", "brokenpkg"]);
    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("not found").not())
        .stderr(predicate::str::contains("could not load that spec"));

    assert!(path.exists(), "a spec selfie refused to read must survive");
}

// Removing a package is irreversible, and the sentence the user acts on is the
// one saying nothing depends on it. A spec selfie could not read may name the
// package, so that sentence cannot be printed over an incomplete check.
#[test]
fn test_cli_spec_remove_does_not_clear_a_package_it_could_not_fully_check() {
    let temp_dir = setup_default_test_config();
    let packages = temp_dir.path().join("packages");
    std::fs::write(
        packages.join("target.yaml"),
        format!("name: target\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"),
    )
    .unwrap();
    std::fs::write(packages.join("brokenpkg.yaml"), "{{{\n").unwrap();

    let output = sandboxed_command(&temp_dir)
        .args(["spec", "remove", "target", "-y"])
        .output()
        .unwrap();

    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !all.contains("is not a dependency of any other packages"),
        "must not clear a package over an unreadable spec, got:\n{all}"
    );
    // Once, not twice. The pre-flight check and the service's own repeat of it
    // both reach the unreadable file, and a run that names it twice reads as two
    // broken specs.
    assert_eq!(
        all.matches("brokenpkg.yaml").count(),
        1,
        "must name the spec it could not read exactly once, got:\n{all}"
    );
}

// The control. With every spec readable the clearance is honest and must still
// be printed, or the guard above has made `spec remove` useless.
#[test]
fn test_cli_spec_remove_still_clears_a_package_nothing_depends_on() {
    let temp_dir = setup_default_test_config();
    let packages = temp_dir.path().join("packages");
    std::fs::write(
        packages.join("target.yaml"),
        format!("name: target\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"),
    )
    .unwrap();

    let output = sandboxed_command(&temp_dir)
        .args(["spec", "remove", "target", "-y"])
        .output()
        .unwrap();

    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        all.contains("is not a dependency of any other packages"),
        "a complete check with no dependents must still say so, got:\n{all}"
    );
}

#[test]
fn test_cli_spec_validate() {
    let temp_dir = setup_default_test_config();

    let package = PackageBuilder::default()
        .name("test-package")
        .environment(SELFIE_ENV, |builder| builder.install("echo 'hi'"))
        .build();

    add_package(&temp_dir, &package);
    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["spec", "validate", "test-package"]);
    cmd.assert().success();
}
