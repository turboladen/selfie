//! Dependency graph resolution for package installation.
//!
//! Resolves package dependencies into a topological install order and detects
//! circular dependencies using DFS with three-state visit tracking.

use crate::package::{
    event::{EventSender, OperationFailure},
    port::PackageRepository,
};

/// The result of resolving a package's dependency graph.
#[derive(Debug, Clone)]
pub(crate) struct DependencyGraph {
    /// Packages in topological install order (dependencies first, target last).
    pub install_order: Vec<String>,
}

/// Visit state for cycle detection during DFS traversal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VisitState {
    /// Not yet visited.
    Unvisited,
    /// Currently on the DFS stack — encountering this again means a cycle.
    Visiting,
    /// Fully explored — all descendants processed.
    Visited,
}

/// Resolve the dependency graph for `root_package`, returning a topological
/// install order or an `OperationFailure` on cycle / missing dependency.
pub(crate) async fn resolve_dependencies<PR>(
    root_package: &str,
    repo: &PR,
    config_environment: &str,
    sender: &EventSender,
) -> Result<DependencyGraph, OperationFailure>
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

    dfs(
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

    Ok(DependencyGraph { install_order })
}

/// Recursive DFS that builds `install_order` bottom-up and detects cycles.
fn dfs<'a, PR>(
    package_name: &'a str,
    repo: &'a PR,
    config_environment: &'a str,
    sender: &'a EventSender,
    visit_state: &'a mut std::collections::HashMap<String, VisitState>,
    install_order: &'a mut Vec<String>,
    path: &'a mut Vec<String>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), OperationFailure>> + Send + 'a>>
where
    PR: PackageRepository + Sync,
{
    Box::pin(async move {
        let state = visit_state
            .get(package_name)
            .copied()
            .unwrap_or(VisitState::Unvisited);

        match state {
            VisitState::Visited => return Ok(()),
            VisitState::Visiting => {
                // Build the cycle path from where the cycle starts.
                // The path already ends with package_name (pushed by the caller),
                // so path[cycle_start..] gives e.g. [A, B, A] for A->B->A.
                let cycle_start = path
                    .iter()
                    .position(|n| n == package_name)
                    .expect("package must be on path when in Visiting state");
                let cycle: Vec<String> = path[cycle_start..].to_vec();

                return Err(OperationFailure::circular_dependency(
                    package_name.to_string(),
                    cycle,
                ));
            }
            VisitState::Unvisited => {}
        }

        visit_state.insert(package_name.to_string(), VisitState::Visiting);

        // Load the package to discover its dependencies
        let package_blob = repo.get_package(package_name).map_err(|repo_err| {
            // For transitive deps, report as missing dependency with the parent name.
            // For the root package (path has only itself), propagate the repo error
            // so the caller gets a proper PackageNotFound.
            if path.len() >= 2 {
                let parent = path[path.len() - 2].clone();
                OperationFailure::missing_dependency(parent, package_name.to_string())
            } else {
                OperationFailure::from(repo_err)
            }
        })?;

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

        Ok(())
    })
}

/// Walk a recommend's dependency graph for cycle detection only.
///
/// Unlike `dfs`, this does NOT add packages to `install_order`. It only checks
/// for cycles by examining `Visiting` state. Packages already `Visited` by
/// the main DFS are safely skipped.
///
/// Note: This intentionally only traverses hard `dependencies` of the recommend,
/// NOT recommends-of-recommends. Since recommends are one-level deep (we never
/// install recommends of recommends), those transitive recommends can't create
/// runtime cycles.
fn check_recommend_cycles<'a, PR>(
    package_name: &'a str,
    repo: &'a PR,
    config_environment: &'a str,
    _sender: &'a EventSender,
    visit_state: &'a mut std::collections::HashMap<String, VisitState>,
    path: &'a mut Vec<String>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), OperationFailure>> + Send + 'a>>
where
    PR: PackageRepository + Sync,
{
    Box::pin(async move {
        let state = visit_state
            .get(package_name)
            .copied()
            .unwrap_or(VisitState::Unvisited);

        match state {
            // Already fully processed — no cycle through this node
            VisitState::Visited => return Ok(()),
            // Currently on the DFS stack — cycle detected
            VisitState::Visiting => {
                let cycle_start = path
                    .iter()
                    .position(|n| n == package_name)
                    .expect("package must be on path when in Visiting state");
                let cycle: Vec<String> = path[cycle_start..].to_vec();

                return Err(OperationFailure::circular_dependency(
                    package_name.to_string(),
                    cycle,
                ));
            }
            VisitState::Unvisited => {}
        }

        // Mark visiting for cycle detection
        visit_state.insert(package_name.to_string(), VisitState::Visiting);

        // Try loading the package — if it doesn't exist, silently skip
        // (the recommend will fail gracefully at install time)
        let Ok(package_blob) = repo.get_package(package_name) else {
            visit_state.insert(package_name.to_string(), VisitState::Visited);
            return Ok(());
        };

        // Check hard dependencies of this recommend for cycles
        let deps: Vec<String> = package_blob
            .package
            .environments()
            .get(config_environment)
            .map(|env| env.dependencies.clone())
            .unwrap_or_default();

        for dep in &deps {
            path.push(dep.clone());
            check_recommend_cycles(dep, repo, config_environment, _sender, visit_state, path)
                .await?;
            path.pop();
        }

        visit_state.insert(package_name.to_string(), VisitState::Visited);
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
            .version("1.0.0")
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
}
