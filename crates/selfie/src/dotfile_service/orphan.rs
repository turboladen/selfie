//! Deploy-state records whose target no entry deploys to any more.
//!
//! selfie never deletes or changes an orphaned file. It reports one that is
//! still there, and a run that writes drops the record of one that is gone.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::{
    fs::{
        filesystem::{AbsentReason, DirectoryState, FileSystem},
        target::{HomeDir, TargetRejection, deploy_target},
    },
    package::{Package, event::EventSender},
};

use super::state::{DeployEntry, DeployState};
use super::warning::CollectionRefusal;

/// Why the targets the collected packages produce may not be all of them, so
/// that a record missing from them may still belong to an entry.
#[derive(Clone)]
pub(crate) enum Shortfall {
    /// Collection refused something, and this is the first thing it refused.
    Refused(CollectionRefusal),
    /// A dotfiles/ spec of this name was left unused because packages/ claims
    /// the name.
    SetAside(String),
    /// A configured dotfiles directory is not there.
    AbsentDotfilesDirectory,
    /// A package was refused whole, so its entries were never read.
    RefusedPackage(String),
    /// A `~` target could not be resolved for want of a home directory.
    NoHome,
    /// No collected package deploys anything in this environment.
    NothingProduced,
}

impl std::fmt::Display for Shortfall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(CollectionRefusal::UnloadableSpec(path)) => {
                write!(f, "spec '{}' could not be loaded", path.display())
            }
            Self::Refused(CollectionRefusal::AmbiguousName { name, .. }) => {
                write!(f, "several spec files claim the name '{name}'")
            }
            Self::Refused(CollectionRefusal::UnreadableDotfilesDirectory) => {
                f.write_str("the dotfiles directory could not be read")
            }
            Self::SetAside(name) => write!(
                f,
                "dotfiles/ spec '{name}' is unused because packages/ claims its name"
            ),
            Self::AbsentDotfilesDirectory => f.write_str("the dotfiles directory is not there"),
            Self::RefusedPackage(name) => write!(f, "package '{name}' was refused"),
            Self::NoHome => f.write_str("the home directory could not be determined"),
            Self::NothingProduced => {
                f.write_str("no package deploys a dotfile in this environment")
            }
        }
    }
}

/// Every package collection found, whatever the command covers, and what it
/// knows to be missing from them. Orphans are judged against all of them.
pub(super) struct Catalog<'a> {
    pub(super) packages: &'a [Package],
    pub(super) shortfall: Option<Shortfall>,
}

/// Every target the collected packages deploy to in one environment, with the
/// spec names of the packages that name it, and whether that is all of them.
struct Survey {
    produced: BTreeMap<String, BTreeSet<Option<String>>>,
    shortfall: Option<Shortfall>,
}

impl Survey {
    /// Survey `packages` for `environment`. `collection` is what collecting them
    /// already knows to be missing.
    fn of<H: HomeDir + ?Sized>(
        home: &H,
        packages: &[Package],
        environment: &str,
        collection: Option<Shortfall>,
    ) -> Self {
        let mut shortfall = collection;
        let mut produced: BTreeMap<String, BTreeSet<Option<String>>> = BTreeMap::new();
        for package in packages {
            // A package refused whole may hold entries that were never read, as
            // when a misspelled key swallows its `dotfiles` list.
            if package.spec_refusal(environment).is_some() {
                shortfall
                    .get_or_insert_with(|| Shortfall::RefusedPackage(package.name().to_string()));
                continue;
            }
            // Every entry, refused or secret-bearing alike: each still names a
            // target the user means selfie to manage.
            for scoped in package.effective_dotfiles(Some(environment)) {
                match deploy_target(home, scoped.entry.target()) {
                    Ok(target) => {
                        produced
                            .entry(target.state_key())
                            .or_default()
                            .insert(package.spec_name());
                    }
                    // This entry's target may be the one a record names, and
                    // nothing here can say which.
                    Err(TargetRejection::NoHome) => {
                        shortfall.get_or_insert(Shortfall::NoHome);
                    }
                    // The target rule refuses this form, so no deploy recorded it.
                    Err(_) => {}
                }
            }
        }
        // An empty or mistyped package directory collects no package and refuses
        // nothing. Judged by it, every record would be orphaned and every gone
        // one dropped.
        if produced.is_empty() {
            shortfall.get_or_insert(Shortfall::NothingProduced);
        }
        Self {
            produced,
            shortfall,
        }
    }

    /// Records in `state` no entry produces, belonging to `owner` when one is
    /// given, sorted by target.
    fn unproduced<'s>(
        &self,
        state: &'s DeployState,
        owner: Option<&str>,
    ) -> Vec<(&'s str, &'s DeployEntry)> {
        let mut found: Vec<_> = state
            .entries()
            .iter()
            .filter(|(target, _)| !self.produced.contains_key(target.as_str()))
            // A relative key names no one file: resolved here, it would be
            // resolved against the working directory.
            .filter(|(target, _)| Path::new(target.as_str()).is_absolute())
            .filter(|(_, entry)| owner.is_none_or(|owner| entry.package() == Some(owner)))
            .map(|(target, entry)| (target.as_str(), entry))
            .collect();
        found.sort_by_key(|(target, _)| *target);
        found
    }

    /// Records in `state` whose package does not produce their target while
    /// exactly one package does, paired with that package's spec name. Limited
    /// to `owner`'s targets when one is given.
    fn attributions(&self, state: &DeployState, owner: Option<&str>) -> Vec<(String, String)> {
        let mut found: Vec<(String, String)> = state
            .entries()
            .iter()
            .filter_map(|(target, entry)| {
                let owners = self.produced.get(target)?;
                if owners
                    .iter()
                    .any(|owner| owner.as_deref() == entry.package())
                {
                    return None;
                }
                // A target two packages name could be either's, so it is left
                // as recorded rather than guessed.
                let (1, Some(Some(name))) = (owners.len(), owners.first()) else {
                    return None;
                };
                owner
                    .is_none_or(|owner| owner == name)
                    .then(|| (target.clone(), name.clone()))
            })
            .collect();
        found.sort();
        found
    }

    /// Every produced target as the file system resolves it, for telling one
    /// file under two spellings apart from two files.
    fn resolved<F: FileSystem>(&self, filesystem: &F) -> BTreeSet<PathBuf> {
        self.produced
            .keys()
            .filter_map(|target| resolved_for_comparison(filesystem, target))
            .collect()
    }
}

/// What checking for orphans found and would change.
#[derive(Default)]
pub(super) struct Findings {
    /// How many orphans were reported, their files still there.
    pub(super) reported: usize,
    /// Orphans whose file is gone, whose records a run that writes drops.
    pub(super) gone: Vec<String>,
    /// Records to attribute, as `(target, package)`.
    pub(super) attributions: Vec<(String, String)>,
}

impl Findings {
    /// Apply the drops and attributions to `state`. Returns whether it changed.
    pub(super) fn settle(&self, state: &mut DeployState) -> bool {
        let mut changed = false;
        for target in &self.gone {
            changed |= state.remove(target);
        }
        for (target, package) in &self.attributions {
            changed |= state.attribute(target, package);
        }
        changed
    }
}

/// Check `state` for orphans against every package in `catalog`, reporting each
/// one whose file is still there and, when the catalog may be short, warning
/// once instead.
///
/// `owner` limits the check to one package's records, for an apply of that
/// package alone. Sends nothing when there is nothing to say, and changes
/// nothing: the caller decides whether to [`settle`](Findings::settle).
pub(super) async fn check<F: FileSystem>(
    filesystem: &F,
    catalog: Catalog<'_>,
    environment: &str,
    state: &DeployState,
    owner: Option<&str>,
    sender: &EventSender,
) -> Findings {
    let survey = &Survey::of(filesystem, catalog.packages, environment, catalog.shortfall);
    let unproduced = survey.unproduced(state, owner);

    if let Some(shortfall) = &survey.shortfall {
        // Said only when some record is in question: with every record accounted
        // for, a short survey changes no answer.
        if !unproduced.is_empty() {
            sender
                .send_warning(shortfall_warning(shortfall, unproduced.len()))
                .await;
        }
        return Findings::default();
    }

    let mut findings = Findings {
        attributions: survey.attributions(state, owner),
        ..Findings::default()
    };
    // The produced targets are resolved only once some orphan is still there to
    // report.
    let mut resolved: Option<BTreeSet<PathBuf>> = None;
    for (target, entry) in unproduced {
        let path = Path::new(target);
        if is_gone(filesystem, path) {
            findings.gone.push(target.to_string());
            continue;
        }
        // The same file under another spelling, such as a case-only rename on a
        // case-insensitive volume or a path through a symlinked directory, is
        // still deployed. Resolving here only silences a report: the record is
        // kept, and nothing is written or dropped by what this finds.
        let still_deployed = resolved_for_comparison(filesystem, target).is_some_and(|canonical| {
            resolved
                .get_or_insert_with(|| survey.resolved(filesystem))
                .contains(&canonical)
        });
        if !still_deployed {
            sender
                .send_dotfile_orphaned(entry.source(), target, entry.package())
                .await;
            findings.reported += 1;
        }
    }
    findings
}

/// `target` with every link and `.`/`..` resolved, or `None` if it cannot be.
// The port forbids canonicalizing a deploy target, because a writer handed the
// result writes through the links it resolved. This is the one exception, and it
// holds only while the result is compared and dropped: nothing written, read or
// removed may take its path from here.
fn resolved_for_comparison<F: FileSystem>(filesystem: &F, target: &str) -> Option<PathBuf> {
    filesystem.canonicalize(Path::new(target)).ok()
}

/// The warning for a check skipped because `shortfall` leaves `count` records
/// in question.
fn shortfall_warning(shortfall: &Shortfall, count: usize) -> String {
    match shortfall {
        Shortfall::NothingProduced => format!(
            "Not checking {count} deployed target(s) for orphans: {shortfall}. If \
             package_directory points at the wrong directory, correct it; otherwise remove \
             the files you no longer need yourself"
        ),
        _ => format!(
            "Not checking {count} deployed target(s) for orphans: {shortfall}, so selfie cannot \
             tell whether an entry still deploys to them"
        ),
    }
}

/// Whether nothing is at `target`.
// Only an empty path is gone. A path below a non-directory or a dangling link is
// what a link into an unmounted volume looks like, so it counts as present and
// keeps its record. Dropping a record by mistake costs little: an untracked
// target whose content differs is a conflict, never overwritten.
fn is_gone<F: FileSystem>(filesystem: &F, target: &Path) -> bool {
    matches!(
        filesystem.directory_state(target),
        DirectoryState::Absent(AbsentReason::Empty)
    )
}
