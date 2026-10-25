//! Collecting the packages an operation covers from the package repository and
//! the standalone dotfiles directory, and deciding what a name in both means.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use crate::fs::target::{HomeDir, TargetPath, expand_target_path};
use crate::package::{Package, ScopedEntry, SpecOrigin, port::PackageRepository};

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
    let table = NameTable::of(
        packages
            .iter()
            .map(|pkg| pkg.path().clone())
            .chain(unparsable_in_packages.iter().cloned()),
        dotfiles_packages
            .iter()
            .map(|pkg| pkg.path().clone())
            .chain(unparsable_in_dotfiles.iter().cloned()),
        |path: &PathBuf| path.file_name().and_then(|name| name.to_str()),
    );
    let loaded_in_packages: HashSet<String> =
        packages.iter().filter_map(Package::spec_name).collect();
    let mut unrefused_ambiguities = Vec::new();
    for (name, _, claim) in table.package_claims() {
        if let Claim::Ambiguous(paths) = claim {
            let ambiguity = (name.to_string(), paths.to_vec());
            if ambiguity_matters(paths, &packages.iter().collect::<Vec<_>>(), environment) {
                findings.push(Finding::Refusal(CollectionRefusal::AmbiguousName {
                    name: ambiguity.0,
                    paths: ambiguity.1,
                }));
            } else {
                unrefused_ambiguities.push(ambiguity);
            }
        }
    }
    packages.retain(|pkg| pkg.spec_name().is_none_or(|name| table.uses_package(&name)));
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
    let mut set_aside = Vec::new();
    for (name, _, claim) in table.dotfiles_claims() {
        if let Claim::SetAside(in_packages) = claim {
            // The packages/ spec may deploy nothing this one does, so nothing
            // collected stands in for this file's entries.
            set_aside.push(name.to_string());
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
                name: name.to_string(),
                message: why,
            }));
        } else if let Claim::Ambiguous(paths) = claim {
            let ambiguity = (name.to_string(), paths.to_vec());
            if ambiguity_matters(
                paths,
                &dotfiles_packages.iter().collect::<Vec<_>>(),
                environment,
            ) {
                findings.push(Finding::Refusal(CollectionRefusal::AmbiguousName {
                    name: ambiguity.0,
                    paths: ambiguity.1,
                }));
            } else {
                unrefused_ambiguities.push(ambiguity);
            }
        }
    }
    // A dotfiles/ file that failed to parse could have been used only where
    // packages/ does not claim its name; under a claimed name nothing would have
    // used it, so it is only worth a word.
    for invalid in invalid_in_dotfiles {
        let claimed = crate::package::spec_name_of(invalid.package_path())
            .is_some_and(|name| table.is_set_aside(&name));
        findings.push(if claimed {
            Finding::Warning(ApplyWarning::SkippedSpec(invalid))
        } else {
            Finding::Refusal(CollectionRefusal::UnloadableSpec(invalid))
        });
    }
    dotfiles_packages.retain(|pkg| {
        pkg.spec_name()
            .is_none_or(|name| table.uses_dotfiles(&name))
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

/// What each folded name means across the two spec directories, from the files'
/// names alone: which files claim it in each directory. A name several files of
/// one directory claim is ambiguous, and a dotfiles-directory file under a name
/// the package directory claims is set aside.
pub(crate) struct NameTable<T> {
    packages: BTreeMap<String, Vec<T>>,
    dotfiles: BTreeMap<String, Vec<T>>,
}

/// What the table says about one name in one directory.
pub(crate) enum Claim<'t, T> {
    /// One file claims it, and that file is used.
    One,
    /// Several files of the directory claim it, so none of them is used. Carries
    /// the files, sorted.
    Ambiguous(&'t [T]),
    /// A dotfiles-directory name the package directory also claims, so no
    /// dotfiles file of it is used. Carries the package directory's files.
    SetAside(&'t [T]),
}

impl<T: Ord> NameTable<T> {
    /// The table of `packages` and `dotfiles`, whose file names `file_name` gives.
    /// An item naming no spec is left out.
    pub(crate) fn of(
        packages: impl IntoIterator<Item = T>,
        dotfiles: impl IntoIterator<Item = T>,
        file_name: impl Fn(&T) -> Option<&str>,
    ) -> Self {
        Self {
            packages: crate::package::group_by_spec_name(packages, &file_name),
            dotfiles: crate::package::group_by_spec_name(dotfiles, &file_name),
        }
    }

    /// Every name the package directory claims, in order, with its files and
    /// what the table says about it.
    pub(crate) fn package_claims(&self) -> impl Iterator<Item = (&str, &[T], Claim<'_, T>)> {
        self.packages.iter().map(|(name, files)| {
            let claim = if files.len() > 1 {
                Claim::Ambiguous(files)
            } else {
                Claim::One
            };
            (name.as_str(), files.as_slice(), claim)
        })
    }

    /// Every name the dotfiles directory claims, in order, with its files and
    /// what the table says about it. A set-aside name is never ambiguous.
    pub(crate) fn dotfiles_claims(&self) -> impl Iterator<Item = (&str, &[T], Claim<'_, T>)> {
        self.dotfiles.iter().map(|(name, files)| {
            let claim = match self.set_aside_by(name) {
                Some(by) => Claim::SetAside(by),
                None if files.len() > 1 => Claim::Ambiguous(files),
                None => Claim::One,
            };
            (name.as_str(), files.as_slice(), claim)
        })
    }

    /// Whether a dotfiles-directory file of `name` is set aside, because the
    /// package directory claims the name.
    pub(crate) fn is_set_aside(&self, name: &str) -> bool {
        self.set_aside_by(name).is_some()
    }

    // The package directory's files that set a dotfiles name aside, the one rule
    // every set-aside answer comes from.
    fn set_aside_by(&self, name: &str) -> Option<&[T]> {
        self.packages.get(name).map(Vec::as_slice)
    }

    /// Whether the package directory's file of `name` is the one used.
    pub(crate) fn uses_package(&self, name: &str) -> bool {
        self.packages
            .get(name)
            .is_some_and(|files| files.len() == 1)
    }

    /// Whether the dotfiles directory's file of `name` is the one used.
    pub(crate) fn uses_dotfiles(&self, name: &str) -> bool {
        !self.is_set_aside(name)
            && self
                .dotfiles
                .get(name)
                .is_some_and(|files| files.len() == 1)
    }

    /// Every ambiguous name with its files: the package directory's first, then
    /// the dotfiles directory's that are not set aside. Whether a command refuses
    /// each one is its own verdict.
    pub(crate) fn ambiguous(&self) -> impl Iterator<Item = (&str, &[T])> {
        self.package_claims()
            .chain(self.dotfiles_claims())
            .filter_map(|(name, _, claim)| match claim {
                Claim::Ambiguous(files) => Some((name, files)),
                Claim::One | Claim::SetAside(_) => None,
            })
    }

    /// Every name either directory claims.
    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.packages
            .keys()
            .chain(self.dotfiles.keys())
            .map(String::as_str)
    }
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

/// Entries of two or more specs that deploy to one target in one environment, in
/// order of target. Apply and drift refuse every one; validation reports them.
pub(crate) struct CrossSpecCollisions<'c> {
    groups: Vec<CrossSpecGroup<'c>>,
}

/// One target and the entries of several specs that deploy to it.
pub(crate) struct CrossSpecGroup<'c> {
    target: TargetPath,
    members: Vec<(&'c Package, ScopedEntry<'c>)>,
}

impl<'c> CrossSpecCollisions<'c> {
    /// The cross-spec collisions among `members`, each spec with the entries it
    /// deploys in `environment`, targets compared once expanded against `home`.
    // Asked of every collected package, whichever ones the run deploys, since an
    // entry of the one package a run names can still overwrite another spec's file.
    pub(crate) fn of<H: HomeDir + ?Sized>(
        members: impl IntoIterator<Item = (&'c Package, Vec<ScopedEntry<'c>>)>,
        home: &H,
        environment: &str,
    ) -> Self {
        let mut by_target: BTreeMap<String, CrossSpecGroup<'c>> = BTreeMap::new();
        // A package refused whole takes no part: nothing of it deploys, and its
        // entries may not be the ones it means. An entry refused for a reason of
        // its own still does, so every entry sharing the target gets one answer.
        for (package, entries) in members
            .into_iter()
            .filter(|(package, _)| !package.is_refused(environment))
        {
            for scoped in entries {
                let target = expand_target_path(home, scoped.entry.target());
                by_target
                    .entry(target.state_key())
                    .or_insert_with(|| CrossSpecGroup {
                        target,
                        members: Vec::new(),
                    })
                    .members
                    .push((package, scoped));
            }
        }
        let groups = by_target
            .into_values()
            .filter(|group| {
                group
                    .members
                    .iter()
                    .any(|(package, _)| package.path() != group.members[0].0.path())
            })
            .collect();
        Self { groups }
    }

    /// Every collision, in order of target.
    pub(crate) fn groups(&self) -> &[CrossSpecGroup<'c>] {
        &self.groups
    }

    /// Every collision, in order of target, owned.
    pub(crate) fn into_groups(self) -> Vec<CrossSpecGroup<'c>> {
        self.groups
    }
}

impl<'c> CrossSpecGroup<'c> {
    /// The target every member deploys to.
    pub(crate) fn target(&self) -> &TargetPath {
        &self.target
    }

    /// Every member entry with its spec, in the order collected.
    pub(crate) fn members(&self) -> &[(&'c Package, ScopedEntry<'c>)] {
        &self.members
    }

    /// Whether an entry of the spec at `package`'s file is a member.
    pub(crate) fn involves(&self, package: &Package) -> bool {
        self.members
            .iter()
            .any(|(member, _)| member.path() == package.path())
    }

    // Matched by the spec's file and the entry's place in it, since a run over one
    // named package holds its own copy of that package.
    pub(crate) fn contains(&self, package: &Package, scoped: &ScopedEntry<'_>) -> bool {
        self.members
            .iter()
            .any(|(member, entry)| member.path() == package.path() && entry.is(scoped))
    }

    /// Every member, as `package 'bat' (dotfiles[0])`, in the order collected, and
    /// the target they share, as one clause.
    pub(crate) fn describe(&self) -> String {
        let names: Vec<String> = self
            .members
            .iter()
            .map(|(package, scoped)| format!("{} ({})", spec_label(package), scoped.field()))
            .collect();
        crate::package::deploy_to_clause(&names, &self.target)
    }
}

/// How a refusal names a spec: by the name lookup resolves it by, and as a package
/// or a standalone dotfile spec.
pub(crate) fn spec_label(package: &Package) -> String {
    let name = package
        .spec_name()
        .unwrap_or_else(|| package.name().to_string());
    match package.origin() {
        SpecOrigin::DotfilesDirectory => format!("standalone dotfile '{name}'"),
        SpecOrigin::PackageDirectory | SpecOrigin::Memory => format!("package '{name}'"),
    }
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
