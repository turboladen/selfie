//! What a command could not compare with its own entries' targets, because
//! collecting the specs refused it, and how each command says so.

use std::path::{Path, PathBuf};

use crate::config::SelfieConfig;
use crate::package::{
    Package,
    collect::{collect_all_packages, warning::CollectionRefusal},
    event::{BaseKind, EventSender, UncheckedSource},
    port::{PackageListError, PackageRepository},
};

/// A place a command could not check for an entry sharing a target with its own.
pub(crate) enum Unchecked {
    /// A spec file that could not be loaded, and the directory it is in.
    Unloadable { path: PathBuf, base: BaseKind },
    /// Files in one directory that all claim one name, so none of them is used.
    Ambiguous {
        name: String,
        paths: Vec<PathBuf>,
        base: BaseKind,
    },
    /// The package directory, which could not be listed.
    PackageDirectory(PackageListError),
    /// The dotfiles directory, which could not be listed or checked.
    DotfilesDirectory(PackageListError),
}

/// Which command is asking, which decides how it names what it could not check.
#[derive(Clone, Copy)]
pub(crate) enum Checking {
    /// Track, about the one file it tracks. Names spec files by full path.
    Track,
    /// Apply, about the targets it deploys. Names spec files as `packages/<file>`
    /// or `dotfiles/<file>`.
    Apply,
}

impl From<&CollectionRefusal> for Unchecked {
    fn from(refusal: &CollectionRefusal) -> Self {
        match refusal {
            CollectionRefusal::UnloadableSpec { error, base } => Self::Unloadable {
                path: error.package_path().to_path_buf(),
                base: *base,
            },
            CollectionRefusal::UnlistableDotfilesDirectory(error)
            | CollectionRefusal::UncheckableDotfilesDirectory(error) => {
                Self::DotfilesDirectory(error.clone())
            }
            CollectionRefusal::AmbiguousName { name, paths, base } => Self::Ambiguous {
                name: name.clone(),
                paths: paths.clone(),
                base: *base,
            },
        }
    }
}

impl Checking {
    /// What the command could not check for.
    fn subject(self) -> &'static str {
        match self {
            Self::Track => "an entry deploying this file",
            Self::Apply => "an entry deploying to a target this apply deploys",
        }
    }

    /// How the command names the spec file at `path`, in the directory `base`.
    fn name(self, path: &Path, base: BaseKind) -> String {
        match self {
            Self::Track => path.display().to_string(),
            Self::Apply => UncheckedSource::spec_file(path, base).to_string(),
        }
    }
}

impl Unchecked {
    /// The spec files this place is, if it is any.
    fn files(&self) -> Vec<&PathBuf> {
        match self {
            Self::Unloadable { path, .. } => vec![path],
            Self::Ambiguous { paths, .. } => paths.iter().collect(),
            Self::PackageDirectory(_) | Self::DotfilesDirectory(_) => Vec::new(),
        }
    }

    /// The warning `checking` sends about this place.
    pub(crate) fn sentence(&self, checking: Checking) -> String {
        let subject = checking.subject();
        match self {
            Self::Unloadable { path, base } => format!(
                "Could not check {} for {subject}: it could not be loaded",
                checking.name(path, *base)
            ),
            Self::Ambiguous { name, paths, base } => {
                let files: Vec<String> = paths
                    .iter()
                    .map(|path| checking.name(path, *base))
                    .collect();
                format!(
                    "Could not check {} for {subject}: they all claim the name '{name}', so none \
                     of them is used",
                    files.join(" and ")
                )
            }
            Self::PackageDirectory(error) => {
                format!("Could not check the package specs for {subject}: {error}")
            }
            Self::DotfilesDirectory(error) => {
                format!("Could not check the standalone dotfiles for {subject}: {error}")
            }
        }
    }
}

/// Name, in one line, what `refusals` leave unchecked, when any of `packages` has
/// an entry in play here. Each source is named once.
// A run over every package sends it after the refusals' own warnings.
pub(crate) async fn send_unchecked_line(
    sender: &EventSender,
    refusals: &[CollectionRefusal],
    packages: &[Package],
    config: &SelfieConfig,
) {
    if refusals.is_empty() || !has_entries(packages, config.environment()) {
        return;
    }
    let mut sources: Vec<UncheckedSource> = Vec::new();
    for source in refusals
        .iter()
        .flat_map(CollectionRefusal::unchecked_sources)
    {
        if !sources.contains(&source) {
            sources.push(source);
        }
    }
    if !sources.is_empty() {
        sender.send_collisions_unchecked(sources).await;
    }
}

/// Name each place `refusals` leave unchecked, saying why, when any of
/// `packages` has an entry in play here.
// A run asked for one package sends it, since it sent no finding of its own
// about a spec of another name.
pub(crate) async fn send_unchecked_sentences(
    sender: &EventSender,
    refusals: &[CollectionRefusal],
    packages: &[Package],
    config: &SelfieConfig,
) {
    if refusals.is_empty() || !has_entries(packages, config.environment()) {
        return;
    }
    // A file that fails to parse under a name several files claim is refused
    // twice, once as each. It is named once, for the first reason found, as the
    // line over everything names it once.
    let mut named: Vec<PathBuf> = Vec::new();
    for refusal in refusals {
        let unchecked = Unchecked::from(refusal);
        let files = unchecked.files();
        if !files.is_empty() && files.iter().all(|file| named.contains(file)) {
            continue;
        }
        named.extend(files.into_iter().cloned());
        sender
            .send_warning(unchecked.sentence(Checking::Apply))
            .await;
    }
}

/// Whether any of `packages` deploys an entry in `environment`, so that a target
/// of theirs could be shared with a spec the run could not use. A package
/// refused whole deploys nothing, so it has none.
fn has_entries(packages: &[Package], environment: &str) -> bool {
    packages
        .iter()
        .any(|package| !package.is_refused(environment) && package.has_dotfiles_in(environment))
}

/// The other specs a command compares its targets with, and what kept it from
/// seeing all of them, already worded.
pub(crate) struct Others {
    pub(crate) packages: Vec<Package>,
    /// What could not be used.
    pub(crate) unchecked: Vec<Unchecked>,
}

/// Collect every spec for a command that compares its targets with them.
///
/// When the package directory will not list, collection stops there, but the
/// standalone specs are read through a repository of their own and are still
/// compared. A directory with nothing at its path holds no specs and is worth no
/// word.
pub(crate) fn collect_others<R: PackageRepository>(
    package_repo: &R,
    dotfiles_repo: &R,
    config: &SelfieConfig,
) -> Others {
    match collect_all_packages(
        package_repo,
        dotfiles_repo,
        config.dotfiles_directory_is_expected(),
        config.environment(),
    ) {
        Ok(collected) => Others {
            unchecked: collected.unusable().iter().map(Unchecked::from).collect(),
            packages: collected.packages,
        },
        Err(error) => {
            let mut unchecked = Vec::new();
            if !error.is_absent() {
                unchecked.push(Unchecked::PackageDirectory(error));
            }
            let packages = match dotfiles_repo.list_packages() {
                Ok(output) => {
                    unchecked.extend(output.invalid_packages().map(|invalid| {
                        Unchecked::Unloadable {
                            path: invalid.package_path().to_path_buf(),
                            base: BaseKind::DotfilesDirectory,
                        }
                    }));
                    output.valid_packages().cloned().collect()
                }
                Err(error) => {
                    if !error.is_absent() {
                        unchecked.push(Unchecked::DotfilesDirectory(error));
                    }
                    Vec::new()
                }
            };
            Others {
                packages,
                unchecked,
            }
        }
    }
}
