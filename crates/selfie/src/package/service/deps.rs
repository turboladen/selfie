//! Dependency graph resolution for package installation.
//!
//! Resolves package dependencies into a topological install order and detects
//! circular dependencies using DFS with four-state visit tracking.

use crate::package::{
    event::{EventSender, OperationFailure},
    port::PackageRepository,
};

/// The result of resolving a package's dependency graph.
#[derive(Debug, Clone)]
pub(crate) struct DependencyGraph {
    /// Packages in topological install order (dependencies first, target last).
    pub install_order: Vec<String>,
    /// The root package's recommends for the environment, from the same read
    /// that produced its dependencies.
    pub root_recommends: Vec<String>,
}

/// Visit state for cycle detection during DFS traversal.
// Two walks share one map, and they finish a package for different reasons:
// `dfs` places it in `install_order`, `check_recommend_cycles` only reads its
// edges. They need separate marks, because a package the recommend walk has
// merely looked at is one `dfs` still has to install.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VisitState {
    /// Not yet visited.
    Unvisited,
    /// Currently on the DFS stack — encountering this again means a cycle.
    Visiting,
    /// Fully explored by `dfs` and placed in `install_order`.
    Visited,
    /// Walked by `check_recommend_cycles` and found free of cycles. Says
    /// nothing about installing it: `dfs` must still decide that.
    CycleChecked,
}

/// Resolve the dependency graph for `root_package`, returning a topological
/// install order or an `OperationFailure` on cycle / missing dependency.
pub(crate) async fn resolve_dependencies<PR>(
    root_package: &str,
    repo: &PR,
    config_environment: &str,
    sender: &EventSender,
) -> Result<DependencyGraph, Box<OperationFailure>>
where
    PR: PackageRepository,
{
    use std::collections::HashMap;

    let mut visit_state: HashMap<String, VisitState> = HashMap::new();
    let mut install_order: Vec<String> = Vec::new();

    sender
        .send_trace(format!(
            "Resolving dependencies for package '{root_package}'"
        ))
        .await;

    // Kept from the read that resolved the root, so install never loads the root
    // after installing it. A load failing at that point has no install left to
    // fail, and could only skip every recommend under a successful result.
    let root_recommends = dfs(
        root_package,
        repo,
        config_environment,
        sender,
        &mut visit_state,
        &mut install_order,
        &mut vec![root_package.to_string()],
    )
    .await?;

    sender
        .send_trace(format!(
            "Dependency resolution complete. Install order: {:?}",
            install_order
        ))
        .await;

    Ok(DependencyGraph {
        install_order,
        root_recommends,
    })
}

/// What [`dfs`] resolves to: the package's recommends, or the failure.
type DfsFuture<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<Vec<String>, Box<OperationFailure>>> + Send + 'a>,
>;

/// Recursive DFS that builds `install_order` bottom-up and detects cycles.
///
/// Returns the package's recommends for the environment, or an empty list for a
/// package already visited.
fn dfs<'a, PR>(
    package_name: &'a str,
    repo: &'a PR,
    config_environment: &'a str,
    sender: &'a EventSender,
    visit_state: &'a mut std::collections::HashMap<String, VisitState>,
    install_order: &'a mut Vec<String>,
    path: &'a mut Vec<String>,
) -> DfsFuture<'a>
where
    PR: PackageRepository + Sync,
{
    Box::pin(async move {
        let state = visit_state
            .get(package_name)
            .copied()
            .unwrap_or(VisitState::Unvisited);

        match state {
            VisitState::Visited => return Ok(Vec::new()),
            VisitState::Visiting => {
                // Build the cycle path from where the cycle starts.
                // The path already ends with package_name (pushed by the caller),
                // so path[cycle_start..] gives e.g. [A, B, A] for A->B->A.
                let cycle_start = path
                    .iter()
                    .position(|n| n == package_name)
                    .expect("package must be on path when in Visiting state");
                let cycle: Vec<String> = path[cycle_start..].to_vec();

                return Err(Box::new(OperationFailure::circular_dependency(
                    package_name.to_string(),
                    cycle,
                )));
            }
            // `CycleChecked` means the recommend walk read this package's edges
            // and nothing more. Where it belongs in `install_order` is still
            // this walk's to decide, so both fall through.
            VisitState::Unvisited | VisitState::CycleChecked => {}
        }

        visit_state.insert(package_name.to_string(), VisitState::Visiting);

        // Load the package to discover its dependencies
        let package_blob = repo.get_package(package_name).map_err(|repo_err| {
            // The root package was named by the user, so the repository's own
            // error already says everything there is to say about it.
            if path.len() < 2 {
                return OperationFailure::from(repo_err);
            }

            let parent = path[path.len() - 2].clone();

            // Three answers, not two. A dependency selfie found and could not
            // read is not one that is absent -- calling it missing sends the
            // user looking for a file already in the package directory. But a
            // failure that is not about that package's file either, such as a
            // directory selfie could not list, is not its spec's fault, and
            // saying the spec will not be read blames the wrong thing.
            if repo_err.means_no_such_package() {
                OperationFailure::missing_dependency(parent, package_name.to_string())
            } else if repo_err.names_an_unusable_spec() {
                OperationFailure::unreadable_spec(
                    package_name.to_string(),
                    Some(parent),
                    repo_err.to_string(),
                )
            } else {
                OperationFailure::from(repo_err)
            }
        })?;

        // Asked before the environment is read, because reading it is the harm.
        // A key shadowing `environments:` makes this lookup miss, and a miss
        // here is indistinguishable from a package that genuinely declares no
        // dependencies: `deps` and `recommends` come back empty, the graph is
        // built short, and install runs to completion without the packages this
        // one needs.
        if let Some(refusal) = package_blob.package.spec_refusal(config_environment) {
            // Named the same way a missing dependency is: a user who asked to
            // install one package and is handed the name of another has no way
            // to tell why selfie looked at it.
            let required_by = (path.len() >= 2).then(|| path[path.len() - 2].clone());
            return Err(Box::new(OperationFailure::unreadable_spec(
                package_name.to_string(),
                required_by,
                refusal.to_string(),
            )));
        }

        // Get deps and recommends for the current environment
        let env_config = package_blob.package.environments().get(config_environment);

        let deps: Vec<String> = env_config
            .map(|env| env.dependencies.clone())
            .unwrap_or_default();

        let recommends: Vec<String> = env_config
            .map(|env| env.recommends().to_vec())
            .unwrap_or_default();

        if !deps.is_empty() {
            sender
                .send_trace(format!(
                    "Package '{package_name}' has dependencies: {deps:?}"
                ))
                .await;
        }

        if !recommends.is_empty() {
            sender
                .send_trace(format!(
                    "Package '{package_name}' has recommends: {recommends:?}"
                ))
                .await;
        }

        // Traverse hard dependencies — these go into install_order
        for dep in &deps {
            path.push(dep.clone());
            dfs(
                dep,
                repo,
                config_environment,
                sender,
                visit_state,
                install_order,
                path,
            )
            .await?;
            path.pop();
        }

        // Traverse recommends for cycle detection only — NOT added to install_order.
        // We still need to walk recommends to catch cycles like A recommends B, B depends on A.
        for rec in &recommends {
            path.push(rec.clone());
            // Only check for cycles; don't add to install_order (recommends are installed
            // separately in the post-install phase)
            check_recommend_cycles(rec, repo, config_environment, sender, visit_state, path)
                .await?;
            path.pop();
        }

        visit_state.insert(package_name.to_string(), VisitState::Visited);
        install_order.push(package_name.to_string());

        Ok(recommends)
    })
}

/// Walk a recommend's dependency graph for cycle detection only.
///
/// Unlike `dfs`, this adds nothing to `install_order`, and it marks a package it
/// clears as `CycleChecked` rather than `Visited`, which leaves `dfs` free to
/// install that package later as some other package's hard dependency. A package
/// either walk has already finished is skipped.
///
/// Traverses both hard `dependencies` AND `recommends` of the recommended package
/// to catch cycles formed entirely through recommend edges (e.g., A recommends B,
/// B recommends A).
fn check_recommend_cycles<'a, PR>(
    package_name: &'a str,
    repo: &'a PR,
    config_environment: &'a str,
    _sender: &'a EventSender,
    visit_state: &'a mut std::collections::HashMap<String, VisitState>,
    path: &'a mut Vec<String>,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<(), Box<OperationFailure>>> + Send + 'a>,
>
where
    PR: PackageRepository + Sync,
{
    Box::pin(async move {
        let state = visit_state
            .get(package_name)
            .copied()
            .unwrap_or(VisitState::Unvisited);

        match state {
            // Either mark means this package's edges have already been read, by
            // whichever walk got here first, so there is no cycle to find below
            // it a second time.
            VisitState::Visited | VisitState::CycleChecked => return Ok(()),
            // Currently on the DFS stack — cycle detected
            VisitState::Visiting => {
                let cycle_start = path
                    .iter()
                    .position(|n| n == package_name)
                    .expect("package must be on path when in Visiting state");
                let cycle: Vec<String> = path[cycle_start..].to_vec();

                return Err(Box::new(OperationFailure::circular_dependency(
                    package_name.to_string(),
                    cycle,
                )));
            }
            VisitState::Unvisited => {}
        }

        // Mark visiting for cycle detection
        visit_state.insert(package_name.to_string(), VisitState::Visiting);

        // Every load failure is skipped here, absent file and unreadable file
        // alike, because a recommend is soft and refusing one must not fail the
        // install its parent asked for. Clearing the temporary entry keeps a
        // later hard dependency on the same package from being masked.
        //
        // The silence costs nothing because whatever installs a package skipped
        // here reads it again: `dfs`, when it is also a hard dependency, or
        // `install_single_recommend`, when it is one of the root's recommends or
        // a dependency of one, which reports the failure as `RecommendFailed`. A
        // package neither reaches is never installed.
        let Ok(package_blob) = repo.get_package(package_name) else {
            visit_state.remove(package_name);
            return Ok(());
        };

        // The same question `dfs` asks, and the same reason: a key shadowing
        // `environments:` empties the two lists below, so the edges this walk is
        // here to find are never read. Skipped the way a package that does not
        // load is skipped just above, because a recommend is soft and refusing one
        // must not fail the install its parent asked for. Clearing the entry keeps
        // a `Visited` mark off a package whose edges nothing looked at.
        if package_blob
            .package
            .spec_refusal(config_environment)
            .is_some()
        {
            visit_state.remove(package_name);
            return Ok(());
        }

        // Extract both deps and recommends from the environment config
        let (deps, recs) = package_blob
            .package
            .environments()
            .get(config_environment)
            .map(|env| (env.dependencies.clone(), env.recommends().to_vec()))
            .unwrap_or_default();

        // Check hard dependencies of this recommend for cycles
        for dep in &deps {
            path.push(dep.clone());
            check_recommend_cycles(dep, repo, config_environment, _sender, visit_state, path)
                .await?;
            path.pop();
        }

        // Also walk recommends to catch cycles formed entirely through recommend edges
        for rec in &recs {
            path.push(rec.clone());
            check_recommend_cycles(rec, repo, config_environment, _sender, visit_state, path)
                .await?;
            path.pop();
        }

        // `CycleChecked`, never `Visited`: this walk puts nothing in
        // `install_order`, and marking it the way `dfs` marks a package it has
        // installed would make `dfs` skip it. A package reached here first can
        // still be some other package's hard dependency.
        visit_state.insert(package_name.to_string(), VisitState::CycleChecked);
        Ok(())
    })
}

#[cfg(all(test, feature = "with_mocks"))]
mod tests {
    use super::*;
    use crate::package::{
        GetPackage, PackageBuilder,
        event::{OperationContext, PackageEvent, metadata::OperationType},
        port::MockPackageRepository,
    };
    use tokio::sync::mpsc;

    fn make_sender() -> EventSender {
        let (tx, _rx) = mpsc::channel::<PackageEvent>(32);
        EventSender::new_with_context(
            tx,
            OperationType::PackageInstall,
            "test".to_string(),
            "test".to_string(),
            OperationContext::default(),
        )
    }

    fn mock_package(name: &str, deps: &[&str]) -> GetPackage {
        mock_package_with_recommends(name, deps, &[])
    }

    fn mock_package_with_recommends(name: &str, deps: &[&str], recommends: &[&str]) -> GetPackage {
        let deps_owned: Vec<String> = deps.iter().map(|d| d.to_string()).collect();
        let recs_owned: Vec<String> = recommends.iter().map(|r| r.to_string()).collect();
        let install_cmd = format!("echo 'installing {name}'");
        let check_cmd = format!("echo 'checking {name}'");
        let pkg = PackageBuilder::default()
            .name(name)
            .environment("test", move |b| {
                b.install(&install_cmd)
                    .check(Some(&check_cmd))
                    .dependencies(deps_owned.clone())
                    .recommends(recs_owned.clone())
            })
            .build();
        GetPackage {
            package: pkg,
            file_path: std::path::PathBuf::from(format!("/tmp/{name}.yml")),
            is_new: false,
        }
    }

    #[tokio::test]
    async fn test_no_dependencies() {
        let mut repo = MockPackageRepository::new();
        repo.expect_get_package()
            .withf(|name| name == "pkg-a")
            .returning(|_| Ok(mock_package("pkg-a", &[])));

        let sender = make_sender();
        let graph = resolve_dependencies("pkg-a", &repo, "test", &sender)
            .await
            .unwrap();

        assert_eq!(graph.install_order, vec!["pkg-a"]);
    }

    #[tokio::test]
    async fn test_single_dependency() {
        let mut repo = MockPackageRepository::new();
        repo.expect_get_package()
            .withf(|name| name == "pkg-a")
            .returning(|_| Ok(mock_package("pkg-a", &["pkg-b"])));
        repo.expect_get_package()
            .withf(|name| name == "pkg-b")
            .returning(|_| Ok(mock_package("pkg-b", &[])));

        let sender = make_sender();
        let graph = resolve_dependencies("pkg-a", &repo, "test", &sender)
            .await
            .unwrap();

        assert_eq!(graph.install_order, vec!["pkg-b", "pkg-a"]);
    }

    #[tokio::test]
    async fn test_chain_dependencies() {
        let mut repo = MockPackageRepository::new();
        repo.expect_get_package()
            .withf(|name| name == "pkg-a")
            .returning(|_| Ok(mock_package("pkg-a", &["pkg-b"])));
        repo.expect_get_package()
            .withf(|name| name == "pkg-b")
            .returning(|_| Ok(mock_package("pkg-b", &["pkg-c"])));
        repo.expect_get_package()
            .withf(|name| name == "pkg-c")
            .returning(|_| Ok(mock_package("pkg-c", &[])));

        let sender = make_sender();
        let graph = resolve_dependencies("pkg-a", &repo, "test", &sender)
            .await
            .unwrap();

        assert_eq!(graph.install_order, vec!["pkg-c", "pkg-b", "pkg-a"]);
    }

    #[tokio::test]
    async fn test_diamond_dependencies() {
        let mut repo = MockPackageRepository::new();
        repo.expect_get_package()
            .withf(|name| name == "pkg-a")
            .returning(|_| Ok(mock_package("pkg-a", &["pkg-b", "pkg-c"])));
        repo.expect_get_package()
            .withf(|name| name == "pkg-b")
            .returning(|_| Ok(mock_package("pkg-b", &["pkg-d"])));
        repo.expect_get_package()
            .withf(|name| name == "pkg-c")
            .returning(|_| Ok(mock_package("pkg-c", &["pkg-d"])));
        repo.expect_get_package()
            .withf(|name| name == "pkg-d")
            .returning(|_| Ok(mock_package("pkg-d", &[])));

        let sender = make_sender();
        let graph = resolve_dependencies("pkg-a", &repo, "test", &sender)
            .await
            .unwrap();

        // D must come before B and C; A must be last
        let pos = |name: &str| graph.install_order.iter().position(|n| n == name).unwrap();
        assert!(pos("pkg-d") < pos("pkg-b"));
        assert!(pos("pkg-d") < pos("pkg-c"));
        assert_eq!(*graph.install_order.last().unwrap(), "pkg-a");
        assert_eq!(graph.install_order.len(), 4);
    }

    #[tokio::test]
    async fn test_circular_dependency_direct() {
        let mut repo = MockPackageRepository::new();
        repo.expect_get_package()
            .withf(|name| name == "pkg-a")
            .returning(|_| Ok(mock_package("pkg-a", &["pkg-b"])));
        repo.expect_get_package()
            .withf(|name| name == "pkg-b")
            .returning(|_| Ok(mock_package("pkg-b", &["pkg-a"])));

        let sender = make_sender();
        let result = resolve_dependencies("pkg-a", &repo, "test", &sender).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.is_dependency_error());
        match err.dependency_failure().unwrap() {
            crate::package::event::DependencyFailure::CircularDependency { cycle, .. } => {
                // Cycle should be [A, B, A] — starts and ends with A
                assert_eq!(cycle, &["pkg-a", "pkg-b", "pkg-a"]);
            }
            _ => panic!("Expected CircularDependency"),
        }
    }

    #[tokio::test]
    async fn test_circular_dependency_indirect() {
        let mut repo = MockPackageRepository::new();
        repo.expect_get_package()
            .withf(|name| name == "pkg-a")
            .returning(|_| Ok(mock_package("pkg-a", &["pkg-b"])));
        repo.expect_get_package()
            .withf(|name| name == "pkg-b")
            .returning(|_| Ok(mock_package("pkg-b", &["pkg-c"])));
        repo.expect_get_package()
            .withf(|name| name == "pkg-c")
            .returning(|_| Ok(mock_package("pkg-c", &["pkg-a"])));

        let sender = make_sender();
        let result = resolve_dependencies("pkg-a", &repo, "test", &sender).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.is_dependency_error());
        match err.dependency_failure().unwrap() {
            crate::package::event::DependencyFailure::CircularDependency { cycle, .. } => {
                // Cycle should be [A, B, C, A] — starts and ends with A
                assert_eq!(cycle, &["pkg-a", "pkg-b", "pkg-c", "pkg-a"]);
            }
            _ => panic!("Expected CircularDependency"),
        }
    }

    #[tokio::test]
    async fn test_self_dependency() {
        let mut repo = MockPackageRepository::new();
        repo.expect_get_package()
            .withf(|name| name == "pkg-a")
            .returning(|_| Ok(mock_package("pkg-a", &["pkg-a"])));

        let sender = make_sender();
        let result = resolve_dependencies("pkg-a", &repo, "test", &sender).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.is_dependency_error());
        match err.dependency_failure().unwrap() {
            crate::package::event::DependencyFailure::CircularDependency { cycle, .. } => {
                // Self-cycle should be [A, A]
                assert_eq!(cycle, &["pkg-a", "pkg-a"]);
            }
            _ => panic!("Expected CircularDependency"),
        }
    }

    #[tokio::test]
    async fn test_root_package_not_found() {
        let mut repo = MockPackageRepository::new();
        repo.expect_get_package()
            .withf(|name| name == "nonexistent")
            .returning(|_| {
                Err(crate::package::port::PackageError::PackageNotFound {
                    name: "nonexistent".to_string(),
                    packages_path: std::path::PathBuf::from("/tmp"),
                    files_examined: 0,
                    search_patterns: vec![],
                }
                .into())
            });

        let sender = make_sender();
        let result = resolve_dependencies("nonexistent", &repo, "test", &sender).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        // Root package missing should give a PackageError, NOT a MissingDependency
        assert!(
            err.is_package_error(),
            "Expected PackageError for missing root package, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_missing_dependency() {
        let mut repo = MockPackageRepository::new();
        repo.expect_get_package()
            .withf(|name| name == "pkg-a")
            .returning(|_| Ok(mock_package("pkg-a", &["nonexistent"])));
        repo.expect_get_package()
            .withf(|name| name == "nonexistent")
            .returning(|_| {
                Err(crate::package::port::PackageError::PackageNotFound {
                    name: "nonexistent".to_string(),
                    packages_path: std::path::PathBuf::from("/tmp"),
                    files_examined: 0,
                    search_patterns: vec![],
                }
                .into())
            });

        let sender = make_sender();
        let result = resolve_dependencies("pkg-a", &repo, "test", &sender).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.is_dependency_error());
        match err.dependency_failure().unwrap() {
            crate::package::event::DependencyFailure::MissingDependency {
                package_name,
                dependency_name,
            } => {
                assert_eq!(package_name, "pkg-a");
                assert_eq!(dependency_name, "nonexistent");
            }
            _ => panic!("Expected MissingDependency"),
        }
    }

    // The twin of the test above, differing in one way: the dependency's file is
    // there. A user handed "brokendep is missing" goes looking for a file that is
    // already in the package directory, and is never told what is wrong with it.
    #[tokio::test]
    async fn an_unreadable_dependency_is_not_reported_as_a_missing_one() {
        let mut repo = MockPackageRepository::new();
        repo.expect_get_package()
            .withf(|name| name == "pkg-a")
            .returning(|_| Ok(mock_package("pkg-a", &["brokendep"])));
        repo.expect_get_package()
            .withf(|name| name == "brokendep")
            .returning(|_| {
                Err(crate::package::port::PackageError::UnreadableFile {
                    name: "brokendep".to_string(),
                    packages_path: std::path::PathBuf::from("/tmp"),
                    failed_file: std::path::PathBuf::from("/tmp/brokendep.yml"),
                    source: crate::package::port::PackageParseError::new(
                        "/tmp/brokendep.yml",
                        crate::package::port::PackageParseKind::IrregularFile {
                            kind: "named pipe (fifo)",
                        },
                    ),
                }
                .into())
            });

        let sender = make_sender();
        let result = resolve_dependencies("pkg-a", &repo, "test", &sender).await;

        let err = result.unwrap_err();
        assert!(err.is_dependency_error());
        match err.dependency_failure().unwrap() {
            crate::package::event::DependencyFailure::UnreadableSpec {
                package_name,
                required_by,
                reason,
            } => {
                assert_eq!(package_name, "brokendep");
                assert_eq!(required_by.as_deref(), Some("pkg-a"));
                assert!(
                    reason.contains("named pipe (fifo)"),
                    "the reason must say what selfie could not do with the file, got: {reason}"
                );
            }
            _ => panic!("Expected UnreadableSpec"),
        }
    }

    // The third answer. A directory selfie could not list is not a verdict on
    // this dependency's spec -- there may not be one -- so reporting it as a
    // spec selfie will not read blames a file that was never opened, and hides
    // the permission problem the user actually has to fix.
    #[tokio::test]
    async fn a_listing_failure_is_not_reported_as_an_unreadable_dependency() {
        let mut repo = MockPackageRepository::new();
        repo.expect_get_package()
            .withf(|name| name == "pkg-a")
            .returning(|_| Ok(mock_package("pkg-a", &["pkg-b"])));
        repo.expect_get_package()
            .withf(|name| name == "pkg-b")
            .returning(|_| {
                Err(crate::package::port::PackageRepoError::PackageListError(
                    crate::package::port::PackageListError::new(
                        std::path::PathBuf::from("/locked"),
                        crate::fs::DirectoryState::Unlistable(std::sync::Arc::new(
                            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
                        )),
                    ),
                ))
            });

        let sender = make_sender();
        let err = resolve_dependencies("pkg-a", &repo, "test", &sender)
            .await
            .expect_err("a listing failure must not resolve");

        assert!(
            err.dependency_failure().is_none(),
            "a listing failure is not a verdict about pkg-b's spec, got: {err}"
        );
        let rendered = err.to_string();
        assert!(
            rendered.contains("denied"),
            "the listing failure must survive to the user, got: {rendered}"
        );
    }

    // A dangling link at the package directory holds no specs, but it is not a
    // verdict that pkg-b is missing either: reporting it as one sends the user
    // looking for a spec when the directory is what needs fixing.
    #[tokio::test]
    async fn a_dangling_package_directory_is_not_reported_as_a_missing_dependency() {
        let mut repo = MockPackageRepository::new();
        repo.expect_get_package()
            .withf(|name| name == "pkg-a")
            .returning(|_| Ok(mock_package("pkg-a", &["pkg-b"])));
        repo.expect_get_package()
            .withf(|name| name == "pkg-b")
            .returning(|_| {
                Err(crate::package::port::PackageRepoError::PackageListError(
                    crate::package::port::PackageListError::new(
                        std::path::PathBuf::from("/packages"),
                        crate::fs::DirectoryState::Absent(
                            crate::fs::AbsentReason::DanglingSymlink {
                                points_to: Some(std::path::PathBuf::from("/gone")),
                            },
                        ),
                    ),
                ))
            });

        let sender = make_sender();
        let err = resolve_dependencies("pkg-a", &repo, "test", &sender)
            .await
            .expect_err("a dangling package directory must not resolve");

        assert!(
            err.dependency_failure().is_none(),
            "the directory is the problem, not pkg-b, got: {err}"
        );
        let rendered = err.to_string();
        assert!(
            rendered.contains("/packages is a symlink to nothing"),
            "the directory must be named, got: {rendered}"
        );
    }

    #[tokio::test]
    async fn test_recommend_cycle_detected() {
        // A recommends B, B depends on A → cycle
        let mut repo = MockPackageRepository::new();
        repo.expect_get_package()
            .withf(|name| name == "pkg-a")
            .returning(|_| Ok(mock_package_with_recommends("pkg-a", &[], &["pkg-b"])));
        repo.expect_get_package()
            .withf(|name| name == "pkg-b")
            .returning(|_| Ok(mock_package("pkg-b", &["pkg-a"])));

        let sender = make_sender();
        let result = resolve_dependencies("pkg-a", &repo, "test", &sender).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.is_dependency_error());
        match err.dependency_failure().unwrap() {
            crate::package::event::DependencyFailure::CircularDependency { cycle, .. } => {
                // Cycle: A → (recommends) B → (depends) A
                assert_eq!(cycle, &["pkg-a", "pkg-b", "pkg-a"]);
            }
            _ => panic!("Expected CircularDependency"),
        }
    }

    // A package can be reached by the recommend walk before anything depends on
    // it hard. pkg-a depends on [pkg-b, pkg-c]; pkg-b recommends pkg-r; pkg-c
    // depends on pkg-r. Walking pkg-b's recommends reaches pkg-r first, and if
    // that walk marked it the way `dfs` marks an installed package, `dfs(pkg-c)`
    // would skip it and `install pkg-a` would report success having never
    // installed pkg-r.
    //
    // The order matters as much as the membership: pkg-r has to be installed
    // before the package that needs it.
    #[tokio::test]
    async fn a_recommend_reached_first_is_still_installed_as_a_hard_dependency() {
        let mut repo = MockPackageRepository::new();
        repo.expect_get_package().returning(|name| match name {
            "pkg-a" => Ok(mock_package("pkg-a", &["pkg-b", "pkg-c"])),
            "pkg-b" => Ok(mock_package_with_recommends("pkg-b", &[], &["pkg-r"])),
            "pkg-c" => Ok(mock_package("pkg-c", &["pkg-r"])),
            "pkg-r" => Ok(mock_package("pkg-r", &[])),
            other => panic!("unexpected package {other}"),
        });

        let sender = make_sender();
        let graph = resolve_dependencies("pkg-a", &repo, "test", &sender)
            .await
            .expect("resolution must succeed");

        let order = &graph.install_order;
        let pos = |name: &str| {
            order
                .iter()
                .position(|n| n == name)
                .unwrap_or_else(|| panic!("{name} missing from {order:?}"))
        };

        assert!(
            pos("pkg-r") < pos("pkg-c"),
            "pkg-r is a hard dependency of pkg-c and must precede it, got {order:?}"
        );
        assert!(
            pos("pkg-c") < pos("pkg-a"),
            "pkg-c must precede the package that asked for it, got {order:?}"
        );
    }

    #[tokio::test]
    async fn test_recommends_not_in_install_order() {
        // A recommends B — B should NOT appear in install_order
        let mut repo = MockPackageRepository::new();
        repo.expect_get_package()
            .withf(|name| name == "pkg-a")
            .returning(|_| Ok(mock_package_with_recommends("pkg-a", &[], &["pkg-b"])));
        repo.expect_get_package()
            .withf(|name| name == "pkg-b")
            .returning(|_| Ok(mock_package("pkg-b", &[])));

        let sender = make_sender();
        let graph = resolve_dependencies("pkg-a", &repo, "test", &sender)
            .await
            .unwrap();

        // Only hard deps + root in install_order; recommend pkg-b excluded
        assert_eq!(graph.install_order, vec!["pkg-a"]);
        assert_eq!(graph.root_recommends, vec!["pkg-b"]);
    }

    #[tokio::test]
    async fn test_recommend_recommend_cycle_detected() {
        // A recommends B, B recommends A → cycle through recommend edges only
        let mut repo = MockPackageRepository::new();
        repo.expect_get_package()
            .withf(|name| name == "pkg-a")
            .returning(|_| Ok(mock_package_with_recommends("pkg-a", &[], &["pkg-b"])));
        repo.expect_get_package()
            .withf(|name| name == "pkg-b")
            .returning(|_| Ok(mock_package_with_recommends("pkg-b", &[], &["pkg-a"])));

        let sender = make_sender();
        let result = resolve_dependencies("pkg-a", &repo, "test", &sender).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.is_dependency_error());
        match err.dependency_failure().unwrap() {
            crate::package::event::DependencyFailure::CircularDependency { cycle, .. } => {
                assert_eq!(cycle, &["pkg-a", "pkg-b", "pkg-a"]);
            }
            _ => panic!("Expected CircularDependency"),
        }
    }

    #[tokio::test]
    async fn test_missing_recommend_does_not_mask_hard_dependency() {
        // A recommends missing-pkg, B depends on missing-pkg
        // The missing recommend should NOT prevent B from getting a MissingDependency error
        let mut repo = MockPackageRepository::new();
        repo.expect_get_package()
            .withf(|name| name == "pkg-a")
            .returning(|_| {
                Ok(mock_package_with_recommends(
                    "pkg-a",
                    &["pkg-b"],
                    &["missing-pkg"],
                ))
            });
        repo.expect_get_package()
            .withf(|name| name == "pkg-b")
            .returning(|_| Ok(mock_package("pkg-b", &["missing-pkg"])));
        repo.expect_get_package()
            .withf(|name| name == "missing-pkg")
            .returning(|_| {
                Err(crate::package::port::PackageError::PackageNotFound {
                    name: "missing-pkg".to_string(),
                    packages_path: std::path::PathBuf::from("/tmp"),
                    files_examined: 0,
                    search_patterns: vec![],
                }
                .into())
            });

        let sender = make_sender();
        let result = resolve_dependencies("pkg-a", &repo, "test", &sender).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.is_dependency_error());
        match err.dependency_failure().unwrap() {
            crate::package::event::DependencyFailure::MissingDependency {
                dependency_name, ..
            } => {
                assert_eq!(dependency_name, "missing-pkg");
            }
            _ => panic!("Expected MissingDependency"),
        }
    }

    #[tokio::test]
    async fn test_missing_recommend_is_not_an_error() {
        // A recommends a package that doesn't exist — should not fail cycle detection
        let mut repo = MockPackageRepository::new();
        repo.expect_get_package()
            .withf(|name| name == "pkg-a")
            .returning(|_| Ok(mock_package_with_recommends("pkg-a", &[], &["missing-rec"])));
        repo.expect_get_package()
            .withf(|name| name == "missing-rec")
            .returning(|_| {
                Err(crate::package::port::PackageError::PackageNotFound {
                    name: "missing-rec".to_string(),
                    packages_path: std::path::PathBuf::from("/tmp"),
                    files_examined: 0,
                    search_patterns: vec![],
                }
                .into())
            });

        let sender = make_sender();
        let graph = resolve_dependencies("pkg-a", &repo, "test", &sender)
            .await
            .unwrap();

        assert_eq!(graph.install_order, vec!["pkg-a"]);
    }

    // A package the recommend walk refused must still be refused when something
    // depends on it hard.
    //
    // This is what the `visit_state.remove` in the recommend walk protects.
    // Leaving the entry behind marks a package on the strength of edges nothing
    // read: the walk returns early without recording it, and the later hard
    // visit either reports a cycle that does not exist or skips a package the
    // install needs. The second is the shape of a bug already open against this
    // function, so the cleanup is not a tidy-up.
    #[tokio::test]
    async fn a_refused_recommend_is_still_refused_as_a_hard_dependency() {
        // Parsed from text, not built: the rule reads the file's own top level,
        // and a package assembled in memory has none to read.
        fn refused(name: &str) -> GetPackage {
            let yaml = format!(
                "name: {name}\n_environments:\n  test:\n    install: \"echo decoy\"\nenvironments:\n  test:\n    install: \"echo real\"\n"
            );
            let mut pkg: crate::package::Package =
                crate::yaml::parse(&yaml).expect("fixture must parse");
            pkg.set_source(
                std::path::PathBuf::from(format!("/tmp/{name}.yml")),
                yaml,
                crate::package::SpecOrigin::PackageDirectory,
            );
            GetPackage {
                package: pkg,
                file_path: std::path::PathBuf::from(format!("/tmp/{name}.yml")),
                is_new: false,
            }
        }

        // root -> [pkg-a, pkg-b]; pkg-a recommends pkg-r; pkg-b depends on pkg-r.
        // The recommend walk reaches pkg-r first and declines to judge it.
        let mut repo = MockPackageRepository::new();
        repo.expect_get_package()
            .withf(|name| name == "root")
            .returning(|_| Ok(mock_package("root", &["pkg-a", "pkg-b"])));
        repo.expect_get_package()
            .withf(|name| name == "pkg-a")
            .returning(|_| Ok(mock_package_with_recommends("pkg-a", &[], &["pkg-r"])));
        repo.expect_get_package()
            .withf(|name| name == "pkg-b")
            .returning(|_| Ok(mock_package("pkg-b", &["pkg-r"])));
        repo.expect_get_package()
            .withf(|name| name == "pkg-r")
            .returning(|_| Ok(refused("pkg-r")));

        let sender = make_sender();
        let error = resolve_dependencies("root", &repo, "test", &sender)
            .await
            .expect_err("a hard dependency selfie will not read must fail the resolution");

        let rendered = format!("{error:?}");
        assert!(
            rendered.contains("UnreadableSpec"),
            "the failure must name the spec it would not read, not a cycle: {rendered}"
        );
        assert!(
            rendered.contains("pkg-r"),
            "the failure must name the package: {rendered}"
        );
    }
}
