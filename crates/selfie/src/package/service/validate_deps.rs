//! Dependency-graph checks for `spec validate`.
//!
//! A package-directory spec is resolved the way `package install` resolves it,
//! in every environment it declares, so validation reports what install would
//! refuse.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::package::{
    Package, SpecOrigin, environment_field,
    event::{EventSender, OperationFailure},
    port::{LookupFailure, PackageRepository},
    service::deps::resolve_dependencies,
};
use crate::validation::{IssueMessage, ValidationErrorCategory, ValidationIssue};

/// The dependency issues of each spec in `packages`, keyed by its file.
///
/// A dependency cycle, including one through `recommends`, a dependency that
/// is missing, and a dependency that cannot be installed in an environment the
/// spec declares are errors, since install refuses each. A recommend that is
/// missing or cannot be installed there is a warning, since install only warns.
pub(super) async fn dependency_issues<PR>(
    repo: &PR,
    packages: &[&Package],
    sender: &EventSender,
) -> BTreeMap<PathBuf, Vec<ValidationIssue>>
where
    PR: PackageRepository,
{
    let mut issues: BTreeMap<PathBuf, Vec<ValidationIssue>> = BTreeMap::new();
    for package in packages {
        // A standalone dotfile spec has no environments and is never installed.
        if package.origin() != SpecOrigin::PackageDirectory {
            continue;
        }
        // Install looks a package up by its file name, never by its `name`
        // field, so the root is resolved by that name too.
        let Some(lookup_name) = package.path().file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        for environment in package.environments().keys() {
            // A refused spec's edges cannot be trusted, and the refusal is
            // already reported as an error of its own.
            if package.is_refused(environment) {
                continue;
            }
            let found = issues_in(repo, lookup_name, environment, sender).await;
            if !found.is_empty() {
                issues
                    .entry(package.path().clone())
                    .or_default()
                    .extend(found);
            }
        }
    }
    issues
}

async fn issues_in<PR>(
    repo: &PR,
    lookup_name: &str,
    environment: &str,
    sender: &EventSender,
) -> Vec<ValidationIssue>
where
    PR: PackageRepository,
{
    let graph = match resolve_dependencies(lookup_name, repo, environment, sender).await {
        Ok(graph) => graph,
        Err(failure) => {
            return match *failure {
                // The environment, not one of its lists: a cycle can run through
                // `dependencies` and `recommends` alike.
                OperationFailure::DependencyError(failure) => vec![ValidationIssue::error(
                    ValidationErrorCategory::Dependency,
                    &format!("environments.{environment}"),
                    in_environment(environment, crate::parts::Message::of(&failure)),
                    None,
                )],
                // Anything else is about the collection rather than this spec's
                // edges, such as a package directory selfie could not list,
                // and collection reports it already.
                _ => Vec::new(),
            };
        }
    };

    let mut issues = Vec::new();
    // The root's own refusal is not a dependency's, and other checks report it.
    for (name, failure) in graph.refusals() {
        if graph.required_by.contains_key(name) {
            issues.push(ValidationIssue::error(
                ValidationErrorCategory::Dependency,
                &environment_field(environment, "dependencies"),
                in_environment(environment, failure_message(failure)),
                None,
            ));
        }
    }
    for recommend in &graph.root_recommends {
        if let Some(message) =
            recommend_refusal(repo, lookup_name, recommend, environment, sender).await
        {
            issues.push(ValidationIssue::warning(
                ValidationErrorCategory::Dependency,
                &environment_field(environment, "recommends"),
                in_environment(environment, message),
                Some("Install skips this recommend with a warning."),
            ));
        }
    }
    issues
}

// Why install would skip `recommend`, if it would. Install resolves the
// recommend's own graph and refuses it as it refuses any package's, so this does
// too.
async fn recommend_refusal<PR>(
    repo: &PR,
    parent: &str,
    recommend: &str,
    environment: &str,
    sender: &EventSender,
) -> Option<crate::parts::Message>
where
    PR: PackageRepository,
{
    if let Err(error) = repo.get_package(recommend) {
        return match error.into_lookup_failure() {
            Ok(LookupFailure::NotFound(_)) => Some(
                crate::parts::Parts::new()
                    .text("Package `")
                    .name(parent)
                    .text("` recommends `")
                    .name(recommend)
                    .text("`, which was not found")
                    .into(),
            ),
            // An unreadable or ambiguous spec is reported on that spec, or by
            // collection.
            _ => None,
        };
    }
    match resolve_dependencies(recommend, repo, environment, sender).await {
        Err(failure) => Some(failure_message(&failure)),
        Ok(graph) => graph
            .refusals()
            .next()
            .map(|(_, failure)| failure_message(failure)),
    }
}

// Every failure's own parts, so a path or a name in it stays a part of its
// own rather than words of the issue's sentence.
fn failure_message(failure: &OperationFailure) -> crate::parts::Message {
    crate::parts::Message::of(failure)
}

// Every environment a spec declares is checked, not only the current one, so the
// sentence names which one the problem is in.
fn in_environment(environment: &str, message: crate::parts::Message) -> IssueMessage {
    use crate::parts::Composed as _;
    IssueMessage::Message(
        crate::parts::Parts::new()
            .text("In environment `")
            .name(environment)
            .text("`: ")
            .append(message.parts())
            .into(),
    )
}

#[cfg(test)]
mod tests {
    use crate::{
        config::SelfieConfigBuilder,
        fs::real::RealFileSystem,
        package::{
            SpecOrigin,
            event::{EventSender, PackageEvent, ValidationIssueData},
            repository::yaml::YamlPackageRepository,
            service::{
                ProgressTracker, validate::handle_validate, validate_all::handle_validate_all,
            },
        },
        validation::{ValidationErrorCategory, ValidationLevel},
    };

    // A failure that names a path, such as a package directory that could not
    // be listed, names it as a part: the fixture path holds U+200B, which
    // `values_of` refuses in a text part.
    #[test]
    fn a_recommends_failure_names_its_path_as_a_part() {
        let listing = crate::package::port::PackageListError::new(
            "/p/pack\u{200b}ages".into(),
            crate::fs::filesystem::DirectoryState::Unlistable(std::sync::Arc::new(
                std::io::Error::other("denied"),
            )),
        );
        let message = super::failure_message(&super::OperationFailure::PackageList(listing));
        assert_eq!(crate::parts::values_of(&message), ["/p/pack\u{200b}ages"]);
    }

    // Each `(file stem, spec)` written to a package directory, then every
    // dependency issue `spec validate --all` reports, run in `environment`, as
    // (spec name, level, sentence).
    async fn dependency_issues_of(
        specs: &[(&str, &str)],
        environment: &str,
    ) -> Vec<(String, ValidationLevel, String)> {
        dependency_issues_from(specs, environment, None).await
    }

    // As `dependency_issues_of`, or of `spec validate <only>` when `only` names
    // one spec.
    async fn dependency_issues_from(
        specs: &[(&str, &str)],
        environment: &str,
        only: Option<&str>,
    ) -> Vec<(String, ValidationLevel, String)> {
        let temp = tempfile::TempDir::new().unwrap();
        let packages = temp.path().join("packages");
        let dotfiles = temp.path().join("dotfiles");
        std::fs::create_dir_all(&packages).unwrap();
        std::fs::create_dir_all(&dotfiles).unwrap();
        for (stem, spec) in specs {
            std::fs::write(packages.join(format!("{stem}.yml")), spec).unwrap();
        }
        let config = SelfieConfigBuilder::default()
            .environment(environment)
            .package_directory(&packages)
            .dotfiles_directory(dotfiles.clone())
            .build();
        let repo = YamlPackageRepository::new(
            RealFileSystem,
            packages.clone(),
            SpecOrigin::PackageDirectory,
        );
        let dotfiles_repo =
            YamlPackageRepository::new(RealFileSystem, dotfiles, SpecOrigin::DotfilesDirectory);
        // Large enough that the run never waits on a receiver nobody drains yet.
        let (tx, mut rx) = tokio::sync::mpsc::channel(4096);
        let sender = EventSender::new_with_context(
            tx,
            crate::package::event::metadata::OperationType::SpecValidateAll,
            String::new(),
            environment.to_string(),
            crate::package::event::OperationContext::default(),
        );
        match only {
            Some(name) => {
                handle_validate(
                    name,
                    &repo,
                    &dotfiles_repo,
                    &config,
                    &sender,
                    &mut ProgressTracker::new(3),
                )
                .await
            }
            None => {
                handle_validate_all(
                    &repo,
                    &dotfiles_repo,
                    &config,
                    &sender,
                    &mut ProgressTracker::new(2),
                )
                .await
            }
        };
        drop(sender);

        let mut found = Vec::new();
        while let Some(event) = rx.recv().await {
            if let PackageEvent::ValidationResultCompleted {
                validation_result, ..
            } = event
            {
                for ValidationIssueData {
                    category,
                    level,
                    message,
                    ..
                } in validation_result.issues
                {
                    if category == ValidationErrorCategory::Dependency {
                        found.push((
                            validation_result.package_name.clone(),
                            level,
                            message.to_string(),
                        ));
                    }
                }
            }
        }
        found.sort_by(|a, b| a.0.cmp(&b.0).then(a.2.cmp(&b.2)));
        found
    }

    fn spec(name: &str, environments: &str) -> String {
        format!("name: {name}\nenvironments:\n{environments}")
    }

    #[tokio::test]
    async fn a_cycle_through_a_recommend_is_an_error_on_each_member() {
        let fish = spec(
            "fish",
            "  home:\n    install: \"true\"\n    recommends: [fish-fisher]\n",
        );
        let fisher = spec(
            "fish-fisher",
            "  home:\n    install: \"true\"\n    dependencies: [fish]\n",
        );

        let issues =
            dependency_issues_of(&[("fish", &fish), ("fish-fisher", &fisher)], "home").await;

        assert_eq!(issues.len(), 2, "{issues:#?}");
        for (name, level, message) in &issues {
            assert_eq!(*level, ValidationLevel::Error, "{name}: {message}");
            assert!(message.contains("Circular dependency"), "{name}: {message}");
            assert!(message.contains("`home`"), "{name}: {message}");
        }
    }

    #[tokio::test]
    async fn validating_one_spec_reports_its_own_cycle() {
        let fish = spec(
            "fish",
            "  home:\n    install: \"true\"\n    recommends: [fish-fisher]\n",
        );
        let fisher = spec(
            "fish-fisher",
            "  home:\n    install: \"true\"\n    dependencies: [fish]\n",
        );

        let issues = dependency_issues_from(
            &[("fish", &fish), ("fish-fisher", &fisher)],
            "home",
            Some("fish"),
        )
        .await;

        let [(name, level, message)] = issues.as_slice() else {
            panic!("expected one issue, got {issues:#?}");
        };
        assert_eq!(name, "fish");
        assert_eq!(*level, ValidationLevel::Error);
        assert!(message.contains("Circular dependency"), "{message}");
    }

    #[tokio::test]
    async fn a_dependency_with_no_entry_for_the_environment_is_an_error() {
        let beads = spec(
            "beads",
            "  work:\n    install: \"true\"\n    dependencies: [dolt]\n  home:\n    install: \"true\"\n    dependencies: [dolt]\n",
        );
        let dolt = spec("dolt", "  home:\n    install: \"true\"\n");

        // Run on the home machine, where both install: the work environment is
        // still checked, which is how the problem is found before the work
        // machine meets it.
        let issues = dependency_issues_of(&[("beads", &beads), ("dolt", &dolt)], "home").await;

        let [(name, level, message)] = issues.as_slice() else {
            panic!("expected one issue, got {issues:#?}");
        };
        assert_eq!(name, "beads");
        assert_eq!(*level, ValidationLevel::Error);
        assert!(message.contains("`work`"), "{message}");
        assert!(message.contains("dolt"), "{message}");
        assert!(!message.contains("`home`"), "{message}");
    }

    #[tokio::test]
    async fn a_missing_dependency_is_an_error() {
        let app = spec(
            "app",
            "  home:\n    install: \"true\"\n    dependencies: [nowhere]\n",
        );

        let issues = dependency_issues_of(&[("app", &app)], "home").await;

        let [(name, level, message)] = issues.as_slice() else {
            panic!("expected one issue, got {issues:#?}");
        };
        assert_eq!(name, "app");
        assert_eq!(*level, ValidationLevel::Error);
        assert!(message.contains("nowhere"), "{message}");
    }

    #[tokio::test]
    async fn a_recommend_install_would_skip_is_a_warning() {
        let app = spec(
            "app",
            "  home:\n    install: \"true\"\n    recommends: [extra, absent]\n",
        );
        let extra = spec("extra", "  work:\n    install: \"true\"\n");

        let issues = dependency_issues_of(&[("app", &app), ("extra", &extra)], "home").await;

        assert_eq!(issues.len(), 2, "{issues:#?}");
        for (name, level, message) in &issues {
            assert_eq!(name, "app");
            assert_eq!(*level, ValidationLevel::Warning, "{message}");
        }
        assert!(
            issues
                .iter()
                .any(|(_, _, m)| m.contains("absent") && m.contains("not found"))
        );
        assert!(issues.iter().any(|(_, _, m)| m.contains("extra")));
    }

    // Install resolves a recommend's own graph before it installs one, so a
    // recommend whose dependency is missing is skipped like one that is.
    #[tokio::test]
    async fn a_recommend_whose_own_dependency_is_missing_is_a_warning() {
        let app = spec(
            "app",
            "  home:\n    install: \"true\"\n    recommends: [plugin]\n",
        );
        let plugin = spec(
            "plugin",
            "  home:\n    install: \"true\"\n    dependencies: [nowhere]\n",
        );

        let issues = dependency_issues_of(&[("app", &app), ("plugin", &plugin)], "home").await;

        let warning = issues
            .iter()
            .find(|(name, _, _)| name == "app")
            .unwrap_or_else(|| panic!("app must be warned about: {issues:#?}"));
        assert_eq!(warning.1, ValidationLevel::Warning, "{issues:#?}");
        assert!(warning.2.contains("nowhere"), "{issues:#?}");
    }

    // A spec the run does not validate, because it declares only another
    // environment, is reported by neither validation nor this check.
    #[tokio::test]
    async fn a_spec_the_run_leaves_out_reports_nothing() {
        let tool = spec(
            "tool",
            "  work:\n    install: \"true\"\n    dependencies: [dolt]\n",
        );
        let dolt = spec("dolt", "  home:\n    install: \"true\"\n");

        let on_home = dependency_issues_of(&[("tool", &tool), ("dolt", &dolt)], "home").await;
        let on_work = dependency_issues_of(&[("tool", &tool), ("dolt", &dolt)], "work").await;

        assert!(on_home.is_empty(), "{on_home:#?}");
        assert_eq!(
            on_work.len(),
            1,
            "the control: on work it is found: {on_work:#?}"
        );
    }

    // The control: a graph install resolves, with a shared dependency and a
    // recommend that depends on what recommends it only through another
    // package, reports nothing.
    #[tokio::test]
    async fn a_graph_install_resolves_reports_nothing() {
        let app = spec(
            "app",
            "  home:\n    install: \"true\"\n    dependencies: [lib, tool]\n    recommends: [plugin]\n",
        );
        let tool = spec(
            "tool",
            "  home:\n    install: \"true\"\n    dependencies: [lib]\n",
        );
        let lib = spec("lib", "  home:\n    install: \"true\"\n");
        let plugin = spec(
            "plugin",
            "  home:\n    install: \"true\"\n    dependencies: [lib]\n",
        );

        let issues = dependency_issues_of(
            &[
                ("app", &app),
                ("tool", &tool),
                ("lib", &lib),
                ("plugin", &plugin),
            ],
            "home",
        )
        .await;

        assert!(issues.is_empty(), "{issues:#?}");
    }
}
