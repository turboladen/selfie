use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::{
    dotfile_service::service::repository_read_refusal,
    fs::{FileSystem, filesystem::FileSystemError, target::repository_path},
    package::{
        GetPackage, Package, TopLevelKeys,
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

    // The only read of a package file in the crate, and the only place the
    // irregular-file guard has to sit. Reading a fifo blocks until a writer
    // arrives, so one `mkfifo ghost.yml` in the package directory wedges every
    // command that enumerates specs. `command_timeout` does not bound it; that
    // governs provider commands, not filesystem calls (selfie-h2kr).
    //
    // It stays a separate function because it is the whole read path: anything
    // that grows a second way to load a spec goes through it, so the guard is not
    // something a new caller can forget.
    fn read_spec_file(&self, path: &Path) -> Result<String, PackageParseError> {
        if let Some(refusal) = self.fs.irregular_target_refusal(&repository_path(path)) {
            return Err(irregular_spec_refusal(path, refusal));
        }

        self.fs
            .read_file(path)
            .map_err(|e| PackageParseError::FileSystemError {
                package_path: path.to_path_buf(),
                source: Arc::new(e),
            })
    }

    // Load a Package from a file using the FileSystem trait
    fn load_package_from_file(&self, path: &Path) -> Result<Package, PackageParseError> {
        let content = self.read_spec_file(path)?;

        let mut package: Package =
            serde_saphyr::from_str(&content).map_err(|e| PackageParseError::YamlParse {
                package_path: path.to_path_buf(),
                source: Arc::new(e),
            })?;
        package.set_source(path.to_path_buf(), content);

        Ok(package)
    }
}

// The third wording frame on the shared classifier. `IrregularTarget`'s `Display`
// is worded for a deploy target and `repository_read_refusal` for a file in the
// dotfiles repository; neither describes a package spec. Only the wording is new.
//
// The `other` arm fails **closed** rather than returning the content. Nothing
// reaches it today, and letting a future variant through would un-guard the read.
//
// It re-words rather than carrying the `FileSystemError` through: every refusal
// variant's `Display` says selfie will not *write* through a *target*, false
// twice over on a read, and would print the path a second time.
fn irregular_spec_refusal(path: &Path, refusal: FileSystemError) -> PackageParseError {
    match refusal {
        FileSystemError::IrregularTarget { kind, .. } => PackageParseError::IrregularFile {
            package_path: path.to_path_buf(),
            kind,
        },
        FileSystemError::SymlinkedTarget { points_to, .. } => PackageParseError::RefusedFile {
            package_path: path.to_path_buf(),
            reason: match points_to {
                Some(dest) => format!("it is a symlink to '{}'", dest.display()),
                None => "it is a symlink".to_string(),
            },
        },
        other => PackageParseError::RefusedFile {
            package_path: path.to_path_buf(),
            reason: other.to_string(),
        },
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
        //
        // Lexical, so it rules out a written traversal and not a symlink planted
        // inside the package directory — see `crate::paths::is_within`.
        if !crate::paths::is_within(&resolved, base_dir) {
            return Err(FileSystemError::IoError(Arc::new(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("'{relative_path}' escapes the package directory"),
            ))));
        }

        // `spec validate` reads templates through here, and reading a fifo blocks
        // until a writer arrives — so a fifo template wedged `selfie spec validate`
        // with no timeout, the same hang apply and drift have. Same guard, same
        // position: after containment, immediately before the read.
        if let Some(refusal) = self
            .fs
            .irregular_target_refusal(&repository_path(&resolved))
        {
            return Err(FileSystemError::IoError(Arc::new(std::io::Error::other(
                format!(
                    "{}. Replace it with a regular file.",
                    repository_read_refusal(&refusal)
                ),
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

        // A refused file gets its own variant. `ParseError` renders as "Parse
        // error in package `ghost`", which would send the user to inspect YAML
        // in a file that was never opened.
        let package = self
            .load_package_from_file(package_file)
            .map_err(|source| match source {
                refusal @ (PackageParseError::IrregularFile { .. }
                | PackageParseError::RefusedFile { .. }) => PackageError::UnreadableFile {
                    name: name.to_string(),
                    packages_path: self.package_dir.clone(),
                    failed_file: package_file.clone(),
                    source: refusal,
                },
                source => PackageError::ParseError {
                    name: name.to_string(),
                    packages_path: self.package_dir.clone(),
                    failed_file: package_file.clone(),
                    source,
                },
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
        // `content_source()` return `Err(InvalidEntry::UnknownKeys(_))`, so writing
        // the file would launder a refused entry into a deployable one: `var:` for
        // `vars:` — or an anchor named `_vars:` — would vanish and the next apply
        // would write the *unrendered* template — literal `{{ api_key }}` — over
        // the target. Refuse the write instead.
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

        // The same refusal at the file's top level: `_dotfiles:` as an anchor, or
        // a plain `configs:`, is not modeled either, so rewriting drops it and
        // every entry under it with no diagnostic.
        //
        // A file selfie could not read back is refused too. Apply proceeds on one
        // of those because refusing would stop a deploy over a check that did not
        // run; a declined write costs a retry instead, and the thing at risk here
        // is the user's own text (selfie-ebvx).
        match package.top_level_keys() {
            TopLevelKeys::Checked(unknown) if !unknown.is_empty() => {
                let fields: Vec<&str> = unknown.iter().map(|u| u.key.as_str()).collect();
                return Err(PackageRepoError::UnknownTopLevelFields {
                    path: path.to_path_buf(),
                    fields: fields.join(", "),
                });
            }
            TopLevelKeys::Unchecked(error) => {
                return Err(PackageRepoError::UncheckedTopLevel {
                    path: path.to_path_buf(),
                    error: error.clone(),
                });
            }
            TopLevelKeys::Checked(_) | TopLevelKeys::NoSource => {}
        }

        // The same refusal one level down. An environment's unknown key is not
        // modeled, so serializing from the struct drops it: `audt:` for `audit:`
        // takes the user's command text with it, and nothing says so.
        //
        // Reported with the environment named, since a package may define
        // several and only one carries the key.
        let mut env_names: Vec<&String> = package.environments().keys().collect();
        env_names.sort();
        for env_name in env_names {
            let unknown = package.environments()[env_name].unknown_keys();
            if !unknown.is_empty() {
                let fields: Vec<String> = unknown
                    .iter()
                    .map(|key| format!("environments.{env_name}.{key}"))
                    .collect();
                return Err(PackageRepoError::UnknownEnvironmentFields {
                    path: path.to_path_buf(),
                    fields: fields.join(", "),
                });
            }
        }

        // Serialize the package to YAML
        let yaml_content = serde_saphyr::to_string(package).map_err(|e| {
            PackageRepoError::IoError(Arc::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Failed to serialize package to YAML: {e}"),
            )))
        })?;

        // Write the YAML content to the specified path.
        //
        // `write_file_no_follow` rather than a plain write: a symlink at `path`
        // would otherwise be followed and the package YAML written wherever it
        // points. The `path_exists` checks guarding the callers *follow* symlinks,
        // so a dangling one passes them and the write then creates the file at the
        // planter's chosen path (selfie-yw7i).
        //
        // The direction is inward, so the refusal is re-worded rather than
        // rendered as-is. See `PackageRepoError::UnwritablePath`.
        self.fs
            .write_file_no_follow(&repository_path(path), yaml_content.as_bytes())
            .map_err(|e| match &e {
                FileSystemError::SymlinkedTarget { points_to, .. } => {
                    PackageRepoError::UnwritablePath {
                        path: path.to_path_buf(),
                        reason: match points_to {
                            Some(dest) => {
                                format!("it is a symlink to '{}'", dest.display())
                            }
                            None => "it is a symlink".to_string(),
                        },
                    }
                }
                FileSystemError::IrregularTarget { kind, .. } => PackageRepoError::UnwritablePath {
                    path: path.to_path_buf(),
                    reason: format!("it is a {kind}"),
                },
                // Not a refusal -- a permission problem, a full disk. The
                // filesystem's own message is the right one for those, and it
                // does not claim anything about a target.
                _ => PackageRepoError::FileSystemError(e),
            })?;

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
        fs.mock_no_irregular_files();
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
        fs.mock_no_irregular_files();
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
        fs.mock_no_irregular_files();
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
        fs.mock_no_irregular_files();
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

    // `spec validate` reads templates through `read_referenced_file`, and reading
    // a fifo blocks until a writer arrives. Before the guard this wedged the
    // command with no timeout, reproduced as `selfie spec validate` printing
    // "Validating creds..." and never returning.
    //
    // A real filesystem and a real fifo: `MockFileSystem` cannot block, so a
    // mocked fixture would pass against the unguarded code and prove nothing.
    // The deadline is what turns a regression into a failure instead of a wedged
    // suite -- the read blocks on this thread, so no async timeout would fire.
    #[cfg(unix)]
    #[test]
    fn a_fifo_template_is_refused_by_validate() {
        use std::sync::mpsc;

        let temp_dir = TempDir::new().unwrap();
        let package_dir = temp_dir.path().join("packages");
        std::fs::create_dir_all(package_dir.join("creds")).unwrap();
        let template = package_dir.join("creds/credentials.tpl");
        nix::unistd::mkfifo(&template, nix::sys::stat::Mode::S_IRWXU).unwrap();

        let package_path = package_dir.join("creds.yml");
        let repo = YamlPackageRepository::new(RealFileSystem, package_dir);

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(
                repo.read_referenced_file(&package_path, "creds/credentials.tpl")
                    .map_err(|e| e.to_string()),
            );
        });

        let result = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("reading a fifo template must not block");
        let message = result.expect_err("a fifo template must be refused");
        assert!(message.contains("named pipe (fifo)"), "got: {message}");
        assert!(message.contains("repository file"), "got: {message}");
    }

    // A spec file the enumeration will read, and one it must refuse. Only
    // `good.yml` gets a `read_file` expectation, so any attempt to read the
    // irregular one fails the test on an unexpected call -- which is the actual
    // invariant, since the read is what blocks. A mock cannot block, so this
    // pins the guard's position and its wording while the real-fifo tests below
    // pin the absence of the hang.
    fn fs_with_one_irregular_spec(
        package_dir: &Path,
        good: &Path,
        irregular: &Path,
        kind: &'static str,
    ) -> MockFileSystem {
        let mut fs = MockFileSystem::default();
        fs.mock_path_exists(package_dir, true);
        fs.mock_list_directory(package_dir, &[good, irregular]);
        fs.mock_read_file(
            good,
            "name: good\n\nenvironments:\n  test-env:\n    install: true\n",
        );

        let irregular_owned = irregular.to_path_buf();
        fs.expect_irregular_target_refusal()
            .returning(move |target| {
                (target.path() == irregular_owned).then(|| FileSystemError::IrregularTarget {
                    path: irregular_owned.clone(),
                    kind,
                })
            });
        fs
    }

    #[test]
    fn a_fifo_spec_is_reported_rather_than_read() {
        let package_dir = PathBuf::from("/test/packages");
        let good = package_dir.join("good.yml");
        let ghost = package_dir.join("ghost.yml");
        let fs = fs_with_one_irregular_spec(&package_dir, &good, &ghost, "named pipe (fifo)");

        let output = YamlPackageRepository::new(fs, package_dir)
            .list_packages()
            .expect("listing must succeed despite the fifo");

        // The control. If the guard refused everything, or the fixture never
        // reached a read, this is what notices.
        let valid: Vec<_> = output.valid_packages().collect();
        assert_eq!(valid.len(), 1);
        assert_eq!(valid[0].name(), "good");
        assert!(valid[0].environments().contains_key("test-env"));

        let invalid: Vec<_> = output.invalid_packages().collect();
        assert_eq!(invalid.len(), 1);
        assert_eq!(invalid[0].package_path(), ghost);

        let message = invalid[0].to_string();
        assert!(message.contains("named pipe (fifo)"), "got: {message}");
        // It was never parsed, so it must not be described as a parse failure --
        // a regression routing the refusal back through `YamlParse` would
        // otherwise satisfy every other assertion here.
        assert!(!message.contains("YAML"), "got: {message}");
        assert!(!message.contains("parsing"), "got: {message}");
    }

    // The single-package path, which wraps the refusal in a `PackageError` of
    // its own. It carries the same two negative assertions as the listing test
    // above, and they are the point: `PackageError::ParseError` renders as
    // "Parse error in package `ghost`", so routing a refused file through it
    // tells the user to go inspect YAML syntax in a file selfie never opened.
    #[test]
    fn get_package_refuses_a_fifo_spec() {
        let package_dir = PathBuf::from("/test/packages");
        let good = package_dir.join("good.yml");
        let ghost = package_dir.join("ghost.yml");
        let fs = fs_with_one_irregular_spec(&package_dir, &good, &ghost, "named pipe (fifo)");

        let error = YamlPackageRepository::new(fs, package_dir)
            .get_package("ghost")
            .expect_err("a fifo spec must be refused");

        let message = error.to_string();
        assert!(message.contains("named pipe (fifo)"), "got: {message}");
        assert!(message.contains("ghost"), "got: {message}");
        assert!(!message.contains("Parse error"), "got: {message}");
        assert!(!message.contains("YAML"), "got: {message}");
        assert!(!message.contains("parsing"), "got: {message}");
    }

    // The kind is carried through from the classifier rather than hardcoded to
    // the fifo case, which is the only one anyone reproduces.
    #[test]
    fn a_socket_spec_is_refused_too() {
        let package_dir = PathBuf::from("/test/packages");
        let good = package_dir.join("good.yml");
        let sock = package_dir.join("sock.yml");
        let fs = fs_with_one_irregular_spec(&package_dir, &good, &sock, "socket");

        let output = YamlPackageRepository::new(fs, package_dir)
            .list_packages()
            .expect("listing must succeed despite the socket");

        let invalid: Vec<_> = output.invalid_packages().collect();
        assert_eq!(invalid.len(), 1);
        let message = invalid[0].to_string();
        assert!(message.contains("socket"), "got: {message}");
        assert!(!message.contains("fifo"), "got: {message}");
    }

    // The fail-closed arm. Nothing returns a non-`IrregularTarget` refusal from
    // `irregular_target_refusal` today, so the input is synthetic -- the point is
    // that narrowing the guard to the one variant it expects would let a future
    // variant through and un-guard the read. `ghost.yml` is deliberately made
    // readable and parseable, so a guard that stops matching yields `Ok` and
    // fails this on its own assertion rather than on an unexpected mock call.
    #[test]
    fn a_non_irregular_refusal_still_refuses() {
        let package_dir = PathBuf::from("/test/packages");
        let ghost = package_dir.join("ghost.yml");

        let mut fs = MockFileSystem::default();
        fs.mock_path_exists(&package_dir, true);
        fs.mock_list_directory(&package_dir, &[&ghost]);
        fs.mock_read_file(
            &ghost,
            "name: ghost\n\nenvironments:\n  test-env:\n    install: true\n",
        );

        let ghost_owned = ghost.clone();
        fs.expect_irregular_target_refusal().returning(move |_| {
            Some(FileSystemError::SymlinkedTarget {
                path: ghost_owned.clone(),
                points_to: None,
            })
        });

        let output = YamlPackageRepository::new(fs, package_dir)
            .list_packages()
            .expect("listing must succeed");

        let invalid: Vec<_> = output.invalid_packages().collect();
        assert_eq!(output.valid_packages().count(), 0);
        assert_eq!(invalid.len(), 1);

        // Counts alone would also be satisfied by a regression that reclassified
        // this as an ordinary parse failure, so the wording is asserted too. It
        // must describe a *read* -- every `FileSystemError` refusal is worded for
        // the deploy side and says selfie will not write through a target, which
        // is false twice over here -- and must not name the path at all, since
        // every caller renders it beside one. Rendering the filesystem error
        // verbatim would print it twice.
        let message = invalid[0].to_string();
        assert!(message.contains("symlink"), "got: {message}");
        assert!(message.contains("will not read"), "got: {message}");
        assert!(!message.contains("write"), "got: {message}");
        assert!(!message.contains("target"), "got: {message}");
        assert_eq!(message.matches("ghost.yml").count(), 0, "got: {message}");
    }

    // The two tests below use a real fifo because `MockFileSystem` cannot block:
    // a mocked fixture passes against the unguarded code and proves nothing about
    // the hang. The deadline is what turns a regression into a failure rather
    // than a wedged suite; the read blocks on the spawned thread, so no async
    // timeout would fire.
    #[cfg(unix)]
    fn package_dir_with_a_fifo_spec() -> (TempDir, PathBuf) {
        let temp_dir = TempDir::new().unwrap();
        let package_dir = temp_dir.path().join("packages");
        std::fs::create_dir_all(&package_dir).unwrap();
        std::fs::write(
            package_dir.join("good.yml"),
            "name: good\n\nenvironments:\n  test-env:\n    install: true\n",
        )
        .unwrap();
        nix::unistd::mkfifo(
            &package_dir.join("ghost.yml"),
            nix::sys::stat::Mode::S_IRWXU,
        )
        .unwrap();
        (temp_dir, package_dir)
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_spec_does_not_block_list_packages() {
        use std::sync::mpsc;

        let (_temp_dir, package_dir) = package_dir_with_a_fifo_spec();
        let repo = YamlPackageRepository::new(RealFileSystem, package_dir);

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(repo.list_packages().map(|output| {
                (
                    output.valid_packages().count(),
                    output
                        .invalid_packages()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>(),
                )
            }));
        });

        let (valid, invalid) = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("enumerating a fifo spec must not block")
            .expect("listing must succeed despite the fifo");

        assert_eq!(valid, 1);
        assert_eq!(invalid.len(), 1);
        assert!(invalid[0].contains("named pipe (fifo)"), "got: {invalid:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_spec_does_not_block_get_package() {
        use std::sync::mpsc;

        let (_temp_dir, package_dir) = package_dir_with_a_fifo_spec();
        let repo = YamlPackageRepository::new(RealFileSystem, package_dir);

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(repo.get_package("ghost").map_err(|e| e.to_string()));
        });

        let message = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("reading a fifo spec must not block")
            .expect_err("a fifo spec must be refused");

        assert!(message.contains("named pipe (fifo)"), "got: {message}");
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

        // Mock the write operation
        fs.mock_write_file_no_follow(&package_path);

        let repo = YamlPackageRepository::new(fs, package_dir);

        // Test saving the package
        let result = repo.save_package(&package, &package_path);
        assert!(result.is_ok());
    }

    // A package YAML with one well-formed dotfile and one carrying `var:`.
    fn package_with_typo() -> Package {
        serde_saphyr::from_str(
            "name: creds\nenvironments:\n  test:\n    install: echo i\ndotfiles:\n  \
             - source: creds.tpl\n    target: ~/.creds\n    var:\n      k: op read x\n",
        )
        .expect("fixture must parse — the typo is a validation error, not a parse error")
    }

    // A file selfie could not read back is refused too, and this is the half that
    // was inverted: the guard passed exactly when selfie knew least, so the
    // rewrite dropped the key that made the file unreadable in the first place.
    //
    // `times(0)` is the assertion that matters. Refusing after the write would
    // already have destroyed the user's text.
    #[test]
    fn save_package_refuses_a_file_whose_top_level_could_not_be_read() {
        let yaml = "name: creds\nextra:\n  ? [a, b]\n  : v\nenvironments:\n  work:\n    install: \"echo i\"\n";
        let mut package: Package = serde_saphyr::from_str(yaml).expect("fixture must parse");
        package.set_source(PathBuf::from("/test/packages/creds.yml"), yaml.to_string());
        assert!(
            matches!(package.top_level_keys(), TopLevelKeys::Unchecked(_)),
            "fixture must reach the unchecked state, got: {:?}",
            package.top_level_keys()
        );

        let mut fs = MockFileSystem::default();
        let package_dir = PathBuf::from("/test/packages");
        fs.expect_write_file_no_follow().times(0);

        let repo = YamlPackageRepository::new(fs, package_dir.clone());
        let err = repo
            .save_package(&package, &package_dir.join("creds.yml"))
            .expect_err("a file selfie could not read back must not be rewritten");

        assert!(
            matches!(err, PackageRepoError::UncheckedTopLevel { .. }),
            "got: {err:?}"
        );
        // Not the found-keys variant: that one names keys, and here there are
        // none to name -- claiming otherwise would send the user looking.
        let rendered = err.to_string();
        assert!(
            !rendered.contains("unrecognized"),
            "the refusal must not claim it found keys, got: {err}"
        );
        // The parse failure has to come last. It renders as several lines of
        // source snippet with a `|` gutter, so a remedy placed after it arrives
        // as `  |. Edit /packages/creds.yml directly` and reads as snippet.
        let remedy = rendered
            .find("simplify it until selfie can read it")
            .expect("the refusal must give a remedy");
        let failure = rendered
            .find("The read failed with:")
            .expect("the refusal must carry the parse failure");
        assert!(
            remedy < failure,
            "the remedy must precede the parse failure, got: {rendered}"
        );
    }

    // The positive half of the same boundary: a top-level key still gets the
    // top-level variant. Narrowing the guard to fix the environment case could
    // otherwise have disabled it outright, and every other test here would pass.
    #[test]
    fn save_package_refuses_a_top_level_key_carrying_an_anchor_name() {
        let yaml = "name: creds\n_dotfiles:\n  - source: a\n    target: ~/.a\nenvironments:\n  work:\n    install: \"echo i\"\n";
        let mut package: Package = serde_saphyr::from_str(yaml).expect("fixture must parse");
        package.set_source(PathBuf::from("/test/packages/creds.yml"), yaml.to_string());

        let mut fs = MockFileSystem::default();
        let package_dir = PathBuf::from("/test/packages");

        fs.expect_write_file_no_follow().times(0);

        let repo = YamlPackageRepository::new(fs, package_dir.clone());
        let err = repo
            .save_package(&package, &package_dir.join("creds.yml"))
            .expect_err("a package with an unrecognized top-level key must not be rewritten");

        assert!(
            matches!(err, PackageRepoError::UnknownTopLevelFields { .. }),
            "got: {err:?}"
        );
        assert!(
            err.to_string().contains("_dotfiles"),
            "the refusal must name the key, got: {err}"
        );
    }

    // The fixture must carry its raw YAML, as a loaded package does. Without
    // `set_source` the top-level check has nothing to read and goes quiet, which
    // hid the guard ordering below entirely.
    //
    // selfie-nr4b, the same laundering one level up. `audt:` is not modeled, so
    // serializing from the struct drops the user's audit command with no message.
    //
    // `times(0)` is again the assertion that matters: refusing after the write
    // would already have destroyed the text.
    #[test]
    fn save_package_refuses_an_environment_carrying_an_unrecognized_key() {
        let package: Package = serde_saphyr::from_str(
            "name: creds\nenvironments:\n  work:\n    install: \"echo i\"\n    audt: \"brew audit myapp\"\n",
        )
        .expect("fixture must parse -- the typo is a validation error, not a parse error");

        let mut package = package;
        package.set_source(
            PathBuf::from("/test/packages/creds.yml"),
            "name: creds\nenvironments:\n  work:\n    install: \"echo i\"\n    audt: \"brew audit myapp\"\n"
                .to_string(),
        );

        let mut fs = MockFileSystem::default();
        let package_dir = PathBuf::from("/test/packages");
        let package_path = package_dir.join("creds.yml");

        fs.expect_write_file_no_follow().times(0);

        let repo = YamlPackageRepository::new(fs, package_dir);
        let err = repo
            .save_package(&package, &package_path)
            .expect_err("a package with an unrecognized environment key must not be rewritten");

        assert!(
            matches!(err, PackageRepoError::UnknownEnvironmentFields { .. }),
            "got: {err:?}"
        );
        // Not the dotfile-entry variant: that one says the rewrite would make an
        // entry deployable, which is not what is at stake for a setting.
        assert!(
            !err.to_string().contains("deployable"),
            "the refusal must not describe an entry, got: {err}"
        );
        // Names the environment, not just the key: a package may define several
        // and only one carries it.
        assert!(
            err.to_string().contains("environments.work.audt"),
            "the diagnostic must name the environment and the key, got: {err}"
        );
    }

    #[test]
    fn save_package_refuses_an_entry_carrying_an_unrecognized_key() {
        // Saving rewrites the file from the struct, which drops `var:` entirely.
        // The entry would stop being refused and the next apply would write the
        // unrendered template over the credentials target — so the write must not
        // happen at all. `times(0)` is the assertion that matters: refusing after
        // writing would already have destroyed the key.
        let mut fs = MockFileSystem::default();
        let package_dir = PathBuf::from("/test/packages");
        let package_path = package_dir.join("creds.yml");

        fs.expect_write_file_no_follow().times(0);

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

        fs.expect_write_file_no_follow().times(0);

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

        fs.expect_write_file_no_follow().times(0);

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

        fs.mock_write_file_no_follow(&package_path);

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

        // Mock the write to fail
        let expected = package_path.clone();
        fs.expect_write_file_no_follow()
            .withf(move |target, _| target.path() == expected)
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

    // Builds a repository whose write fails with `refusal`, and returns the
    // rendered error from `save_package`.
    fn save_package_refused_with(refusal: FileSystemError) -> String {
        let mut fs = MockFileSystem::default();
        let package_dir = PathBuf::from("/test/packages");
        let package_path = package_dir.join("test-package.yml");

        let package = Package::new(
            "test-package".to_string(),
            None,
            None,
            Vec::new(),
            None,
            HashMap::new(),
            package_path.clone(),
        );

        fs.expect_write_file_no_follow()
            .returning(move |_, _| Err(refusal.clone()));

        let repo = YamlPackageRepository::new(fs, package_dir);
        repo.save_package(&package, &package_path)
            .unwrap_err()
            .to_string()
    }

    // selfie-yw7i. A package file is written *into* selfie's own package
    // directory, so a refusal must describe that file -- but both refusal
    // variants say "target" in their `Display`, having been written for a dotfile
    // target. Rendering either verbatim tells someone who ran `selfie spec
    // update` that their "target" is a symlink.
    //
    // Asserted as an absence, which is what makes it bite: the natural regression
    // is a reversion to `PackageRepoError::FileSystemError`, whose passthrough
    // `Display` puts the word straight back.
    #[test]
    fn a_refused_package_path_does_not_call_it_a_target() {
        for refusal in [
            FileSystemError::SymlinkedTarget {
                path: PathBuf::from("/test/packages/test-package.yml"),
                points_to: Some(PathBuf::from("/tmp/planted")),
            },
            FileSystemError::IrregularTarget {
                path: PathBuf::from("/test/packages/test-package.yml"),
                kind: "named pipe (fifo)",
            },
        ] {
            let message = save_package_refused_with(refusal);
            assert!(
                !message.contains("target"),
                "refusal calls a package file a target: {message}"
            );
            assert!(
                message.contains("test-package.yml"),
                "refusal does not name the package file: {message}"
            );
        }
    }

    // The control for the test above: a failure that is *not* a refusal keeps the
    // filesystem's own message, so the mapping is narrow rather than swallowing
    // every write error into one rephrased variant.
    #[test]
    fn a_package_write_failure_that_is_not_a_refusal_is_passed_through() {
        let message = save_package_refused_with(FileSystemError::IoError(Arc::new(
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "Permission denied"),
        )));
        assert!(message.contains("Permission denied"), "got: {message}");
    }

    #[test]
    fn test_remove_package_success() {
        let mut fs = MockFileSystem::default();
        fs.mock_no_irregular_files();
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
        fs.mock_no_irregular_files();
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
