use std::{
    fs::{self, File},
    io::Write,
};

use selfie::{
    fs::real::RealFileSystem,
    package::{
        port::{PackageError, PackageRepository},
        repository::YamlPackageRepository,
    },
};
use tempfile::tempdir;

#[test]
fn test_repository_with_mixed_package_formats() {
    let temp_dir = tempdir().unwrap();
    let package_dir = temp_dir.path().join("packages");
    fs::create_dir(&package_dir).unwrap();

    // Create package files in different formats
    let package1_path = package_dir.join("package1.yaml");
    let package2_path = package_dir.join("package2.yml");
    let not_a_package_path = package_dir.join("not-a-package.txt");
    let readme_path = package_dir.join("README.md");

    let package1_yaml = r#"
name: package1
environments:
  test-env:
    install: echo "installing package1"
"#;
    let package2_yaml = r#"
name: package2
environments:
  test-env:
    install: echo "installing package2"
"#;

    // Write package files
    let mut file = File::create(&package1_path).unwrap();
    file.write_all(package1_yaml.as_bytes()).unwrap();

    let mut file = File::create(&package2_path).unwrap();
    file.write_all(package2_yaml.as_bytes()).unwrap();

    File::create(&not_a_package_path).unwrap();
    File::create(&readme_path).unwrap();

    let repo = YamlPackageRepository::new(RealFileSystem, package_dir.clone());
    let result = repo.list_packages().unwrap();

    // Should find exactly two valid packages
    assert_eq!(result.valid_packages().count(), 2);

    // Verify package names
    let names: Vec<&str> = result
        .valid_packages()
        .map(selfie::package::Package::name)
        .collect();
    assert!(names.contains(&"package1"));
    assert!(names.contains(&"package2"));
}

#[test]
fn test_repository_duplicate_package_names() {
    let temp_dir = tempdir().unwrap();
    let package_dir = temp_dir.path().join("packages");
    fs::create_dir(&package_dir).unwrap();

    // Create duplicate package files
    let duplicate_yaml_path = package_dir.join("duplicate.yaml");
    let duplicate_yml_path = package_dir.join("duplicate.yml");

    let duplicate_yaml = r#"
name: duplicate
version: 1.0.0
environments:
  test-env:
    install: echo "installing duplicate"
"#;

    // Write duplicate package files
    let mut file = File::create(&duplicate_yaml_path).unwrap();
    file.write_all(duplicate_yaml.as_bytes()).unwrap();

    let mut file = File::create(&duplicate_yml_path).unwrap();
    file.write_all(duplicate_yaml.as_bytes()).unwrap();

    let repo = YamlPackageRepository::new(RealFileSystem, package_dir.clone());
    let result = repo.get_package("duplicate");

    // Should return an error about multiple packages
    assert!(matches!(
        result,
        Err(selfie::package::port::PackageRepoError::PackageError(ref box_error))
        if matches!(**box_error, PackageError::MultiplePackagesFound { .. })
    ));
}
