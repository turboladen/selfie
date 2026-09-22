use selfie::package::SpecOrigin;
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

    let repo = YamlPackageRepository::new(
        RealFileSystem,
        package_dir.clone(),
        SpecOrigin::PackageDirectory,
    );
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

    let repo = YamlPackageRepository::new(
        RealFileSystem,
        package_dir.clone(),
        SpecOrigin::PackageDirectory,
    );
    let result = repo.get_package("duplicate");

    // Should return an error about multiple packages
    assert!(matches!(
        result,
        Err(selfie::package::port::PackageRepoError::PackageError(ref box_error))
        if matches!(**box_error, PackageError::MultiplePackagesFound { .. })
    ));
}

// A package directory behind a parent that denies access exists, so listing it
// is an IO error rather than a directory the user should create.
#[test]
fn a_package_directory_behind_an_unreadable_parent_is_not_reported_missing() {
    use std::os::unix::fs::PermissionsExt as _;

    // Restores the parent's mode when dropped, so a panic before the end of the
    // test cannot leave a directory the temp dir is unable to remove.
    struct Locked<'a>(&'a std::path::Path);
    impl Drop for Locked<'_> {
        fn drop(&mut self) {
            let _ = fs::set_permissions(self.0, fs::Permissions::from_mode(0o755));
        }
    }

    let temp_dir = tempdir().unwrap();
    let parent = temp_dir.path().join("locked");
    let package_dir = parent.join("packages");
    fs::create_dir_all(&package_dir).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o000)).unwrap();
    let _locked = Locked(&parent);

    // Root ignores the mode bits, so confirm the precondition rather than infer
    // it from the user id.
    if fs::read_dir(&parent).is_ok() {
        eprintln!(
            "SKIP a_package_directory_behind_an_unreadable_parent_is_not_reported_missing: \
             still readable"
        );
        return;
    }

    let repo = YamlPackageRepository::new(
        RealFileSystem,
        package_dir.clone(),
        SpecOrigin::PackageDirectory,
    );

    match repo.list_packages() {
        // A parent that denies traversal leaves the path unclassifiable, which is
        // unknown rather than absent. The error names the directory it read, because
        // there are two configured directories a listing failure could be about.
        Err(error) if matches!(error.state(), selfie::fs::DirectoryState::Unknown(_)) => {
            let rendered = error.to_string();
            assert!(
                rendered.contains(&package_dir.display().to_string()),
                "the error must name the directory, got: {rendered}"
            );
        }
        Err(other) => panic!("an unreachable directory must be unknown, got: {other:?}"),
        Ok(_) => panic!("an unreachable directory must not list"),
    }
}
