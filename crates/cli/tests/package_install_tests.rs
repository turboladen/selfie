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
        "Installation completed successfully",
    ));
}
