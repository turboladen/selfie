//! Collecting the packages an operation covers from the package repository and
//! the standalone dotfiles directory, and deciding what a name in both means.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use crate::package::{Package, port::PackageRepository};

pub(crate) mod directory;
pub(crate) mod warning;

use warning::{ApplyWarning, CollectionRefusal, Finding};

/// What collecting the packages an operation covers found.
pub(crate) struct Collected {
    pub(crate) packages: Vec<Package>,
    /// What is worth saying and what was refused, in the order it was found.
    pub(crate) findings: Vec<Finding>,
    /// Names several files in one directory claim that matter to no operation over
    /// every package, since none of them declares dotfiles here. Not refusals, but
    /// none of the files is used, so a run asking for one by name still fails as
    /// ambiguous, as install does. Each with its files, sorted.
    pub(crate) unrefused_ambiguities: Vec<(String, Vec<PathBuf>)>,
    /// Names of dotfiles/ specs left unused because packages/ claims the same
    /// name.
    pub(crate) set_aside: Vec<String>,
}

impl Collected {
    /// What collection refused, in the order it found them.
    pub(crate) fn refusals(&self) -> impl Iterator<Item = &CollectionRefusal> {
        self.findings.iter().filter_map(|finding| match finding {
            Finding::Refusal(refusal) => Some(refusal),
            Finding::Warning(_) => None,
        })
    }

    /// What collection found worth saying and refused nothing, in order.
    pub(crate) fn warnings(&self) -> impl Iterator<Item = &ApplyWarning> {
        self.findings.iter().filter_map(|finding| match finding {
            Finding::Warning(warning) => Some(warning),
            Finding::Refusal(_) => None,
        })
    }
}

/// Collect packages from both the main package repository and the dotfiles
/// repository, returning the packages a deploy would use and what collecting
/// them found. The caller sends each [`Finding`] once its event stream is up.
///
/// `dotfiles_directory_is_expected` decides whether a dotfiles directory that is
/// **not there** is worth a warning, and nothing else. A directory that could not
/// be read or classified is refused either way. `environment` decides whether a
/// name several files claim matters to a deploying caller.
// Findings are returned rather than emitted because collection happens before
// the event channel exists.
pub(crate) fn collect_all_packages<R: PackageRepository>(
    package_repo: &R,
    dotfiles_repo: &R,
    dotfiles_directory_is_expected: bool,
    environment: &str,
) -> Result<Collected, crate::package::port::PackageListError> {
    let mut findings = Vec::new();

    // The failure travels typed. Rendering it here hands the caller a bare
    // sentence, leaving it able to say only that loading failed -- not which of
    // the three fixes for a missing package directory applies.
    let output = package_repo.list_packages()?;
    let invalid_in_packages: Vec<_> = output.invalid_packages().cloned().collect();
    let unparsable_in_packages = unparsable_paths(&output);
    let mut packages = output.valid_packages().cloned().collect::<Vec<_>>();

    let mut dotfiles_packages = Vec::new();
    let mut invalid_in_dotfiles = Vec::new();
    let mut unparsable_in_dotfiles = Vec::new();

    // One directory configured as both settings is read once, through the
    // package repository, so its files load as package specs. Reading it twice
    // lists every entry twice and makes every name collide with itself.
    let same_directory = same_directory(package_repo, dotfiles_repo);

    if !same_directory {
        match dotfiles_repo.list_packages() {
            Ok(output) => {
                invalid_in_dotfiles = output.invalid_packages().cloned().collect();
                unparsable_in_dotfiles = unparsable_paths(&output);
                dotfiles_packages = output.valid_packages().cloned().collect();
            }
            Err(error) => {
                use directory::UnlistedDotfilesDirectory as Unlisted;

                // Whether it refuses is the classification's to say, so `config
                // validate` reaches the same verdict by asking it. Two refusals
                // rather than one so the sentence a user reads says which one
                // happened: one asserts a directory is there and unreadable, the
                // other cannot say even that.
                match Unlisted::classify(error, dotfiles_directory_is_expected) {
                    Unlisted::OrdinarilyAbsent => {}
                    Unlisted::Absent { path, reason } => {
                        findings.push(Finding::Warning(ApplyWarning::AbsentDotfilesDirectory {
                            path,
                            reason,
                        }))
                    }
                    Unlisted::Unlistable(error) => findings.push(Finding::Refusal(
                        CollectionRefusal::UnlistableDotfilesDirectory(error),
                    )),
                    Unlisted::Unknown(error) => findings.push(Finding::Refusal(
                        CollectionRefusal::UncheckableDotfilesDirectory(error),
                    )),
                }
            }
        }
    }

    // Refusals are found in this order: the dotfiles directory above, then
    // packages/ ambiguities, packages/ unparsable specs, dotfiles/ ambiguities and
    // dotfiles/ unparsable specs. Two consumers read the first refusal, the
    // orphan check's notice and `stop_on_error`, so the order names the cause.
    //
    // Several files in one directory claiming one name are refused as install
    // refuses them, whether each parses or not: deploying any of them picks one by
    // enumeration order, and deploying all of them lets the last overwrite the rest.
    let packages_claims = claims(&packages, &unparsable_in_packages);
    let loaded_in_packages: HashSet<String> =
        packages.iter().filter_map(Package::spec_name).collect();
    let mut unrefused_ambiguities = Vec::new();
    for (name, paths) in &packages_claims {
        if paths.len() > 1 {
            if ambiguity_matters(paths, &packages.iter().collect::<Vec<_>>(), environment) {
                findings.push(Finding::Refusal(CollectionRefusal::AmbiguousName {
                    name: name.clone(),
                    paths: paths.clone(),
                }));
            } else {
                unrefused_ambiguities.push((name.clone(), paths.clone()));
            }
        }
    }
    packages.retain(|pkg| !is_ambiguous(pkg, &packages_claims));
    // A package file that does not parse is dropped by `valid_packages`, and
    // silence there is dangerous: a dotfile that quietly stops deploying surfaces
    // much later as an authentication failure nobody traces back to a typo. Every
    // packages/ file that failed to parse could have been used, so each is a
    // refusal, one within an ambiguous name too: it needs its own fix.
    findings.extend(
        invalid_in_packages
            .into_iter()
            .map(|invalid| Finding::Refusal(CollectionRefusal::UnloadableSpec(invalid))),
    );

    // A name packages/ claims is settled in packages/: every dotfiles/ file of that
    // name is set aside with a warning and is never a refusal, whether it parsed,
    // and however many there are. Names are spec file names with case folded, as
    // package lookup resolves them. A packages/ spec that failed to parse, or whose
    // name several packages/ files claim, still claims it: deploying the dotfiles/
    // copy in its place would apply a file the user did not mean. The reason names
    // the first thing to fix in packages/: an ambiguity before a failed parse.
    let dotfiles_claims = claims(&dotfiles_packages, &unparsable_in_dotfiles);
    let mut set_aside = Vec::new();
    for (name, paths) in &dotfiles_claims {
        if let Some(in_packages) = packages_claims.get(name) {
            // The packages/ spec may deploy nothing this one does, so nothing
            // collected stands in for this file's entries.
            set_aside.push(name.clone());
            let why = if in_packages.len() > 1 {
                format!(
                    "Not using '{name}' from dotfiles/: packages/ has more than one spec by that \
                     name"
                )
            } else if !loaded_in_packages.contains(name) {
                format!(
                    "Not using '{name}' from dotfiles/: packages/ has a spec by that name that \
                     could not be loaded"
                )
            } else {
                format!(
                    "Duplicate name '{name}' found in both packages/ and dotfiles/ — using the \
                     packages/ version"
                )
            };
            findings.push(Finding::Warning(ApplyWarning::Named {
                name: name.clone(),
                message: why,
            }));
        } else if paths.len() > 1 {
            if ambiguity_matters(
                paths,
                &dotfiles_packages.iter().collect::<Vec<_>>(),
                environment,
            ) {
                findings.push(Finding::Refusal(CollectionRefusal::AmbiguousName {
                    name: name.clone(),
                    paths: paths.clone(),
                }));
            } else {
                unrefused_ambiguities.push((name.clone(), paths.clone()));
            }
        }
    }
    // A dotfiles/ file that failed to parse could have been used only where
    // packages/ does not claim its name; under a claimed name nothing would have
    // used it, so it is only worth a word.
    for invalid in invalid_in_dotfiles {
        let claimed = crate::package::spec_name_of(invalid.package_path())
            .is_some_and(|name| packages_claims.contains_key(&name));
        findings.push(if claimed {
            Finding::Warning(ApplyWarning::SkippedSpec(invalid))
        } else {
            Finding::Refusal(CollectionRefusal::UnloadableSpec(invalid))
        });
    }
    dotfiles_packages.retain(|pkg| {
        pkg.spec_name()
            .is_none_or(|name| !packages_claims.contains_key(&name))
            && !is_ambiguous(pkg, &dotfiles_claims)
    });
    packages.extend(dotfiles_packages);

    Ok(Collected {
        packages,
        findings,
        unrefused_ambiguities,
        set_aside,
    })
}

/// Whether `packages` and `dotfiles` read one directory.
pub(crate) fn same_directory<R: PackageRepository>(packages: &R, dotfiles: &R) -> bool {
    // The directories are compared as the file system resolves them, since a
    // symlink or a different spelling names the same one. A directory that
    // cannot be resolved matches nothing: it is not shown to be the other one.
    matches!(
        (packages.resolved_directory(), dotfiles.resolved_directory()),
        (Some(packages), Some(dotfiles)) if packages == dotfiles
    )
}

/// The spec files in `output` that failed to parse.
fn unparsable_paths(output: &crate::package::port::ListPackagesOutput) -> Vec<PathBuf> {
    output
        .invalid_packages()
        .map(|error| error.package_path().to_path_buf())
        .collect()
}

/// Every file in one directory, loaded or not, grouped by the name it claims.
fn claims(packages: &[Package], unparsable: &[PathBuf]) -> BTreeMap<String, Vec<PathBuf>> {
    let files = packages
        .iter()
        .map(|pkg| pkg.path().to_path_buf())
        .chain(unparsable.iter().cloned());
    crate::package::group_by_spec_name(files, |path: &PathBuf| {
        path.file_name().and_then(|name| name.to_str())
    })
}

/// Whether a name the files at `paths` all claim matters to a run in
/// `environment`: one of them failed to parse, so what it declares is unknown, or
/// one of them has dotfiles there. `loaded` holds the files of that directory
/// that parsed.
// A pair of install-only specs is left to install, which refuses the name
// itself: apply and drift would deploy and check nothing from either file.
pub(crate) fn ambiguity_matters(paths: &[PathBuf], loaded: &[&Package], environment: &str) -> bool {
    paths.iter().any(|path| {
        loaded
            .iter()
            .find(|pkg| pkg.path() == path)
            .is_none_or(|pkg| {
                !pkg.dotfiles().is_empty()
                    || pkg
                        .environments()
                        .get(environment)
                        .is_some_and(|env| !env.dotfiles().is_empty())
            })
    })
}

/// Whether more than one file in `claims` claims `pkg`'s name.
fn is_ambiguous(pkg: &Package, claims: &BTreeMap<String, Vec<PathBuf>>) -> bool {
    pkg.spec_name()
        .and_then(|name| claims.get(&name))
        .is_some_and(|paths| paths.len() > 1)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::package::{
        PackageBuilder,
        port::{ListPackagesOutput, MockPackageRepository},
    };

    // A repository holding one spec at `path`, resolving its directory to
    // `resolved`.
    fn repo_holding(path: &str, resolved: Option<&str>) -> MockPackageRepository {
        let package = PackageBuilder::default()
            .name("spec")
            .environment("test", |b| b.install("true"))
            .path(PathBuf::from(path))
            .build();
        let mut repo = MockPackageRepository::new();
        repo.expect_list_packages()
            .returning(move || Ok(ListPackagesOutput(vec![Ok(package.clone())])));
        repo.expect_resolved_directory()
            .return_const(resolved.map(PathBuf::from));
        repo
    }

    fn collected_paths(
        packages: &MockPackageRepository,
        dotfiles: &MockPackageRepository,
    ) -> Vec<PathBuf> {
        let collected = collect_all_packages(packages, dotfiles, false, "test").unwrap();
        collected
            .packages
            .iter()
            .map(|pkg| pkg.path().to_path_buf())
            .collect()
    }

    #[test]
    fn one_resolved_directory_is_read_once() {
        let packages = repo_holding("/packages/spec.yml", Some("/real"));
        let dotfiles = repo_holding("/dotfiles/spec.yml", Some("/real"));

        assert_eq!(
            collected_paths(&packages, &dotfiles),
            [PathBuf::from("/packages/spec.yml")]
        );
    }

    // Neither directory resolves, so neither is shown to be the other, and both
    // are read. Treating two unknowns as equal would drop the dotfiles directory
    // whenever resolution failed on both sides.
    #[test]
    fn two_unresolved_directories_are_both_read() {
        let packages = repo_holding("/packages/spec.yml", None);
        let dotfiles = repo_holding("/dotfiles/other.yml", None);

        assert_eq!(
            collected_paths(&packages, &dotfiles),
            [
                PathBuf::from("/packages/spec.yml"),
                PathBuf::from("/dotfiles/other.yml")
            ]
        );
    }

    // A dotfiles repository whose listing failed with the directory in `state`.
    fn unlisted_dotfiles(state: crate::fs::DirectoryState) -> MockPackageRepository {
        let mut repo = MockPackageRepository::new();
        repo.expect_list_packages().returning(move || {
            Err(crate::package::port::PackageListError::new(
                PathBuf::from("/dotfiles"),
                state.clone(),
            ))
        });
        repo.expect_resolved_directory().return_const(None);
        repo
    }

    fn collected_over(dotfiles: &MockPackageRepository) -> Collected {
        let packages = repo_holding("/packages/spec.yml", None);
        collect_all_packages(&packages, dotfiles, true, "test").unwrap()
    }

    // A loop gives nothing to classify, so what is behind it is unknown and may
    // hold standalone dotfiles. The collection cannot claim to be complete.
    #[test]
    fn a_looping_dotfiles_directory_refuses_the_collection() {
        let dotfiles = unlisted_dotfiles(crate::fs::DirectoryState::Unknown(std::sync::Arc::new(
            std::io::Error::other("Too many levels of symbolic links"),
        )));

        let collected = collected_over(&dotfiles);

        assert!(matches!(
            collected.findings.as_slice(),
            [Finding::Refusal(
                CollectionRefusal::UncheckableDotfilesDirectory(_)
            )]
        ));
    }

    // Its control: a directory that is simply not there holds nothing, so it is
    // a warning and no refusal.
    #[test]
    fn an_absent_dotfiles_directory_does_not_refuse_the_collection() {
        let dotfiles = unlisted_dotfiles(crate::fs::DirectoryState::Absent(
            crate::fs::AbsentReason::Empty,
        ));

        let collected = collected_over(&dotfiles);

        assert!(matches!(
            collected.findings.as_slice(),
            [Finding::Warning(
                ApplyWarning::AbsentDotfilesDirectory { .. }
            )]
        ));
    }
}
