pub mod common;

use common::{SELFIE_ENV, add_package, get_command_with_test_config, setup_default_test_config};
use predicates::prelude::*;
use selfie::package::PackageBuilder;

#[test]
fn test_package_install() {
    let temp_dir = setup_default_test_config();

    // Create a single package
    let package = PackageBuilder::default()
        .name("test-package")
        .version("1.0.0")
        .environment(SELFIE_ENV, |b| {
            b.install("echo 'Installing test package'")
                .check_some("exit 1")
        })
        .build();
    add_package(&temp_dir, &package);

    let mut cmd = get_command_with_test_config(&temp_dir);
    cmd.args(["package", "install", "test-package"]);

    cmd.assert().success().stdout(predicate::str::contains(
        "Package 'test-package' installation completed successfully",
    ));
}

#[test]
fn test_failing_recommend_does_not_fail_parent_install() {
    let temp_dir = setup_default_test_config();

    // Create parent package that recommends a broken package
    let parent = PackageBuilder::default()
        .name("parent-pkg")
        .version("1.0.0")
        .environment(SELFIE_ENV, |b| {
            b.install("echo 'Installing parent'")
                .check_some("exit 1")
                .recommends(vec!["broken-rec"])
        })
        .build();
    add_package(&temp_dir, &parent);

    // Create a broken recommended package
    let broken = PackageBuilder::default()
        .name("broken-rec")
        .version("1.0.0")
        .environment(SELFIE_ENV, |b| b.install("exit 1").check_some("exit 1"))
        .build();
    add_package(&temp_dir, &broken);

    let mut cmd = get_command_with_test_config(&temp_dir);
    cmd.args(["package", "install", "parent-pkg"]);

    // Parent should succeed even though recommend failed.
    // Recommend start goes to stdout; failure warning goes to stderr.
    cmd.assert()
        .success()
        .stdout(predicate::str::contains(
            "Installing recommended: broken-rec",
        ))
        .stderr(predicate::str::contains("broken-rec failed"));
}

#[test]
fn test_no_recommends_flag_skips_recommends() {
    let temp_dir = setup_default_test_config();

    let parent = PackageBuilder::default()
        .name("skip-rec-pkg")
        .version("1.0.0")
        .environment(SELFIE_ENV, |b| {
            b.install("echo 'Installing'")
                .check_some("exit 1")
                .recommends(vec!["some-rec"])
        })
        .build();
    add_package(&temp_dir, &parent);

    let rec = PackageBuilder::default()
        .name("some-rec")
        .version("1.0.0")
        .environment(SELFIE_ENV, |b| {
            b.install("echo 'Installing rec'").check_some("exit 1")
        })
        .build();
    add_package(&temp_dir, &rec);

    let mut cmd = get_command_with_test_config(&temp_dir);
    cmd.args(["package", "install", "skip-rec-pkg", "--no-recommends"]);

    // Should succeed without any recommend output
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("Installing recommended").not());
}
