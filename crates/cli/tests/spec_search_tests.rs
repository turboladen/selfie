pub mod common;

use common::{SELFIE_ENV, add_package, sandboxed_command, setup_default_test_config};
use predicates::prelude::*;
use selfie::package::PackageBuilder;

#[test]
fn test_spec_search_by_name() {
    let temp_dir = setup_default_test_config();

    let package = PackageBuilder::default()
        .name("ripgrep")
        .description("Fast search tool")
        .environment(SELFIE_ENV, |b| b.install("brew install ripgrep"))
        .build();
    add_package(&temp_dir, &package);

    let other = PackageBuilder::default()
        .name("node")
        .description("JavaScript runtime")
        .environment(SELFIE_ENV, |b| b.install("brew install node"))
        .build();
    add_package(&temp_dir, &other);

    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["spec", "search", "ripgrep"]);

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("ripgrep"))
        .stdout(predicate::str::contains("node").not());
}

#[test]
fn test_spec_search_by_description() {
    let temp_dir = setup_default_test_config();

    let package = PackageBuilder::default()
        .name("node")
        .description("JavaScript runtime")
        .environment(SELFIE_ENV, |b| b.install("brew install node"))
        .build();
    add_package(&temp_dir, &package);

    let other = PackageBuilder::default()
        .name("ripgrep")
        .description("Fast search tool")
        .environment(SELFIE_ENV, |b| b.install("brew install ripgrep"))
        .build();
    add_package(&temp_dir, &other);

    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["spec", "search", "runtime"]);

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("node"))
        .stdout(predicate::str::contains("ripgrep").not());
}

#[test]
fn test_spec_search_case_insensitive() {
    let temp_dir = setup_default_test_config();

    let package = PackageBuilder::default()
        .name("ripgrep")
        .description("Fast search tool")
        .environment(SELFIE_ENV, |b| b.install("brew install ripgrep"))
        .build();
    add_package(&temp_dir, &package);

    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["spec", "search", "RIPGREP"]);

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("ripgrep"));
}

#[test]
fn test_spec_search_no_matches() {
    let temp_dir = setup_default_test_config();

    let package = PackageBuilder::default()
        .name("node")
        .environment(SELFIE_ENV, |b| b.install("brew install node"))
        .build();
    add_package(&temp_dir, &package);

    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["spec", "search", "nonexistent"]);

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("No specs found"));
}

#[test]
fn test_spec_search_empty_directory() {
    let temp_dir = setup_default_test_config();

    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["spec", "search", "anything"]);

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("No specs found"));
}

#[test]
fn test_spec_search_matches_across_environments() {
    let temp_dir = setup_default_test_config();

    // Package in a different environment should still be found by search
    let package = PackageBuilder::default()
        .name("apt-tool")
        .description("APT package manager tool")
        .environment("ubuntu", |b| b.install("apt install apt-tool"))
        .build();
    add_package(&temp_dir, &package);

    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["spec", "search", "apt-tool"]);

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("apt-tool"));
}
