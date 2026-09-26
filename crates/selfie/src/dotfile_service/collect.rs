//! Collecting the packages an operation covers from the package repository and
//! the standalone dotfiles directory, and deciding what a name in both means.

use crate::package::{Package, port::PackageRepository};

use super::warning::{ApplyWarning, NameCollision};

/// Collect packages from both the main package repository and the optional
/// dotfiles repository, returning a combined list and any non-fatal warnings.
///
/// Warnings are returned rather than emitted because collection happens before
/// the event channel exists. Each caller sends them once its stream is up, and
/// [`ApplyWarning`] is what tells it which event each one is.
pub(super) fn collect_all_packages<R: PackageRepository>(
    package_repo: &R,
    dotfiles_repo: Option<&R>,
    dotfiles_directory_is_expected: bool,
) -> Result<(Vec<Package>, Vec<ApplyWarning>), crate::package::port::PackageListError> {
    collect_packages(
        package_repo,
        dotfiles_repo,
        NameCollision::PackagesWin,
        dotfiles_directory_is_expected,
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
/// be read or classified is refused either way.
pub(super) fn collect_packages<R: PackageRepository>(
    package_repo: &R,
    dotfiles_repo: Option<&R>,
    collision: NameCollision,
    dotfiles_directory_is_expected: bool,
) -> Result<(Vec<Package>, Vec<ApplyWarning>), crate::package::port::PackageListError> {
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
    let unloadable_package_names: std::collections::HashSet<String> = output
        .invalid_packages()
        .filter_map(|error| crate::package::spec_name_of(error.package_path()))
        .collect();
    let mut packages = output.valid_packages().cloned().collect::<Vec<_>>();

    let packages_count = packages.len();

    if let Some(dotfiles) = dotfiles_repo {
        match dotfiles.list_packages() {
            Ok(output) => {
                note_unparsable(&output, &mut warnings);
                packages.extend(output.valid_packages().cloned());
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
                }
                super::directory::UnlistedDotfilesDirectory::Unknown(error) => {
                    warnings.push(ApplyWarning::UncheckableRepository(error));
                }
            },
        }
    }

    // A packages/ spec claims its name over a dotfiles/ spec of the same
    // name. Names are spec file names with case folded, as package lookup
    // resolves them, so `bat.yml` and `Bat.yml` are one name. A packages/
    // spec that failed to parse still claims its name: deploying the
    // dotfiles/ copy in its place would apply a file the user did not mean.
    if collision == NameCollision::PackagesWin && packages.len() > packages_count {
        let claimed_by_packages: std::collections::HashSet<String> = packages[..packages_count]
            .iter()
            .filter_map(Package::spec_name)
            .collect();
        let mut seen_in_dotfiles = std::collections::HashSet::new();

        // A loaded packages/ spec is asked about first, so the warning names
        // the copy that is used. A dotfiles/ name repeated within dotfiles/
        // is its own case and does not blame packages/.
        let mut deduped_dotfiles = Vec::new();
        for pkg in packages.drain(packages_count..) {
            let Some(name) = pkg.spec_name() else {
                deduped_dotfiles.push(pkg);
                continue;
            };
            if claimed_by_packages.contains(&name) {
                warnings.push(ApplyWarning::Other(format!(
                    "Duplicate name '{name}' found in both packages/ and dotfiles/ — using \
                     the packages/ version"
                )));
            } else if unloadable_package_names.contains(&name) {
                warnings.push(ApplyWarning::Other(format!(
                    "Not using '{name}' from dotfiles/: packages/ has a spec by that name \
                     that could not be loaded"
                )));
            } else if !seen_in_dotfiles.insert(name.clone()) {
                warnings.push(ApplyWarning::Other(format!(
                    "Duplicate name '{name}' found twice in dotfiles/ — using the first"
                )));
            } else {
                deduped_dotfiles.push(pkg);
            }
        }
        packages.extend(deduped_dotfiles);
    }

    Ok((packages, warnings))
}
