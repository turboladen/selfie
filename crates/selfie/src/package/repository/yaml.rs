use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::{
    fs::{FileSystem, filesystem::FileSystemError},
    package::{
        GetPackage, Package,
        port::{
            ListPackagesOutput, PackageError, PackageListError, PackageParseError,
            PackageRepoError, PackageRepository,
        },
    },
    validation::ValidationIssue,
};

#[derive(Debug, Clone)]
pub struct YamlPackageRepository<F: FileSystem> {
    fs: F,
    package_dir: PathBuf,
}

impl<F: FileSystem> YamlPackageRepository<F> {
    pub fn new(fs: F, package_dir: PathBuf) -> Self {
        Self { fs, package_dir }
    }

    /// List all YAML files in a directory, in sorted path order.
    ///
    /// The sort is load-bearing, not cosmetic. `read_dir` yields entries in
    /// whatever order the filesystem stores them — insertion order on APFS,
    /// hash order on ext4 — so without it every consumer of package enumeration
    /// inherits that non-determinism.
    ///
    /// It matters most to `selfie apply` under `stop_on_error`: which packages
    /// get deployed before the abort would otherwise depend on inode hashing, so
    /// the same package directory failing the same way could leave two machines
    /// in different states. For a tool whose purpose is making machines
    /// converge, that is the wrong property to have. It also makes `spec list`,
    /// `validate --all`, and `audit --all` report in a stable order, and makes
    /// the "multiple files match this name" error name them predictably.
    fn list_yaml_files(&self, dir: &Path) -> Result<Vec<PathBuf>, std::io::Error> {
        let entries = self
            .fs
            .list_directory(dir)
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let mut yaml_files: Vec<PathBuf> = entries
            .into_iter()
            .filter(|path| {
                if let Some(ext) = path.extension() {
                    let ext_str = ext.to_string_lossy().to_lowercase();
                    ext_str == "yaml" || ext_str == "yml"
                } else {
                    false
                }
            })
            .collect();

        // By path rather than by package name: names come from parsing, which
        // has not happened yet and which fails for exactly the files whose
        // ordering matters least. Filename and name agree by convention, and
        // validation warns when they do not.
        yaml_files.sort();

        Ok(yaml_files)
    }

    /// Filter a list of paths to those matching `{name}.yml` or `{name}.yaml`.
    fn filter_matching_packages(name: &str, entries: Vec<PathBuf>) -> Vec<PathBuf> {
        entries
            .into_iter()
            .filter(|path| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|file_name| {
                        file_name == format!("{name}.yml") || file_name == format!("{name}.yaml")
                    })
            })
            .collect()
    }

    /// List directory entries and find matching package files, also reporting how many
    /// entries were examined (for error diagnostics in `get_package`).
    fn find_package_files_with_context(
        &self,
        name: &str,
        files_examined: &mut usize,
    ) -> Result<Vec<PathBuf>, std::io::Error> {
        let entries = self
            .fs
            .list_directory(&self.package_dir)
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        *files_examined = entries.len();

        Ok(Self::filter_matching_packages(name, entries))
    }

    fn get_file_size(&self, path: &Path) -> u64 {
        self.fs
            .read_file(path)
            .map(|content| content.len() as u64)
            .unwrap_or(0)
    }

    // Load a Package from a file using the FileSystem trait
    fn load_package_from_file(&self, path: &Path) -> Result<Package, PackageParseError> {
        let content = self
            .fs
            .read_file(path)
            .map_err(|e| PackageParseError::FileSystemError {
                package_path: path.to_path_buf(),
                source: Arc::new(e),
            })?;

        let mut package: Package =
            serde_saphyr::from_str(&content).map_err(|e| PackageParseError::YamlParse {
                package_path: path.to_path_buf(),
                source: Arc::new(e),
            })?;
        package.path = path.to_path_buf();
        package.raw_yaml = content;

        Ok(package)
    }
}

impl<F: FileSystem> PackageRepository for YamlPackageRepository<F> {
    fn read_referenced_file(
        &self,
        package_path: &Path,
        relative_path: &str,
    ) -> Result<String, FileSystemError> {
        let base_dir = package_path.parent().unwrap_or_else(|| Path::new("."));
        let resolved = base_dir.join(relative_path);

        // A package names its referenced files with paths relative to its own
        // directory, and nothing stops one naming `../../../etc/passwd`. Reading
        // it would let validation report the contents of an arbitrary file.
        if !crate::paths::is_within(&resolved, base_dir) {
            return Err(FileSystemError::IoError(Arc::new(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("'{relative_path}' escapes the package directory"),
            ))));
        }

        self.fs.read_file(&resolved)
    }

    fn get_package(&self, name: &str) -> Result<GetPackage, PackageRepoError> {
        // Check if package directory exists first
        if !self.fs.path_exists(&self.package_dir) {
            return Err(PackageRepoError::PackageListError(
                PackageListError::PackageDirectoryNotFound(self.package_dir.clone()),
            ));
        }

        let search_patterns = vec![format!("{}.yml", name), format!("{}.yaml", name)];
        let mut files_examined = 0;

        let package_files = self
            .find_package_files_with_context(name, &mut files_examined)
            .map_err(|e| PackageRepoError::IoError(Arc::new(e)))?;

        if package_files.is_empty() {
            return Err(PackageError::PackageNotFound {
                name: name.to_string(),
                packages_path: self.package_dir.clone(),
                files_examined,
                search_patterns,
            }
            .into());
        }

        if package_files.len() > 1 {
            return Err(PackageError::MultiplePackagesFound {
                name: name.to_string(),
                packages_path: self.package_dir.clone(),
                conflicting_paths: package_files,
                files_examined,
                search_patterns,
            }
            .into());
        }

        let package_file = &package_files[0];
        let file_size = self.get_file_size(package_file);

        let package = self
            .load_package_from_file(package_file)
            .map_err(|source| PackageError::ParseError {
                name: name.to_string(),
                packages_path: self.package_dir.clone(),
                failed_file: package_file.clone(),
                file_size_bytes: file_size,
                source,
            })?;

        Ok(GetPackage::from_existing(package, package_file.clone()))
    }

    fn list_packages(&self) -> Result<ListPackagesOutput, PackageListError> {
        if !self.fs.path_exists(&self.package_dir) {
            return Err(PackageListError::PackageDirectoryNotFound(
                self.package_dir.clone(),
            ));
        }

        // Get all YAML files in the directory
        let yaml_files = self.list_yaml_files(&self.package_dir).map_err(Arc::new)?;

        // Parse each file into a Package
        let mut packages: Vec<Result<Package, PackageParseError>> = Vec::new();

        for path in yaml_files {
            packages.push(self.load_package_from_file(&path));
        }

        Ok(ListPackagesOutput(packages))
    }

    fn find_package_files(&self, name: &str) -> Result<Vec<PathBuf>, PackageListError> {
        if !self.fs.path_exists(&self.package_dir) {
            return Err(PackageListError::PackageDirectoryNotFound(
                self.package_dir.clone(),
            ));
        }

        let entries = self
            .fs
            .list_directory(&self.package_dir)
            .map_err(|e| std::io::Error::other(e.to_string()))
            .map_err(|e| PackageListError::from(Arc::new(e)))?;

        Ok(Self::filter_matching_packages(name, entries))
    }

    fn save_package(&self, package: &Package, path: &Path) -> Result<(), PackageRepoError> {
        // A save rewrites the file from the struct, dropping every key the struct
        // does not model. In a dotfile entry that key is what makes
        // `content_source()` return `Invalid`, so writing the file would launder a
        // refused entry into a deployable one: `var:` for `vars:` would vanish and
        // the next apply would write the *unrendered* template — literal
        // `{{ api_key }}` — over the target. Refuse the write instead.
        //
        // The guard lives here rather than at the call sites because this is where
        // the key is destroyed, and a fourth caller cannot forget it. See selfie-6lz4.
        let unknown = package.validate_unknown_dotfile_fields();
        if !unknown.is_empty() {
            let fields: Vec<&str> = unknown.iter().map(ValidationIssue::field).collect();
            return Err(PackageRepoError::UnknownDotfileFields {
                path: path.to_path_buf(),
                fields: fields.join(", "),
            });
        }

        // Serialize the package to YAML
        let yaml_content = serde_saphyr::to_string(package).map_err(|e| {
            PackageRepoError::IoError(Arc::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Failed to serialize package to YAML: {e}"),
            )))
        })?;

        // Write the YAML content to the specified path
        self.fs.write_file(path, yaml_content.as_bytes())?;

        // Best-effort: run dprint fmt on the saved file to normalize formatting.
        // Silently ignored if dprint is not installed, fails, or times out (5s).
        if let Ok(canonical) = std::fs::canonicalize(path)
            && let Ok(mut child) = std::process::Command::new("dprint")
                .args(["fmt", &canonical.to_string_lossy()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
        {
            let start = std::time::Instant::now();
            let timeout = std::time::Duration::from_secs(5);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break, // Process finished
                    Ok(None) if start.elapsed() < timeout => {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    _ => {
                        let _ = child.kill(); // Timed out or error — kill it
                        break;
                    }
                }
            }
        }

        Ok(())
    }

    fn remove_package(&self, name: &str) -> Result<(), PackageRepoError> {
        // First, get the package to find its file path
        let package_blob = self.get_package(name)?;

        // Remove the file from the file system
        self.fs.remove_file(&package_blob.file_path)?;

        Ok(())
    }

    fn find_dependent_packages(
        &self,
        target_package: &str,
    ) -> Result<Vec<Package>, PackageRepoError> {
        let mut dependents = Vec::new();

        // Get all packages and check their dependencies
        let package_list = self.list_packages()?;

        for package in package_list.valid_packages() {
            // Skip the target package itself
            if package.name() == target_package {
                continue;
            }

            // Check all environments for dependencies
            for env_config in package.environments().values() {
                if env_config
                    .dependencies()
                    .contains(&target_package.to_string())
                {
                    dependents.push(package.clone());
                    break; // Found dependency, no need to check other environments
                }
            }
        }

        Ok(dependents)
    }
}

#[cfg(test)]
mod tests {
    use mockall::*;

    use super::*;
    use crate::fs::filesystem::MockFileSystem;
    use crate::fs::real::RealFileSystem;
    use crate::package::port::PackageRepoError;
    use std::collections::HashMap;
    use tempfile::TempDir;

    #[test]
    fn test_get_package_success() {
        let mut fs = MockFileSystem::default();
        let package_dir = PathBuf::from("/test/packages");

        // Mock path_exists for directory
        fs.expect_path_exists()
            .with(predicate::eq(package_dir.clone()))
            .returning(|_| true);

        // Mock list_directory to return the package file
        let package_path = package_dir.join("ripgrep.yaml");
        let package_path_for_list = package_path.clone();
        fs.expect_list_directory()
            .with(predicate::eq(package_dir.clone()))
            .returning(move |_| Ok(vec![package_path_for_list.clone()]));

        let yaml = r"
            name: ripgrep

            environments:
              mac:
                install: brew install ripgrep
        ";

        fs.mock_read_file(package_path, yaml);

        let repo = YamlPackageRepository::new(fs, package_dir.clone());
        let package = repo.get_package("ripgrep").unwrap();

        assert_eq!(package.package.name(), "ripgrep");
        assert_eq!(package.package.environments().len(), 1);
    }

    #[test]
    fn test_get_package_not_found() {
        let mut fs = MockFileSystem::default();
        // Mock filesystem to simulate package not found
        let package_dir = PathBuf::from("/test/packages");

        // Mock path_exists for directory
        fs.expect_path_exists()
            .with(predicate::eq(package_dir.clone()))
            .returning(|_| true);

        fs.expect_list_directory()
            .with(predicate::eq(package_dir.clone()))
            .returning(|_| Ok(vec![PathBuf::from("/test/packages/other.yaml")]));

        let repo = YamlPackageRepository::new(fs, package_dir.clone());
        let result = repo.get_package("nonexistent");

        assert!(matches!(
            result,
            Err(PackageRepoError::PackageError(ref box_error))
            if matches!(**box_error, PackageError::PackageNotFound { .. })
        ));
    }

    #[test]
    fn test_get_package_directory_not_found() {
        let mut fs = MockFileSystem::default();
        // Mock filesystem error
        let package_dir = PathBuf::from("/test/nonexistent");

        // Mock path_exists to return false for the directory
        fs.expect_path_exists()
            .with(predicate::eq(package_dir.clone()))
            .returning(|_| false);

        let repo = YamlPackageRepository::new(fs, package_dir.clone());
        let result = repo.get_package("ripgrep");

        assert!(matches!(
            result,
            Err(PackageRepoError::PackageListError(
                PackageListError::PackageDirectoryNotFound(_)
            ))
        ));
    }

    #[test]
    fn test_get_package_multiple_found() {
        let mut fs = MockFileSystem::default();
        let package_dir = PathBuf::from("/test/packages");

        // Create multiple mock package files with the same name
        let yaml_path = package_dir.join("ripgrep.yaml");
        let yml_path = package_dir.join("ripgrep.yml");

        // Mock path_exists for directory
        fs.expect_path_exists()
            .with(predicate::eq(package_dir.clone()))
            .returning(|_| true);

        // Mock list_directory to return both files
        let long_path_for_list = yaml_path.clone();
        let short_path_for_list = yml_path.clone();
        fs.expect_list_directory()
            .with(predicate::eq(package_dir.clone()))
            .returning(move |_| {
                Ok(vec![
                    long_path_for_list.clone(),
                    short_path_for_list.clone(),
                ])
            });

        let repo = YamlPackageRepository::new(fs, package_dir.clone());
        let result = repo.get_package("ripgrep");

        assert!(matches!(
            result,
            Err(PackageRepoError::PackageError(ref box_error))
            if matches!(**box_error, PackageError::MultiplePackagesFound { .. })
        ));
    }

    #[test]
    fn test_find_package_files() {
        let mut fs = MockFileSystem::default();
        let package_dir = PathBuf::from("/test/packages");

        let yaml_path = package_dir.join("ripgrep.yaml");
        let yml_path = package_dir.join("other.yml");

        // Mock path_exists for directory check only
        fs.expect_path_exists()
            .with(predicate::eq(package_dir.clone()))
            .returning(|_| true);

        // Mock list_directory to return all files in the package dir
        let yaml_clone = yaml_path.clone();
        let yml_clone = yml_path.clone();
        fs.expect_list_directory()
            .with(predicate::eq(package_dir.clone()))
            .returning(move |_| Ok(vec![yaml_clone.clone(), yml_clone.clone()]));

        let repo = YamlPackageRepository::new(fs, package_dir.clone());

        // Should find ripgrep.yaml
        let files = repo.find_package_files("ripgrep").unwrap();
        assert_eq!(files.len(), 1, "{:#?}", files);
        assert_eq!(files[0], yaml_path);

        // Should find other.yml
        let files = repo.find_package_files("other").unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0], yml_path);

        // Should not find nonexistent
        let files = repo.find_package_files("nonexistent").unwrap();
        assert_eq!(files.len(), 0);
    }

    #[test]
    fn test_list_packages() {
        let mut fs = MockFileSystem::default();
        let package_dir = PathBuf::from("/test/packages");

        fs.expect_path_exists()
            .with(predicate::eq(package_dir.clone()))
            .returning(|_| true);

        // Add valid package files
        let package1 = r"
            name: ripgrep

            environments:
              test-env:
                install: brew install ripgrep
        ";

        let package2 = r"
            name: fzf

            environments:
              other-env:
                install: brew install fzf
        ";

        fs.mock_list_directory(
            package_dir.clone(),
            &[
                package_dir.join("ripgrep.yaml"),
                package_dir.join("fzf.yml"),
                package_dir.join("invalid.yaml"),
            ],
        );

        fs.mock_read_file(package_dir.join("ripgrep.yaml"), package1);
        fs.mock_read_file(package_dir.join("fzf.yml"), package2);
        fs.mock_read_file(package_dir.join("invalid.yaml"), "not valid yaml: :");

        let repo = YamlPackageRepository::new(fs, package_dir.clone());
        let package_output = repo.list_packages().unwrap();

        // Should find both valid packages
        assert_eq!(
            package_output.valid_packages().collect::<Vec<_>>().len(),
            2,
            "{:#?}",
            package_output
        );
        assert_eq!(
            package_output.invalid_packages().collect::<Vec<_>>().len(),
            1,
            "{:#?}",
            package_output
        );
        assert_eq!(package_output.len(), 3, "{:#?}", package_output);

        // Check package details
        let ripgrep = package_output.get("ripgrep").unwrap();
        let fzf = package_output.get("fzf").unwrap();

        assert!(ripgrep.environments().contains_key("test-env"));

        assert!(fzf.environments().contains_key("other-env"));
    }

    #[test]
    fn list_yaml_files_returns_them_in_sorted_order() {
        // `read_dir` yields filesystem order — insertion order on APFS, hash
        // order on ext4 — so this is the invariant that stops package
        // enumeration inheriting it. Returned here deliberately unsorted, the
        // way a real filesystem may.
        let mut fs = MockFileSystem::default();
        let dir = PathBuf::from("/test/dir");
        let cloned = dir.clone();

        fs.expect_list_directory()
            .with(predicate::eq(dir.clone()))
            .returning(move |_| {
                Ok(vec![
                    cloned.join("zzz.yml"),
                    cloned.join("mmm.yaml"),
                    cloned.join("aaa.yml"),
                ])
            });

        let repo = YamlPackageRepository::new(fs, PathBuf::from("/dummy"));
        let yaml_files = repo.list_yaml_files(&dir).unwrap();

        assert_eq!(
            yaml_files,
            vec![
                dir.join("aaa.yml"),
                dir.join("mmm.yaml"),
                dir.join("zzz.yml"),
            ],
            "enumeration must not depend on the order the filesystem happens to \
             return entries in"
        );
    }

    #[test]
    fn test_list_yaml_files() {
        let mut fs = MockFileSystem::default();
        let dir = PathBuf::from("/test/dir");
        let cloned = dir.clone();

        fs.expect_list_directory()
            .with(predicate::eq(dir.clone()))
            .returning(move |_| {
                Ok(vec![
                    cloned.join("file1.yaml"),
                    cloned.join("file2.yml"),
                    cloned.join("file3.txt"),
                    cloned.join("file4.YAML"),
                    cloned.join("file5.YML"),
                ])
            });

        let repo = YamlPackageRepository::new(fs, Path::new("/dummy").to_path_buf()); // Path doesn't matter here
        let yaml_files = repo.list_yaml_files(&dir).unwrap();

        // Should find all yaml/yml files regardless of case
        assert_eq!(yaml_files.len(), 4);

        // Check each expected file is found
        assert!(yaml_files.contains(&dir.join("file1.yaml")));
        assert!(yaml_files.contains(&dir.join("file2.yml")));
        assert!(yaml_files.contains(&dir.join("file4.YAML")));
        assert!(yaml_files.contains(&dir.join("file5.YML")));

        // Check that non-yaml file is not included
        assert!(!yaml_files.contains(&dir.join("file3.txt")));
    }

    #[test]
    fn test_available_packages() {
        let mut fs = MockFileSystem::default();
        let package_dir = PathBuf::from("/test/packages");

        fs.expect_path_exists()
            .with(predicate::eq(package_dir.clone()))
            .returning(|_| true);

        // Add valid and invalid package files
        let ripgrep_package = r"
            name: ripgrep

            environments:
              test-env:
                install: brew install ripgrep
        ";

        let fzf_package = r"
            name: fzf

            environments:
              other-env:
                install: brew install fzf
        ";

        fs.mock_list_directory(
            package_dir.clone(),
            &[
                package_dir.join("ripgrep.yaml"),
                package_dir.join("fzf.yml"),
                package_dir.join("invalid.yaml"),
            ],
        );

        fs.mock_read_file(package_dir.join("ripgrep.yaml"), ripgrep_package);
        fs.mock_read_file(package_dir.join("fzf.yml"), fzf_package);
        fs.mock_read_file(package_dir.join("invalid.yaml"), "not valid yaml: :");

        let repo = YamlPackageRepository::new(fs, package_dir.clone());
        let available_packages = repo.available_packages().unwrap();

        // Should find only valid packages
        assert_eq!(available_packages.len(), 2);

        // Check package details
        assert!(available_packages.iter().any(|p| *p == "ripgrep"));
        assert!(available_packages.iter().any(|p| *p == "fzf"));
    }

    #[test]
    fn test_package_parse_error_handling() {
        let mut fs = MockFileSystem::default();
        let package_dir = PathBuf::from("/test/packages");
        let package_path = package_dir.join("invalid.yaml");

        fs.expect_path_exists()
            .with(predicate::eq(package_dir.clone()))
            .returning(|_| true);

        fs.expect_path_exists()
            .with(predicate::eq(package_path.clone()))
            .returning(|_| true);

        fs.expect_path_exists()
            .with(predicate::eq(package_dir.join("invalid.yml")))
            .returning(|_| false);

        // Mock invalid YAML content
        let invalid_yaml = "invalid: yaml: content: [";

        fs.mock_list_directory(package_dir.clone(), std::slice::from_ref(&package_path));
        fs.mock_read_file(package_path.clone(), invalid_yaml);

        let repo = YamlPackageRepository::new(fs, package_dir.clone());
        let result = repo.get_package("invalid");

        assert!(result.is_err());
        match result.unwrap_err() {
            PackageRepoError::PackageError(box_error) => match *box_error {
                PackageError::ParseError {
                    name,
                    packages_path,
                    source,
                    ..
                } => {
                    assert_eq!(name, "invalid");
                    assert_eq!(packages_path, package_dir);
                    match source {
                        PackageParseError::YamlParse {
                            package_path: error_path,
                            ..
                        } => {
                            assert_eq!(error_path, package_path);
                        }
                        _ => panic!("Expected YamlParse error"),
                    }
                }
                _ => panic!("Expected ParseError"),
            },
            _ => panic!("Expected PackageError"),
        }
    }

    #[test]
    fn test_directory_not_found_error() {
        let mut fs = MockFileSystem::default();
        let nonexistent_dir = PathBuf::from("/nonexistent");

        fs.expect_path_exists()
            .with(predicate::eq(nonexistent_dir.clone()))
            .returning(|_| false);

        let repo = YamlPackageRepository::new(fs, nonexistent_dir.clone());
        let result = repo.list_packages();

        assert!(result.is_err());
        match result.unwrap_err() {
            PackageListError::PackageDirectoryNotFound(path) => {
                assert_eq!(path, nonexistent_dir);
            }
            PackageListError::IoError(_) => panic!("Expected PackageDirectoryNotFound error"),
        }
    }

    #[test]
    fn test_multiple_packages_found_error() {
        let mut fs = MockFileSystem::default();
        let package_dir = PathBuf::from("/test/packages");

        fs.expect_path_exists()
            .with(predicate::eq(package_dir.clone()))
            .returning(|_| true);

        let file1 = package_dir.join("duplicate.yaml");
        let file2 = package_dir.join("duplicate.yml");

        fs.expect_path_exists()
            .with(predicate::eq(file1.clone()))
            .returning(|_| true);
        fs.expect_path_exists()
            .with(predicate::eq(file2.clone()))
            .returning(|_| true);

        // Create multiple files with the same package name
        let package_yaml = r"
            name: duplicate

            environments:
              test-env:
                install: echo test
        ";

        fs.mock_list_directory(package_dir.clone(), &[file1.clone(), file2.clone()]);
        fs.mock_read_file(file1, package_yaml);
        fs.mock_read_file(file2, package_yaml);

        let repo = YamlPackageRepository::new(fs, package_dir.clone());
        let result = repo.get_package("duplicate");

        assert!(result.is_err());
        match result.unwrap_err() {
            PackageRepoError::PackageError(box_error) => match *box_error {
                PackageError::MultiplePackagesFound {
                    name,
                    packages_path,
                    ..
                } => {
                    assert_eq!(name, "duplicate");
                    assert_eq!(packages_path, package_dir);
                }
                _ => panic!("Expected MultiplePackagesFound error"),
            },
            _ => panic!("Expected PackageError"),
        }
    }

    #[test]
    fn test_find_dependent_packages_no_dependencies() {
        // Test finding dependents when there are none
        let temp_dir = TempDir::new().unwrap();
        let package_dir = temp_dir.path().join("packages");
        std::fs::create_dir_all(&package_dir).unwrap();

        // Create a simple package with no dependencies
        let package_content = r"
name: simple-package

environments:
  test:
    install: echo 'install'
    dependencies: []
";
        std::fs::write(
            package_dir.join("simple-package.yml"),
            package_content.trim(),
        )
        .unwrap();

        let fs = RealFileSystem;
        let repo = YamlPackageRepository::new(fs, package_dir);

        let dependents = repo.find_dependent_packages("target-package").unwrap();
        assert!(dependents.is_empty());
    }

    #[test]
    fn test_find_dependent_packages_with_dependencies() {
        // Test finding dependents when there are some
        let temp_dir = TempDir::new().unwrap();
        let package_dir = temp_dir.path().join("packages");
        std::fs::create_dir_all(&package_dir).unwrap();

        // Create target package
        let target_content = r"
name: target-package

environments:
  test:
    install: echo 'install target'
    dependencies: []
";
        std::fs::write(
            package_dir.join("target-package.yml"),
            target_content.trim(),
        )
        .unwrap();

        // Create dependent package
        let dependent_content = r"
name: dependent-package

environments:
  test:
    install: echo 'install dependent'
    dependencies:
      - target-package
";
        std::fs::write(
            package_dir.join("dependent-package.yml"),
            dependent_content.trim(),
        )
        .unwrap();

        // Create another package without dependency
        let independent_content = r"
name: independent-package

environments:
  test:
    install: echo 'install independent'
    dependencies: []
";
        std::fs::write(
            package_dir.join("independent-package.yml"),
            independent_content.trim(),
        )
        .unwrap();

        let fs = RealFileSystem;
        let repo = YamlPackageRepository::new(fs, package_dir);

        let dependents = repo.find_dependent_packages("target-package").unwrap();
        assert_eq!(dependents.len(), 1);
        assert_eq!(dependents[0].name(), "dependent-package");
    }

    #[test]
    fn test_find_dependent_packages_multiple_environments() {
        // Test that dependencies are found across different environments
        let temp_dir = TempDir::new().unwrap();
        let package_dir = temp_dir.path().join("packages");
        std::fs::create_dir_all(&package_dir).unwrap();

        // Create target package
        let target_content = r"
name: target-package

environments:
  test:
    install: echo 'install target'
    dependencies: []
";
        std::fs::write(
            package_dir.join("target-package.yml"),
            target_content.trim(),
        )
        .unwrap();

        // Create dependent package with dependency in production environment
        let dependent_content = r"
name: multi-env-package

environments:
  test:
    install: echo 'install test'
    dependencies: []
  production:
    install: echo 'install prod'
    dependencies:
      - target-package
";
        std::fs::write(
            package_dir.join("multi-env-package.yml"),
            dependent_content.trim(),
        )
        .unwrap();

        let fs = RealFileSystem;
        let repo = YamlPackageRepository::new(fs, package_dir);

        let dependents = repo.find_dependent_packages("target-package").unwrap();
        assert_eq!(dependents.len(), 1);
        assert_eq!(dependents[0].name(), "multi-env-package");
    }

    #[test]
    fn test_find_dependent_packages_excludes_target_package() {
        // Test that the target package itself is not included in dependents
        let temp_dir = TempDir::new().unwrap();
        let package_dir = temp_dir.path().join("packages");
        std::fs::create_dir_all(&package_dir).unwrap();

        // Create a self-referencing package (edge case)
        let self_ref_content = r"
name: self-package

environments:
  test:
    install: echo 'install'
    dependencies:
      - self-package
";
        std::fs::write(
            package_dir.join("self-package.yml"),
            self_ref_content.trim(),
        )
        .unwrap();

        let fs = RealFileSystem;
        let repo = YamlPackageRepository::new(fs, package_dir);

        let dependents = repo.find_dependent_packages("self-package").unwrap();
        assert!(dependents.is_empty());
    }

    #[test]
    fn test_find_dependent_packages_multiple_dependents() {
        // Test finding multiple packages that depend on the same target
        let temp_dir = TempDir::new().unwrap();
        let package_dir = temp_dir.path().join("packages");
        std::fs::create_dir_all(&package_dir).unwrap();

        // Create target package
        let target_content = r"
name: shared-lib

environments:
  test:
    install: echo 'install shared-lib'
    dependencies: []
";
        std::fs::write(package_dir.join("shared-lib.yml"), target_content.trim()).unwrap();

        // Create first dependent package
        let dependent1_content = r"
name: app-one

environments:
  test:
    install: echo 'install app-one'
    dependencies:
      - shared-lib
";
        std::fs::write(package_dir.join("app-one.yml"), dependent1_content.trim()).unwrap();

        // Create second dependent package
        let dependent2_content = r"
name: app-two

environments:
  test:
    install: echo 'install app-two'
    dependencies:
      - shared-lib
      - some-other-lib
";
        std::fs::write(package_dir.join("app-two.yml"), dependent2_content.trim()).unwrap();

        let fs = RealFileSystem;
        let repo = YamlPackageRepository::new(fs, package_dir);

        let dependents = repo.find_dependent_packages("shared-lib").unwrap();
        assert_eq!(dependents.len(), 2);
        let names: Vec<String> = dependents.iter().map(|p| p.name().to_string()).collect();
        assert!(names.contains(&"app-one".to_string()));
        assert!(names.contains(&"app-two".to_string()));
    }

    #[test]
    fn test_find_dependent_packages_handles_parse_errors() {
        // Test that the method gracefully handles packages with parse errors
        let temp_dir = TempDir::new().unwrap();
        let package_dir = temp_dir.path().join("packages");
        std::fs::create_dir_all(&package_dir).unwrap();

        // Create a valid package
        let valid_content = r"
name: valid-package

environments:
  test:
    install: echo 'install valid'
    dependencies:
      - target-package
";
        std::fs::write(package_dir.join("valid-package.yml"), valid_content.trim()).unwrap();

        // Create an invalid package file
        let invalid_content = "invalid: yaml: content: [unclosed";
        std::fs::write(package_dir.join("invalid-package.yml"), invalid_content).unwrap();

        let fs = RealFileSystem;
        let repo = YamlPackageRepository::new(fs, package_dir);

        // Should still find the valid dependent, ignoring the parse error
        let dependents = repo.find_dependent_packages("target-package").unwrap();
        assert_eq!(dependents.len(), 1);
        assert_eq!(dependents[0].name(), "valid-package");
    }

    #[test]
    fn test_error_display_formatting() {
        let package_dir = PathBuf::from("/packages");

        // Test PackageNotFound error
        let not_found_error = PackageError::PackageNotFound {
            name: "missing".to_string(),
            packages_path: package_dir.clone(),
            files_examined: 0,
            search_patterns: vec!["missing.yml".to_string()],
        };
        assert!(not_found_error.to_string().contains("missing"));
        assert!(not_found_error.to_string().contains("/packages"));

        // Test MultiplePackagesFound error
        let multiple_error = PackageError::MultiplePackagesFound {
            name: "duplicate".to_string(),
            packages_path: package_dir.clone(),
            conflicting_paths: vec![
                PathBuf::from("/packages/duplicate.yml"),
                PathBuf::from("/packages/duplicate.yaml"),
            ],
            files_examined: 2,
            search_patterns: vec!["duplicate.yml".to_string(), "duplicate.yaml".to_string()],
        };
        assert!(multiple_error.to_string().contains("duplicate"));
        assert!(
            multiple_error
                .to_string()
                .contains("Multiple packages found")
        );

        // Test PackageDirectoryNotFound error
        let dir_error = PackageListError::PackageDirectoryNotFound(package_dir.clone());
        assert!(dir_error.to_string().contains("/packages"));
        assert!(dir_error.to_string().contains("does not exist"));
    }

    #[test]
    fn test_save_package_success() {
        let mut fs = MockFileSystem::default();
        let package_dir = PathBuf::from("/test/packages");
        let package_path = PathBuf::from("/test/packages/test-package.yml");

        // Create a test package
        let package = Package::new(
            "test-package".to_string(),
            None,
            None,
            Vec::new(),
            None,
            HashMap::new(),
            package_path.clone(),
        );

        // Mock the write_file operation
        fs.mock_write_file(&package_path);

        let repo = YamlPackageRepository::new(fs, package_dir);

        // Test saving the package
        let result = repo.save_package(&package, &package_path);
        assert!(result.is_ok());
    }

    /// A package YAML with one well-formed dotfile and one carrying `var:`.
    fn package_with_typo() -> Package {
        serde_saphyr::from_str(
            "name: creds\nenvironments:\n  test:\n    install: echo i\ndotfiles:\n  \
             - source: creds.tpl\n    target: ~/.creds\n    var:\n      k: op read x\n",
        )
        .expect("fixture must parse — the typo is a validation error, not a parse error")
    }

    #[test]
    fn save_package_refuses_an_entry_carrying_an_unrecognized_key() {
        // Saving rewrites the file from the struct, which drops `var:` entirely.
        // The entry would stop being `Invalid` and the next apply would write the
        // unrendered template over the credentials target — so the write must not
        // happen at all. `times(0)` is the assertion that matters: refusing after
        // writing would already have destroyed the key.
        let mut fs = MockFileSystem::default();
        let package_dir = PathBuf::from("/test/packages");
        let package_path = package_dir.join("creds.yml");

        fs.expect_write_file().times(0);

        let repo = YamlPackageRepository::new(fs, package_dir);
        let err = repo
            .save_package(&package_with_typo(), &package_path)
            .expect_err("a package with an unrecognized dotfile key must not be rewritten");

        assert!(
            matches!(err, PackageRepoError::UnknownDotfileFields { .. }),
            "got: {err:?}"
        );
        assert!(
            err.to_string().contains("dotfiles[0].var"),
            "the diagnostic must name the offending key, got: {err}"
        );
    }

    #[test]
    fn save_package_refuses_an_entry_carrying_an_anchor_that_collides_with_a_field() {
        // Same hazard as `var:`, reached a different way: a rewrite drops `_vars`
        // and the entry stops being refused, so the next apply writes the
        // unrendered template over the credentials target. The refusal has to
        // cover it, or the fix for selfie-kj5y is undone by the first `selfie
        // spec edit`.
        let mut fs = MockFileSystem::default();
        let package_dir = PathBuf::from("/test/packages");
        let package_path = package_dir.join("creds.yml");

        fs.expect_write_file().times(0);

        let package: Package = serde_saphyr::from_str(
            "name: creds\nenvironments:\n  test:\n    install: echo i\ndotfiles:\n  \
             - source: creds.tpl\n    target: ~/.creds\n    _vars:\n      k: op read x\n",
        )
        .expect("fixture must parse — the collision is a validation error, not a parse error");

        let repo = YamlPackageRepository::new(fs, package_dir);
        let err = repo
            .save_package(&package, &package_path)
            .expect_err("a colliding anchor must not be rewritten away");

        assert!(
            err.to_string().contains("dotfiles[0]._vars"),
            "the diagnostic must name the offending key, got: {err}"
        );
    }

    #[test]
    fn save_package_refuses_an_environment_scoped_entry_carrying_an_unrecognized_key() {
        // The guard has to reach `environments.<env>.dotfiles` too. Kept separate
        // from the shared-dotfiles test because dropping the environments loop from
        // `validate_unknown_dotfile_fields` leaves that one green — someone
        // deleting the loop would otherwise see every save test still pass.
        let mut fs = MockFileSystem::default();
        let package_dir = PathBuf::from("/test/packages");
        let package_path = package_dir.join("creds.yml");

        fs.expect_write_file().times(0);

        let package: Package = serde_saphyr::from_str(
            "name: creds\nenvironments:\n  test:\n    install: echo i\n    dotfiles:\n      \
             - source: creds.tpl\n        target: ~/.creds\n        var:\n          k: op read x\n",
        )
        .expect("fixture must parse");

        let repo = YamlPackageRepository::new(fs, package_dir);
        let err = repo
            .save_package(&package, &package_path)
            .expect_err("an environment-scoped unrecognized key must not be rewritten");

        assert!(
            err.to_string()
                .contains("environments.test.dotfiles[0].var"),
            "the diagnostic must name the environment and entry, got: {err}"
        );
    }

    #[test]
    fn save_package_still_writes_a_package_whose_dotfiles_are_all_recognized() {
        // Control for the test above: the guard must refuse the typo, not every
        // package that happens to declare dotfiles.
        let mut fs = MockFileSystem::default();
        let package_dir = PathBuf::from("/test/packages");
        let package_path = package_dir.join("creds.yml");

        fs.mock_write_file(&package_path);

        let package: Package = serde_saphyr::from_str(
            "name: creds\nenvironments:\n  test:\n    install: echo i\ndotfiles:\n  \
             - source: creds.tpl\n    target: ~/.creds\n    vars:\n      k: op read x\n",
        )
        .unwrap();

        let repo = YamlPackageRepository::new(fs, package_dir);
        assert!(repo.save_package(&package, &package_path).is_ok());
    }

    #[test]
    fn test_save_package_filesystem_error() {
        let mut fs = MockFileSystem::default();
        let package_dir = PathBuf::from("/test/packages");
        let package_path = PathBuf::from("/test/packages/test-package.yml");

        // Create a test package
        let package = Package::new(
            "test-package".to_string(),
            None,
            None,
            Vec::new(),
            None,
            HashMap::new(),
            package_path.clone(),
        );

        // Mock write_file to fail
        fs.expect_write_file()
            .with(
                mockall::predicate::eq(package_path.clone()),
                mockall::predicate::always(),
            )
            .returning(|_, _| {
                Err(crate::fs::filesystem::FileSystemError::IoError(Arc::new(
                    std::io::Error::new(std::io::ErrorKind::PermissionDenied, "Permission denied"),
                )))
            });

        let repo = YamlPackageRepository::new(fs, package_dir);

        // Test saving the package should fail
        let result = repo.save_package(&package, &package_path);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            PackageRepoError::FileSystemError(_)
        ));
    }

    #[test]
    fn test_remove_package_success() {
        let mut fs = MockFileSystem::default();
        let package_dir = PathBuf::from("/test/packages");
        let package_name = "test-package";
        let package_path = package_dir.join("test-package.yml");

        // Mock get_package to return a valid package
        let package_yaml = r#"
name: test-package

environments:
  default:
    install: echo "install"
    check: echo "check"
"#;

        // Set up mocks for get_package operation
        fs.expect_path_exists()
            .with(mockall::predicate::eq(package_dir.clone()))
            .returning(|_| true);

        let package_path_for_list = package_path.clone();
        fs.expect_list_directory()
            .with(mockall::predicate::eq(package_dir.clone()))
            .returning(move |_| Ok(vec![package_path_for_list.clone()]));

        let package_path_for_read = package_path.clone();
        fs.expect_read_file()
            .with(mockall::predicate::eq(package_path_for_read.clone()))
            .returning(move |_| Ok(package_yaml.to_string()));

        // Mock the remove_file operation
        fs.mock_remove_file(&package_path);

        let repo = YamlPackageRepository::new(fs, package_dir);

        // Test removing the package
        let result = repo.remove_package(package_name);
        assert!(result.is_ok());
    }

    #[test]
    fn test_remove_package_not_found() {
        let mut fs = MockFileSystem::default();
        let package_dir = PathBuf::from("/test/packages");
        let package_name = "nonexistent-package";

        // Mock get_package to return package not found
        fs.expect_path_exists()
            .with(mockall::predicate::eq(package_dir.clone()))
            .returning(|_| true);

        fs.expect_list_directory()
            .with(mockall::predicate::eq(package_dir.clone()))
            .returning(|_| Ok(vec![])); // No packages found

        let repo = YamlPackageRepository::new(fs, package_dir);

        // Test removing non-existent package should fail
        let result = repo.remove_package(package_name);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            PackageRepoError::PackageError(_)
        ));
    }

    #[test]
    fn test_remove_package_filesystem_error() {
        let mut fs = MockFileSystem::default();
        let package_dir = PathBuf::from("/test/packages");
        let package_name = "test-package";
        let package_path = package_dir.join("test-package.yml");

        // Mock get_package to return a valid package
        let package_yaml = r#"
name: test-package

environments:
  default:
    install: echo "install"
    check: echo "check"
"#;

        // Set up mocks for get_package operation
        fs.expect_path_exists()
            .with(mockall::predicate::eq(package_dir.clone()))
            .returning(|_| true);

        let package_path_for_list = package_path.clone();
        fs.expect_list_directory()
            .with(mockall::predicate::eq(package_dir.clone()))
            .returning(move |_| Ok(vec![package_path_for_list.clone()]));

        let package_path_for_read = package_path.clone();
        fs.expect_read_file()
            .with(mockall::predicate::eq(package_path_for_read.clone()))
            .returning(move |_| Ok(package_yaml.to_string()));

        // Mock remove_file to fail
        fs.expect_remove_file()
            .with(mockall::predicate::eq(package_path.clone()))
            .returning(|_| {
                Err(crate::fs::filesystem::FileSystemError::IoError(Arc::new(
                    std::io::Error::new(std::io::ErrorKind::PermissionDenied, "Permission denied"),
                )))
            });

        let repo = YamlPackageRepository::new(fs, package_dir);

        // Test removing package should fail due to filesystem error
        let result = repo.remove_package(package_name);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            PackageRepoError::FileSystemError(_)
        ));
    }
}
