//! Collecting the packages an operation covers from the package repository and
//! the standalone dotfiles directory, and deciding what a name in both means.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use crate::package::{Package, port::PackageRepository};

use super::warning::{ApplyWarning, CollectionRefusal, NameCollision};

/// What collecting the packages an operation covers found.
pub(super) struct Collected {
    pub(super) packages: Vec<Package>,
    /// What is worth saying, in the order it was found.
    pub(super) warnings: Vec<ApplyWarning>,
    /// What collection refused, in the order it found them. Each is one refusal
    /// for an operation over every package.
    pub(super) refusals: Vec<CollectionRefusal>,
    /// Names several files in one directory claim that matter to no operation over
    /// every package, since none of them declares dotfiles here. Not refusals, but
    /// none of the files is used, so a run asking for one by name still fails as
    /// ambiguous, as install does. Each with its files, sorted.
    pub(super) unrefused_ambiguities: Vec<(String, Vec<PathBuf>)>,
}

/// Collect packages from both the main package repository and the optional
/// dotfiles repository, returning a combined list, what is worth saying, and what
/// was refused.
///
/// Warnings are returned rather than emitted because collection happens before
/// the event channel exists. Each caller sends them once its stream is up, and
/// [`ApplyWarning`] is what tells it which event each one is.
pub(super) fn collect_all_packages<R: PackageRepository>(
    package_repo: &R,
    dotfiles_repo: Option<&R>,
    dotfiles_directory_is_expected: bool,
    environment: &str,
) -> Result<Collected, crate::package::port::PackageListError> {
    collect_packages(
        package_repo,
        dotfiles_repo,
        NameCollision::PackagesWin,
        dotfiles_directory_is_expected,
        environment,
    )
}

/// Collect from both repositories, deciding what a name in both means.
///
/// Deploying has to choose one, because two packages cannot both own a name.
/// Listing must not: both files exist, and a caller asking what is on disk is
/// asking about the files rather than about what would win.
///
/// `dotfiles_directory_is_expected` decides whether a dotfiles directory that is
/// **not there** is worth a warning, and nothing else. A directory that could not
/// be read or classified is refused either way. `environment` decides whether a
/// name several files claim matters to a deploying caller.
pub(super) fn collect_packages<R: PackageRepository>(
    package_repo: &R,
    dotfiles_repo: Option<&R>,
    collision: NameCollision,
    dotfiles_directory_is_expected: bool,
    environment: &str,
) -> Result<Collected, crate::package::port::PackageListError> {
    let mut warnings = Vec::new();

    // A package file that does not parse is dropped by `valid_packages`, and
    // silence there is dangerous for this command specifically: apply is what
    // people run, and a dotfile that quietly stops deploying surfaces much
    // later as an authentication failure nobody traces back to a typo. The
    // run would otherwise report success having done nothing at all.
    let note_unparsable = |output: &crate::package::port::ListPackagesOutput,
                           warnings: &mut Vec<ApplyWarning>| {
        for invalid in output.invalid_packages() {
            warnings.push(ApplyWarning::SkippedSpec(invalid.clone()));
        }
    };

    // The failure travels typed. Rendering it here hands the caller a bare
    // sentence, leaving it able to say only that loading failed -- not which of
    // the three fixes for a missing package directory applies.
    let output = package_repo.list_packages()?;
    note_unparsable(&output, &mut warnings);
    let unparsable_in_packages = unparsable_paths(&output);
    let mut packages = output.valid_packages().cloned().collect::<Vec<_>>();

    let mut dotfiles_packages = Vec::new();
    let mut unparsable_in_dotfiles = Vec::new();
    let mut refusals = Vec::new();

    if let Some(dotfiles) = dotfiles_repo {
        match dotfiles.list_packages() {
            Ok(output) => {
                note_unparsable(&output, &mut warnings);
                unparsable_in_dotfiles = unparsable_paths(&output);
                dotfiles_packages = output.valid_packages().cloned().collect();
            }
            Err(error) => match super::directory::UnlistedDotfilesDirectory::classify(
                error,
                dotfiles_directory_is_expected,
            ) {
                super::directory::UnlistedDotfilesDirectory::OrdinarilyAbsent => {}
                super::directory::UnlistedDotfilesDirectory::Absent { path, reason } => {
                    warnings.push(ApplyWarning::AbsentDotfilesDirectory { path, reason });
                }
                // Both refuse the run, because neither can claim the collection
                // is complete. They are pushed as different warnings so the
                // sentence a user reads says which one happened: one asserts a
                // directory is there and unreadable, the other cannot say even
                // that.
                super::directory::UnlistedDotfilesDirectory::Unlistable(error) => {
                    warnings.push(ApplyWarning::UnreadableRepository(error));
                    refusals.push(CollectionRefusal::UnreadableDotfilesDirectory);
                }
                super::directory::UnlistedDotfilesDirectory::Unknown(error) => {
                    warnings.push(ApplyWarning::UncheckableRepository(error));
                    refusals.push(CollectionRefusal::UnreadableDotfilesDirectory);
                }
            },
        }
    }

    if collision == NameCollision::KeepBoth {
        packages.extend(dotfiles_packages);
        return Ok(Collected {
            packages,
            warnings,
            refusals,
            unrefused_ambiguities: Vec::new(),
        });
    }

    // Several files in one directory claiming one name are refused as install
    // refuses them, whether each parses or not: deploying any of them picks one by
    // enumeration order, and deploying all of them lets the last overwrite the rest.
    // Asked only where one package must win a name; a listing keeps every file,
    // since every file is there.
    let packages_claims = claims(&packages, &unparsable_in_packages);
    let loaded_in_packages: HashSet<String> =
        packages.iter().filter_map(Package::spec_name).collect();
    let mut unrefused_ambiguities = Vec::new();
    for (name, paths) in &packages_claims {
        if paths.len() > 1 {
            if ambiguity_matters(paths, &packages, environment) {
                refusals.push(CollectionRefusal::AmbiguousName {
                    name: name.clone(),
                    paths: paths.clone(),
                });
            } else {
                unrefused_ambiguities.push((name.clone(), paths.clone()));
            }
        }
    }
    packages.retain(|pkg| !is_ambiguous(pkg, &packages_claims));
    // Every packages/ file that failed to parse could have been used, so each is a
    // refusal, one within an ambiguous name too: it needs its own fix.
    refusals.extend(
        unparsable_in_packages
            .iter()
            .cloned()
            .map(CollectionRefusal::UnloadableSpec),
    );

    // A name packages/ claims is settled in packages/: every dotfiles/ file of that
    // name is set aside with a warning and is never a refusal, whether it parsed,
    // and however many there are. Names are spec file names with case folded, as
    // package lookup resolves them. A packages/ spec that failed to parse, or whose
    // name several packages/ files claim, still claims it: deploying the dotfiles/
    // copy in its place would apply a file the user did not mean. The reason names
    // the first thing to fix in packages/: an ambiguity before a failed parse.
    let dotfiles_claims = claims(&dotfiles_packages, &unparsable_in_dotfiles);
    for (name, paths) in &dotfiles_claims {
        if let Some(in_packages) = packages_claims.get(name) {
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
            warnings.push(ApplyWarning::Named {
                name: name.clone(),
                message: why,
            });
        } else if paths.len() > 1 {
            if ambiguity_matters(paths, &dotfiles_packages, environment) {
                refusals.push(CollectionRefusal::AmbiguousName {
                    name: name.clone(),
                    paths: paths.clone(),
                });
            } else {
                unrefused_ambiguities.push((name.clone(), paths.clone()));
            }
        }
    }
    // A dotfiles/ file that failed to parse could have been used only where
    // packages/ does not claim its name.
    refusals.extend(
        unparsable_in_dotfiles
            .into_iter()
            .filter(|path| {
                crate::package::spec_name_of(path)
                    .is_none_or(|name| !packages_claims.contains_key(&name))
            })
            .map(CollectionRefusal::UnloadableSpec),
    );
    dotfiles_packages.retain(|pkg| {
        pkg.spec_name()
            .is_none_or(|name| !packages_claims.contains_key(&name))
            && !is_ambiguous(pkg, &dotfiles_claims)
    });
    packages.extend(dotfiles_packages);

    Ok(Collected {
        packages,
        warnings,
        refusals,
        unrefused_ambiguities,
    })
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
fn ambiguity_matters(paths: &[PathBuf], loaded: &[Package], environment: &str) -> bool {
    paths.iter().any(|path| {
        loaded
            .iter()
            .find(|pkg| pkg.path() == path)
            .is_none_or(|pkg| !pkg.dotfiles_for_environment(environment).is_empty())
    })
}

/// Whether more than one file in `claims` claims `pkg`'s name.
fn is_ambiguous(pkg: &Package, claims: &BTreeMap<String, Vec<PathBuf>>) -> bool {
    pkg.spec_name()
        .and_then(|name| claims.get(&name))
        .is_some_and(|paths| paths.len() > 1)
}
