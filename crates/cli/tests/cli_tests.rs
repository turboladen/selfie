pub mod common;

use common::{
    SELFIE_ENV, add_package, get_command, get_command_with_test_config, setup_default_test_config,
};
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
    let mut cmd = get_command_with_test_config(&temp_dir);
    cmd.args(["--no-color", "config", "validate"]);
    cmd.assert().success();
}

// The following tests just check that the CLI accepts these commands,
// but they don't verify actual functionality since that's not implemented yet

#[test]
fn test_cli_config_validate() {
    let temp_dir = setup_default_test_config();
    let mut cmd = get_command_with_test_config(&temp_dir);
    cmd.args(["config", "validate"]);
    cmd.assert().success();
}

#[test]
fn test_cli_package_list() {
    let temp_dir = setup_default_test_config();
    let package = PackageBuilder::default()
        .name("test-package")
        .version("0.1.0")
        .environment(SELFIE_ENV, |builder| builder.install("echo 'hi'"))
        .build();

    add_package(&temp_dir, &package);

    let mut cmd = get_command_with_test_config(&temp_dir);
    cmd.args(["package", "list"]);
    cmd.assert().success();
}

#[test]
fn test_cli_package_info() {
    let temp_dir = setup_default_test_config();
    let package = PackageBuilder::default()
        .name("test-package")
        .version("0.1.0")
        .environment(SELFIE_ENV, |builder| builder.install("echo 'hi'"))
        .build();

    add_package(&temp_dir, &package);

    let mut cmd = get_command_with_test_config(&temp_dir);
    cmd.args(["package", "info", "test-package"]);
    cmd.assert().success();
}

#[test]
fn test_cli_package_check() {
    let temp_dir = setup_default_test_config();
    let package = PackageBuilder::default()
        .name("test-package")
        .version("0.1.0")
        .environment(SELFIE_ENV, |builder| {
            builder
                .install("echo 'hi'")
                .check_some("echo 'package is installed'")
        })
        .build();

    add_package(&temp_dir, &package);

    let mut cmd = get_command_with_test_config(&temp_dir);
    cmd.args(["package", "check", "test-package"]);
    cmd.assert().success();
}

#[test]
fn test_cli_package_install() {
    let temp_dir = setup_default_test_config();
    let package = PackageBuilder::default()
        .name("test-package")
        .version("0.1.0")
        .environment(SELFIE_ENV, |builder| {
            builder.install("echo 'installing test-package'")
        })
        .build();

    add_package(&temp_dir, &package);

    let mut cmd = get_command_with_test_config(&temp_dir);
    cmd.args(["package", "install", "test-package"]);
    cmd.assert().success();
}

#[test]
fn test_cli_package_create() {
    let temp_dir = setup_default_test_config();
    let mut cmd = get_command_with_test_config(&temp_dir);
    cmd.args(["package", "create", "test-package"]);
    cmd.assert().success();
}

#[test]
fn test_cli_package_validate() {
    let temp_dir = setup_default_test_config();

    let package = PackageBuilder::default()
        .name("test-package")
        .version("0.1.0")
        .environment(SELFIE_ENV, |builder| builder.install("echo 'hi'"))
        .build();

    add_package(&temp_dir, &package);
    let mut cmd = get_command_with_test_config(&temp_dir);
    cmd.args(["package", "validate", "test-package"]);
    cmd.assert().success();
}
