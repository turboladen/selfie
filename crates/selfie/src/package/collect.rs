//! Collecting the packages an operation covers from the package repository and
//! the standalone dotfiles directory, and deciding what a name in both means.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use crate::fs::target::{HomeDir, TargetPath, expand_target_path};
use crate::package::{Package, ScopedEntry, SpecOrigin, event::BaseKind, port::PackageRepository};

pub(crate) mod directory;
pub(crate) mod unchecked;
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
    pub(crate) unrefused_ambiguities: Vec<(String, Vec<PathBuf>, BaseKind)>,
    /// Names of dotfiles/ specs left unused because packages/ claims the same
    /// name.
    pub(crate) set_aside: Vec<String>,
}

impl Collected {
    /// What collection refused, in the order it found them.
    pub(crate) fn refusals(&self) -> impl Iterator<Item = &CollectionRefusal> {
        self.findings.iter().filter_map(Finding::refusal)
    }

    /// Everything that left a spec unused, in the order found: what collection
    /// refused, then the names several files claim that matter to no operation
    /// over every package. A run that compares targets cannot compare with any of
    /// them.
    pub(crate) fn unusable(&self) -> Vec<CollectionRefusal> {
        let unrefused = self
            .unrefused_ambiguities
            .iter()
            .map(|(name, paths, base)| CollectionRefusal::AmbiguousName {
                name: name.clone(),
                paths: paths.clone(),
                base: *base,
            });
        self.refusals().cloned().chain(unrefused).collect()
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
    Ok(Listed::of(package_repo, dotfiles_repo)?
        .collect(dotfiles_directory_is_expected, environment))
}

/// Records the spec files both directories hold, as listed, before any name is
/// settled.
#[derive(Clone)]
pub(crate) struct Listed {
    packages: Vec<Package>,
    invalid_in_packages: Vec<crate::package::port::PackageParseError>,
    // Empty when one directory serves as both, which is listed once.
    dotfiles: Result<
        (Vec<Package>, Vec<crate::package::port::PackageParseError>),
        crate::package::port::PackageListError,
    >,
}

// Lists the dotfiles directory: the specs that parsed and the files that did not.
fn list_dotfiles<R: PackageRepository>(
    dotfiles_repo: &R,
) -> Result<
    (Vec<Package>, Vec<crate::package::port::PackageParseError>),
    crate::package::port::PackageListError,
> {
    dotfiles_repo.list_packages().map(|output| {
        (
            output.valid_packages().cloned().collect(),
            output.invalid_packages().cloned().collect(),
        )
    })
}

// The paths of the spec files that failed to parse.
fn unparsable_paths(invalid: &[crate::package::port::PackageParseError]) -> Vec<PathBuf> {
    invalid
        .iter()
        .map(|error| error.package_path().to_path_buf())
        .collect()
}

/// Reports that [`Listed::place`] could not place a spec.
#[derive(Debug, Clone, thiserror::Error)]
#[error(
    "selfie could not tell which spec directory {} belongs in, so it could not compare it with \
     the other specs",
    .0.display()
)]
pub(crate) struct UnplacedSpec(PathBuf);

impl Listed {
    /// Lists both directories.
    ///
    /// # Errors
    ///
    /// Returns the [`PackageListError`](crate::package::port::PackageListError)
    /// when the package directory could not be listed.
    pub(crate) fn of<R: PackageRepository>(
        package_repo: &R,
        dotfiles_repo: &R,
    ) -> Result<Self, crate::package::port::PackageListError> {
        // The failure travels typed. Rendering it here hands the caller a bare
        // sentence, leaving it able to say only that loading failed -- not which of
        // the three fixes for a missing package directory applies.
        let output = package_repo.list_packages()?;
        // One directory configured as both settings is read once, through the
        // package repository, so its files load as package specs. Reading it twice
        // lists every entry twice and makes every name collide with itself.
        let dotfiles = if same_directory(package_repo, dotfiles_repo) {
            Ok((Vec::new(), Vec::new()))
        } else {
            list_dotfiles(dotfiles_repo)
        };
        Ok(Self {
            packages: output.valid_packages().cloned().collect(),
            invalid_in_packages: output.invalid_packages().cloned().collect(),
            dotfiles,
        })
    }

    /// Lists the dotfiles directory alone, for a package directory that could not
    /// be listed.
    pub(crate) fn of_dotfiles_only<R: PackageRepository>(dotfiles_repo: &R) -> Self {
        Self {
            packages: Vec::new(),
            invalid_in_packages: Vec::new(),
            dotfiles: list_dotfiles(dotfiles_repo),
        }
    }

    /// Changes the listing to what it would be with `spec`'s file written: `spec`
    /// replaces the listed spec at its path, or is added in path order among the
    /// specs of the directory its origin names.
    ///
    /// # Errors
    ///
    /// Returns [`UnplacedSpec`], and leaves the listing as it was, in either of
    /// these cases:
    ///
    /// - `spec`'s origin names no spec directory.
    /// - `spec`'s origin names the dotfiles directory, and that directory could not
    ///   be listed.
    pub(crate) fn place(&mut self, spec: &Package) -> Result<(), UnplacedSpec> {
        let unplaced = || UnplacedSpec(spec.path().clone());
        let (specs, invalid) = match spec.origin() {
            SpecOrigin::PackageDirectory => (&mut self.packages, &mut self.invalid_in_packages),
            SpecOrigin::DotfilesDirectory => match &mut self.dotfiles {
                Ok((specs, invalid)) => (specs, invalid),
                Err(_) => return Err(unplaced()),
            },
            SpecOrigin::Memory => return Err(unplaced()),
        };
        // The file holds `spec` now, whatever failed to parse there before.
        invalid.retain(|error| error.package_path() != spec.path().as_path());
        // Matched by a scan, since a repository need not list its specs in order.
        // A new file goes before the first listed path that sorts after it, which
        // is where a sorted listing has it.
        match specs.iter().position(|listed| listed.path() == spec.path()) {
            Some(at) => specs[at] = spec.clone(),
            None => {
                let at = specs
                    .iter()
                    .position(|listed| listed.path() > spec.path())
                    .unwrap_or(specs.len());
                specs.insert(at, spec.clone());
            }
        }
        Ok(())
    }

    /// Settles every name and returns what collecting the listed specs found, as
    /// [`collect_all_packages`] does.
    pub(crate) fn collect(
        self,
        dotfiles_directory_is_expected: bool,
        environment: &str,
    ) -> Collected {
        let mut findings = Vec::new();
        let mut packages = self.packages;
        let invalid_in_packages = self.invalid_in_packages;
        let unparsable_in_packages = unparsable_paths(&invalid_in_packages);

        let mut dotfiles_packages = Vec::new();
        let mut invalid_in_dotfiles = Vec::new();
        match self.dotfiles {
            Ok((specs, invalid)) => {
                dotfiles_packages = specs;
                invalid_in_dotfiles = invalid;
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
                    Unlisted::Unreadable(unreadable) => findings.push(Finding::Refusal(
                        CollectionRefusal::UnreadableDotfilesDirectory(unreadable),
                    )),
                }
            }
        }
        let unparsable_in_dotfiles = unparsable_paths(&invalid_in_dotfiles);

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
                let ambiguity = (name.to_string(), paths.to_vec(), BaseKind::PackageDirectory);
                if ambiguity_matters(paths, &packages.iter().collect::<Vec<_>>(), environment) {
                    findings.push(Finding::Refusal(CollectionRefusal::AmbiguousName {
                        name: ambiguity.0,
                        paths: ambiguity.1,
                        base: ambiguity.2,
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
        findings.extend(invalid_in_packages.into_iter().map(|error| {
            Finding::Refusal(CollectionRefusal::UnloadableSpec {
                error,
                base: BaseKind::PackageDirectory,
            })
        }));

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
                let ambiguity = (
                    name.to_string(),
                    paths.to_vec(),
                    BaseKind::DotfilesDirectory,
                );
                if ambiguity_matters(
                    paths,
                    &dotfiles_packages.iter().collect::<Vec<_>>(),
                    environment,
                ) {
                    findings.push(Finding::Refusal(CollectionRefusal::AmbiguousName {
                        name: ambiguity.0,
                        paths: ambiguity.1,
                        base: ambiguity.2,
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
                Finding::Refusal(CollectionRefusal::UnloadableSpec {
                    error: invalid,
                    base: BaseKind::DotfilesDirectory,
                })
            });
        }
        dotfiles_packages.retain(|pkg| {
            pkg.spec_name()
                .is_none_or(|name| table.uses_dotfiles(&name))
        });
        packages.extend(dotfiles_packages);

        Collected {
            packages,
            findings,
            unrefused_ambiguities,
            set_aside,
        }
    }
}

/// Whether `packages` and `dotfiles` read one directory.
pub(crate) fn same_directory<R: PackageRepository>(packages: &R, dotfiles: &R) -> bool {
    // The directories are compared by identity, since a symlink, a firmlink, a
    // bind mount or a different spelling names the same one. A directory with no
    // identity matches nothing: it is not shown to be the other one.
    matches!(
        (packages.directory_identity(), dotfiles.directory_identity()),
        (Some(packages), Some(dotfiles)) if packages == dotfiles
    )
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

    /// The file used for `name` in the directory `origin` names, if one is.
    pub(crate) fn used_file(&self, origin: SpecOrigin, name: &str) -> Option<&T> {
        match origin {
            SpecOrigin::PackageDirectory => {
                self.uses_package(name).then(|| &self.packages[name][0])
            }
            SpecOrigin::DotfilesDirectory => {
                self.uses_dotfiles(name).then(|| &self.dotfiles[name][0])
            }
            SpecOrigin::Memory => None,
        }
    }

    /// The file of every name whose file is used, with the directory it is in:
    /// the package directory's, then the dotfiles directory's.
    pub(crate) fn used_files(&self) -> impl Iterator<Item = (SpecOrigin, &T)> {
        let packages = self.packages.keys().filter_map(|name| {
            let origin = SpecOrigin::PackageDirectory;
            Some((origin, self.used_file(origin, name)?))
        });
        let dotfiles = self.dotfiles.keys().filter_map(|name| {
            let origin = SpecOrigin::DotfilesDirectory;
            Some((origin, self.used_file(origin, name)?))
        });
        packages.chain(dotfiles)
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
            .is_none_or(|pkg| pkg.has_dotfiles_in(environment))
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

    // A file listed as unparsable that now holds the written spec no longer claims
    // its name twice, so the spec is used.
    #[test]
    fn a_file_written_over_one_listed_as_unparsable_is_used() {
        use crate::package::port::{PackageParseError, PackageParseKind};
        let cause = crate::yaml::parse::<Package>("name: [unclosed")
            .expect_err("fixture must fail to parse");
        let broken = PackageParseError::new(
            PathBuf::from("/packages/a.yml"),
            PackageParseKind::Yaml { cause },
        );
        let mut packages = MockPackageRepository::new();
        packages
            .expect_list_packages()
            .return_once(move || Ok(ListPackagesOutput(vec![Err(broken)])));
        packages.expect_directory_identity().return_const(None);
        let dotfiles = MockPackageRepository::listing_nothing();
        let written = PackageBuilder::default()
            .name("a")
            .environment("test", |b| b.install("true"))
            .path("/packages/a.yml")
            .origin(SpecOrigin::PackageDirectory)
            .build();

        let mut listed = Listed::of(&packages, &dotfiles).unwrap();
        listed.place(&written).unwrap();
        let collected = listed.collect(false, "test");

        let paths: Vec<&PathBuf> = collected.packages.iter().map(Package::path).collect();
        assert_eq!(paths, [&PathBuf::from("/packages/a.yml")]);
        assert!(collected.unusable().is_empty(), "nothing is refused");
    }

    // A repository need not list its specs in order, so the spec at the written
    // file's path is found however they are listed, and replaced rather than
    // listed twice.
    #[test]
    fn a_file_written_over_one_listed_out_of_order_replaces_it() {
        let spec = |name: &str| {
            PackageBuilder::default()
                .name(name)
                .environment("test", |b| b.install("true"))
                .path(format!("/packages/{name}.yml"))
                .origin(SpecOrigin::PackageDirectory)
                .build()
        };
        let listed = vec![Ok(spec("m")), Ok(spec("z")), Ok(spec("a"))];
        let mut packages = MockPackageRepository::new();
        packages
            .expect_list_packages()
            .return_once(move || Ok(ListPackagesOutput(listed)));
        packages.expect_directory_identity().return_const(None);
        let dotfiles = MockPackageRepository::listing_nothing();

        let mut listed = Listed::of(&packages, &dotfiles).unwrap();
        listed.place(&spec("a")).unwrap();
        let collected = listed.collect(false, "test");

        let paths: Vec<&PathBuf> = collected.packages.iter().map(Package::path).collect();
        assert_eq!(
            paths.iter().filter(|path| path.ends_with("a.yml")).count(),
            1,
            "{paths:?}"
        );
        assert_eq!(paths.len(), 3, "{paths:?}");
    }

    // A file written beside an unparsable file of the same name makes the name
    // ambiguous, as collection settles it, so neither file is used.
    #[test]
    fn a_file_written_beside_an_unparsable_one_of_its_name_is_not_used() {
        use crate::package::port::{PackageParseError, PackageParseKind};
        let cause = crate::yaml::parse::<Package>("name: [unclosed")
            .expect_err("fixture must fail to parse");
        let broken = PackageParseError::new(
            PathBuf::from("/packages/alpha.yaml"),
            PackageParseKind::Yaml { cause },
        );
        let mut packages = MockPackageRepository::new();
        packages
            .expect_list_packages()
            .return_once(move || Ok(ListPackagesOutput(vec![Err(broken)])));
        packages.expect_directory_identity().return_const(None);
        let dotfiles = MockPackageRepository::listing_nothing();
        let alpha = PackageBuilder::default()
            .name("alpha")
            .environment("test", |b| b.install("true"))
            .path("/packages/alpha.yml")
            .origin(SpecOrigin::PackageDirectory)
            .build();

        let mut listed = Listed::of(&packages, &dotfiles).unwrap();
        listed.place(&alpha).unwrap();
        let collected = listed.collect(false, "test");

        assert!(collected.packages.is_empty(), "{:?}", collected.packages);
        assert!(
            collected.unusable().iter().any(|refusal| matches!(
                refusal,
                CollectionRefusal::AmbiguousName { name, .. } if name == "alpha"
            )),
            "the name is refused as ambiguous"
        );
    }

    // A name used from each directory is listed once; an ambiguous name and a
    // set-aside dotfiles name are not used, so they are not listed.
    #[test]
    fn used_files_skip_ambiguous_and_set_aside_names() {
        let names = |list: &[&str]| list.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        let table = NameTable::of(
            names(&["a.yml", "b.yml", "b.yaml", "c.yml"]),
            names(&["c.yml", "d.yml", "e.yml", "e.yaml"]),
            |name: &String| Some(name.as_str()),
        );

        let used: Vec<(crate::package::SpecOrigin, &str)> = table
            .used_files()
            .map(|(origin, file)| (origin, file.as_str()))
            .collect();

        assert_eq!(
            used,
            [
                (crate::package::SpecOrigin::PackageDirectory, "a.yml"),
                (crate::package::SpecOrigin::PackageDirectory, "c.yml"),
                (crate::package::SpecOrigin::DotfilesDirectory, "d.yml"),
            ]
        );
    }

    // A repository holding one spec at `path`, its directory having the identity
    // `identity`.
    fn repo_holding(path: &str, identity: Option<u64>) -> MockPackageRepository {
        let package = PackageBuilder::default()
            .name("spec")
            .environment("test", |b| b.install("true"))
            .path(PathBuf::from(path))
            .build();
        let mut repo = MockPackageRepository::new();
        repo.expect_list_packages()
            .returning(move || Ok(ListPackagesOutput(vec![Ok(package.clone())])));
        repo.expect_directory_identity()
            .return_const(identity.map(|inode| crate::fs::FileIdentity::new(1, inode)));
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
    fn one_directory_identity_is_read_once() {
        let packages = repo_holding("/packages/spec.yml", Some(7));
        let dotfiles = repo_holding("/dotfiles/spec.yml", Some(7));

        assert_eq!(
            collected_paths(&packages, &dotfiles),
            [PathBuf::from("/packages/spec.yml")]
        );
    }

    // Neither directory has an identity, so neither is shown to be the other, and
    // both are read. Treating two unknowns as equal would drop the dotfiles
    // directory whenever both lookups failed.
    #[test]
    fn two_directories_without_identity_are_both_read() {
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
        repo.expect_directory_identity().return_const(None);
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
                CollectionRefusal::UnreadableDotfilesDirectory(
                    directory::UnreadableDotfilesDirectory::Uncheckable(_)
                )
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
