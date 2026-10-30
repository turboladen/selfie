//! What a command could not compare with its own entries' targets, because
//! collecting the specs refused it, and how each command says so.

use std::path::{Path, PathBuf};

use crate::config::SelfieConfig;
use crate::package::{
    Package,
    collect::{
        Listed, UnplacedSpec,
        warning::{CollectionRefusal, Finding},
    },
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
    /// Validate, about the one spec it validates. Names spec files as apply does.
    Validate,
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
            Self::Validate => "an entry deploying to a target this spec deploys",
        }
    }

    /// How the command names the spec file at `path`, in the directory `base`.
    fn name(self, path: &Path, base: BaseKind) -> String {
        match self {
            Self::Track => path.display().to_string(),
            Self::Apply | Self::Validate => UncheckedSource::spec_file(path, base).to_string(),
        }
    }

    /// Whether any of `packages` has an entry this command could see share a
    /// target with a spec it could not use. Apply and track deploy or write in the
    /// environment in use. A validation compares in every environment any spec
    /// declares, so a spec with an entry in any environment it declares can meet one.
    fn sees_entries_in(self, packages: &[Package], environment: &str) -> bool {
        match self {
            Self::Track | Self::Apply => any_deploys_in(packages, environment),
            Self::Validate => packages
                .iter()
                .any(Package::has_dotfiles_in_any_environment),
        }
    }
}

/// Whether any of `packages` deploys an entry in `environment`, so that a target
/// of theirs could be shared with a spec the run could not use. A package
/// refused whole deploys nothing, so it has none.
fn any_deploys_in(packages: &[Package], environment: &str) -> bool {
    packages
        .iter()
        .any(|package| !package.is_refused(environment) && package.has_dotfiles_in(environment))
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
    if refusals.is_empty() || !any_deploys_in(packages, config.environment()) {
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

/// Name each of `places`, saying why, when any of `packages` has an entry
/// `checking` could see shared.
// A run asked for one package sends it, since it sent no finding of its own
// about a spec of another name. An apply and a validation of one spec both do.
pub(crate) async fn send_unchecked_warnings(
    sender: &EventSender,
    places: &[Unchecked],
    packages: &[Package],
    config: &SelfieConfig,
    checking: Checking,
) {
    if places.is_empty() || !checking.sees_entries_in(packages, config.environment()) {
        return;
    }
    // A file that fails to parse under a name several files claim is refused
    // twice, once as each. It is named once, for the first reason found, as the
    // line over everything names it once.
    let mut named: Vec<PathBuf> = Vec::new();
    for unchecked in places {
        let files = unchecked.files();
        if !files.is_empty() && files.iter().all(|file| named.contains(file)) {
            continue;
        }
        named.extend(files.into_iter().cloned());
        sender.send_warning(unchecked.sentence(checking)).await;
    }
}

/// The other specs a command compares its targets with, what collecting them
/// found, and what kept the command from seeing all of them.
pub(crate) struct Others {
    pub(crate) packages: Vec<Package>,
    /// What collection found worth saying, in the order found. Empty when the
    /// package directory could not be listed.
    pub(crate) findings: Vec<Finding>,
    /// What could not be used, which `Unchecked::sentence` words for a command.
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
    OthersListing::of(package_repo, dotfiles_repo).collect(config)
}

/// Records the spec files a command compares its targets with, as listed, and the
/// package directory's error when it could not be listed.
#[derive(Clone)]
pub(crate) struct OthersListing {
    listed: Listed,
    unlisted_packages: Option<PackageListError>,
}

impl OthersListing {
    /// Lists both directories, or the dotfiles directory alone when the package
    /// directory will not list.
    pub(crate) fn of<R: PackageRepository>(package_repo: &R, dotfiles_repo: &R) -> Self {
        match Listed::of(package_repo, dotfiles_repo) {
            Ok(listed) => Self {
                listed,
                unlisted_packages: None,
            },
            Err(error) => Self {
                listed: Listed::of_dotfiles_only(dotfiles_repo),
                unlisted_packages: Some(error),
            },
        }
    }

    /// Changes the listing to what it would be with `spec`'s file written, as
    /// [`Listed::place`] places it.
    ///
    /// # Errors
    ///
    /// Returns [`UnplacedSpec`], and leaves the listing as it was, when `spec`
    /// cannot be placed.
    pub(crate) fn place(&mut self, spec: &Package) -> Result<(), UnplacedSpec> {
        self.listed.place(spec)
    }

    /// Settles every name and returns the specs to compare with, what collecting
    /// them found, and what could not be used.
    pub(crate) fn collect(self, config: &SelfieConfig) -> Others {
        let collected = self.listed.collect(
            config.dotfiles_directory_is_expected(),
            config.environment(),
        );
        let unusable = collected.unusable();
        let Some(error) = self.unlisted_packages else {
            return Others {
                unchecked: unusable.iter().map(Unchecked::from).collect(),
                findings: collected.findings,
                packages: collected.packages,
            };
        };
        // Collection's findings are about a listing that is missing the package
        // directory, so only what it could not use is said, as the places left
        // unchecked.
        let mut unchecked = Vec::new();
        if !error.is_absent() {
            unchecked.push(Unchecked::PackageDirectory(error));
        }
        unchecked.extend(unusable.iter().map(Unchecked::from));
        Others {
            packages: collected.packages,
            findings: Vec::new(),
            unchecked,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::SelfieConfigBuilder,
        fs::DirectoryState,
        package::{
            PackageBuilder,
            event::{OperationContext, PackageEvent, metadata::OperationType},
            port::MockPackageRepository,
        },
    };

    fn config() -> SelfieConfig {
        SelfieConfigBuilder::default()
            .environment("test")
            .package_directory("/packages")
            .dotfiles_directory(PathBuf::from("/dotfiles"))
            .build()
    }

    // A dotfiles repository whose listing failed with the directory in `state`.
    fn unlisted_dotfiles(state: DirectoryState) -> MockPackageRepository {
        let mut repo = MockPackageRepository::new();
        repo.expect_list_packages().returning(move || {
            Err(PackageListError::new(
                PathBuf::from("/dotfiles"),
                state.clone(),
            ))
        });
        repo.expect_directory_identity().return_const(None);
        repo
    }

    fn package_with_an_entry() -> Package {
        let entry: crate::package::DotfileEntry =
            crate::yaml::parse("command: echo key\ntarget: ~/.key\n").expect("fixture must parse");
        PackageBuilder::default()
            .name("alpha")
            .environment("test", |b| b.install("true"))
            .dotfiles(vec![entry])
            .path("/packages/alpha.yml")
            .build()
    }

    async fn warnings_sent_for(places: &[Unchecked], package: &Package) -> Vec<String> {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let sender = EventSender::new_with_context(
            tx,
            OperationType::PackageValidate,
            "alpha".to_string(),
            "test".to_string(),
            OperationContext::default(),
        );
        send_unchecked_warnings(
            &sender,
            places,
            std::slice::from_ref(package),
            &config(),
            Checking::Validate,
        )
        .await;
        drop(sender);
        let mut warnings = Vec::new();
        while let Some(event) = rx.recv().await {
            if let PackageEvent::Warning { message, .. } = event {
                warnings.push(message);
            }
        }
        warnings
    }

    // A dotfiles directory that cannot be classified is refused, and named as the
    // one place left unchecked, once.
    #[tokio::test]
    async fn an_uncheckable_dotfiles_directory_is_named_once() {
        let dotfiles = unlisted_dotfiles(DirectoryState::Unknown(std::sync::Arc::new(
            std::io::Error::other("Too many levels of symbolic links"),
        )));

        let others = collect_others(
            &MockPackageRepository::listing_nothing(),
            &dotfiles,
            &config(),
        );

        assert!(
            matches!(
                others.unchecked.as_slice(),
                [Unchecked::DotfilesDirectory(_)]
            ),
            "got {} places",
            others.unchecked.len()
        );
        let warnings = warnings_sent_for(&others.unchecked, &package_with_an_entry()).await;
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("standalone dotfiles"), "{warnings:?}");
    }

    // Collision checks across the environments a spec declares: an entry in an
    // environment other than the one in use is an entry that could be shared.
    #[tokio::test]
    async fn an_entry_in_another_environment_gets_the_warning() {
        let package = PackageBuilder::default()
            .name("alpha")
            .environment("test", |b| b.install("true"))
            .environment("work", |b| {
                let entry: crate::package::DotfileEntry =
                    crate::yaml::parse("command: echo key\ntarget: ~/.key\n")
                        .expect("fixture must parse");
                b.install("true").dotfiles(vec![entry])
            })
            .path("/packages/alpha.yml")
            .build();
        let place = Unchecked::Unloadable {
            path: PathBuf::from("/packages/beta.yml"),
            base: BaseKind::PackageDirectory,
        };

        let warnings = warnings_sent_for(std::slice::from_ref(&place), &package).await;

        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }
}
