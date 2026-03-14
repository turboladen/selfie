// crates/selfie/tests/package_validation_tests.rs
use selfie::{
    fs::real::RealFileSystem,
    package::{port::PackageRepository, repository::YamlPackageRepository},
    validation::ValidationLevel,
};
use std::{fs, path::Path};

fn create_test_package(dir: &Path, name: &str, content: &str) {
    let package_dir = dir.join("packages");
    fs::create_dir_all(&package_dir).unwrap();

    let package_path = package_dir.join(format!("{name}.yaml"));
    fs::write(&package_path, content).unwrap();
}

#[test]
fn test_validate_package_with_invalid_command_syntax() {
    let temp_dir = tempfile::tempdir().unwrap();

    // Create package with unmatched quote in command
    let package_yaml = r#"
name: test-package
version: 1.0.0
environments:
  test-env:
    install: 'echo "hello world'
"#;

    create_test_package(temp_dir.path(), "test-package", package_yaml);

    let fs = RealFileSystem;
    let repo_path = temp_dir.path().join("packages");
    let repo = YamlPackageRepository::new(fs, repo_path.clone());

    let package = repo.get_package("test-package").unwrap();
    let validation = package.package().validate("test-env");

    // Should find at least one error with command syntax
    assert!(validation.issues().has_errors());

    let command_errors = validation
        .issues()
        .all_issues()
        .iter()
        .filter(|issue: &&selfie::validation::ValidationIssue| {
            issue.category() == selfie::validation::ValidationErrorCategory::CommandSyntax
        })
        .collect::<Vec<_>>();

    assert!(!command_errors.is_empty());
    assert_eq!(command_errors[0].level(), ValidationLevel::Error);
    assert!(command_errors[0].message().contains("quote"));
}

#[test]
fn test_validate_package_with_invalid_url() {
    let temp_dir = tempfile::tempdir().unwrap();

    // Create package with invalid homepage URL
    let package_yaml = r#"
name: test-package
version: 1.0.0
homepage: "not-a-valid-url"
environments:
  test-env:
    install: echo "hello"
"#;

    create_test_package(temp_dir.path(), "test-package", package_yaml);

    let fs = RealFileSystem;
    let repo_path = temp_dir.path().join("packages");
    let repo = YamlPackageRepository::new(fs, repo_path.clone());

    let package = repo.get_package("test-package").unwrap();
    let validation = package.package().validate("test-env");

    // Should find URL format error
    let url_errors = validation
        .issues()
        .all_issues()
        .iter()
        .filter(|issue: &&selfie::validation::ValidationIssue| {
            issue.category() == selfie::validation::ValidationErrorCategory::UrlFormat
        })
        .collect::<Vec<_>>();

    assert!(!url_errors.is_empty());
    assert_eq!(url_errors[0].level(), ValidationLevel::Error);
}
