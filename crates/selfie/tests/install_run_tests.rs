// How one install run treats its recommends: the order they install in, the
// progress each one reports, and the packages an earlier graph in the run
// settled, which a recommend neither checks nor installs again.

use std::path::Path;

use selfie::package::{
    event::{OperationResult, Outcome, PackageEvent, Settled},
    service::{InstallOptions, PackageService},
};
use tempfile::TempDir;
use test_common::{
    FakeCommandRunner, SelfieConfigBuilder, collect_events,
    create_service_test_service_with_runner as service, create_service_with_config_and_runner,
    get_operation_result,
};

// Writes `<name>.yml` with `fields` added to its `test` environment, which
// installs with `install-<name>`.
fn write_spec(dir: &Path, name: &str, fields: &str) {
    std::fs::write(
        dir.join(format!("{name}.yml")),
        format!("name: {name}\nenvironments:\n  test:\n    install: \"install-{name}\"\n{fields}"),
    )
    .unwrap();
}

// Installs `pkg` from the specs in `temp`.
async fn install(temp: &TempDir, runner: FakeCommandRunner) -> Vec<PackageEvent> {
    collect_events(
        service(temp, runner)
            .install("pkg", InstallOptions::default())
            .await,
    )
    .await
}

// A runner on which `install-<name>` succeeds for each of `names`.
fn installing(names: &[&str]) -> FakeCommandRunner {
    names.iter().fold(FakeCommandRunner::new(), |runner, name| {
        runner.succeeding(&format!("install-{name}"), b"")
    })
}

// How many times `runner` was asked to run `command`.
fn runs(runner: &FakeCommandRunner, command: &str) -> usize {
    runner
        .calls()
        .iter()
        .filter(|(called, _)| called == command)
        .count()
}

// Each `SettledEarlier` event as (package, recommend, how), in order.
fn settled_earlier(events: &[PackageEvent]) -> Vec<(String, String, Settled)> {
    events
        .iter()
        .filter_map(|event| match event {
            PackageEvent::SettledEarlier {
                package_name,
                recommend_name,
                settled,
                ..
            } => Some((package_name.clone(), recommend_name.clone(), *settled)),
            _ => None,
        })
        .collect()
}

fn settled(package: &str, recommend: &str, how: Settled) -> (String, String, Settled) {
    (package.to_string(), recommend.to_string(), how)
}

// The run's outcome, which must be a success.
fn outcome(events: &[PackageEvent]) -> Outcome {
    match get_operation_result(events) {
        Some(OperationResult::Success(success)) => success.outcome(),
        other => panic!("the run must complete successfully: {other:?}"),
    }
}

fn recommend_succeeded(events: &[PackageEvent], recommend: &str) -> bool {
    events.iter().any(|event| {
        matches!(
            event,
            PackageEvent::RecommendSucceeded { recommend_name, .. } if recommend_name == recommend
        )
    })
}

// The error each `RecommendFailed` event carries, by recommend, in order.
fn recommend_failures(events: &[PackageEvent]) -> Vec<(String, String)> {
    events
        .iter()
        .filter_map(|event| match event {
            PackageEvent::RecommendFailed {
                recommend_name,
                error,
                ..
            } => Some((recommend_name.clone(), error.clone())),
            _ => None,
        })
        .collect()
}

// Returns each progress event's step and total, from the one after the
// `RecommendStarted` naming `recommend` up to the next recommend event.
fn recommend_progress(events: &[PackageEvent], recommend: &str) -> Vec<(usize, usize)> {
    events
        .iter()
        .skip_while(|event| {
            !matches!(
                event,
                PackageEvent::RecommendStarted { recommend_name, .. } if recommend_name == recommend
            )
        })
        .skip(1)
        .take_while(|event| {
            !matches!(
                event,
                PackageEvent::RecommendStarted { .. }
                    | PackageEvent::RecommendSucceeded { .. }
                    | PackageEvent::RecommendFailed { .. }
            )
        })
        .filter_map(|event| match event {
            PackageEvent::Progress {
                step, total_steps, ..
            } => Some((*step, *total_steps)),
            _ => None,
        })
        .collect()
}

// A recommend with dependencies of its own counts every package of its graph,
// so its last step is its total and no step runs past it.
#[tokio::test]
async fn a_recommends_progress_ends_at_its_total() {
    let temp = TempDir::new().unwrap();
    write_spec(temp.path(), "pkg", "    recommends:\n      - rec\n");
    write_spec(
        temp.path(),
        "rec",
        "    dependencies:\n      - a\n      - b\n",
    );
    write_spec(temp.path(), "a", "");
    write_spec(temp.path(), "b", "");
    let runner = ["install-pkg", "install-rec", "install-a", "install-b"]
        .into_iter()
        .fold(FakeCommandRunner::new(), |runner, command| {
            runner.succeeding(command, b"")
        });

    let events = install(&temp, runner).await;

    assert!(
        matches!(
            get_operation_result(&events),
            Some(OperationResult::Success(_))
        ),
        "{events:#?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            PackageEvent::RecommendSucceeded { recommend_name, .. } if recommend_name == "rec"
        )),
        "{events:#?}"
    );
    let progress = recommend_progress(&events, "rec");
    let Some(&(last, total)) = progress.last() else {
        panic!("the recommend reported no progress: {events:#?}");
    };
    assert_eq!(last, total, "{progress:?}");
    assert!(
        progress.iter().all(|&(step, total)| step <= total),
        "{progress:?}"
    );
    // Three packages of seven steps each, so the control fails if a total
    // counts the recommend alone.
    assert_eq!(total, 21, "{progress:?}");
}

// Recommends install one at a time, in the order the package lists them,
// whatever `max_concurrency` allows. The runner yields before each answer, as
// a running command does, so recommends awaited together would interleave.
#[tokio::test]
async fn recommends_install_one_at_a_time_in_the_order_listed() {
    let temp = TempDir::new().unwrap();
    write_spec(
        temp.path(),
        "pkg",
        "    recommends:\n      - r1\n      - r2\n      - r3\n",
    );
    for name in ["r1", "r2", "r3"] {
        write_spec(temp.path(), name, "");
    }
    let runner = ["install-pkg", "install-r1", "install-r2", "install-r3"]
        .into_iter()
        .fold(FakeCommandRunner::new().yielding(), |runner, command| {
            runner.succeeding(command, b"")
        });
    let config = SelfieConfigBuilder::default()
        .environment("test")
        .package_directory(temp.path())
        .max_concurrency_unchecked(4)
        .build();

    let events = collect_events(
        create_service_with_config_and_runner(config, runner.clone())
            .install("pkg", InstallOptions::default())
            .await,
    )
    .await;

    let recommend_events: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            PackageEvent::RecommendStarted { recommend_name, .. } => {
                Some(format!("started {recommend_name}"))
            }
            PackageEvent::RecommendSucceeded { recommend_name, .. } => {
                Some(format!("succeeded {recommend_name}"))
            }
            PackageEvent::RecommendFailed { recommend_name, .. } => {
                Some(format!("failed {recommend_name}"))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        recommend_events,
        [
            "started r1",
            "succeeded r1",
            "started r2",
            "succeeded r2",
            "started r3",
            "succeeded r3",
        ],
        "{events:#?}"
    );
    let installs: Vec<String> = runner
        .calls()
        .into_iter()
        .map(|(command, _)| command)
        .filter(|command| command.starts_with("install-r"))
        .collect();
    assert_eq!(installs, ["install-r1", "install-r2", "install-r3"]);
}

// A recommend counts only the packages it installs itself: one the root
// settled leaves its total, and a recommend the root settled whole reports no
// progress at all.
#[tokio::test]
async fn a_recommend_counts_only_the_packages_it_installs() {
    let temp = TempDir::new().unwrap();
    write_spec(
        temp.path(),
        "pkg",
        "    dependencies:\n      - a\n      - whole\n    recommends:\n      - rec\n      - whole\n",
    );
    write_spec(
        temp.path(),
        "rec",
        "    dependencies:\n      - a\n      - b\n",
    );
    for name in ["a", "b", "whole"] {
        write_spec(temp.path(), name, "");
    }
    let runner = installing(&["pkg", "rec", "a", "b", "whole"]);

    let events = install(&temp, runner).await;

    let progress = recommend_progress(&events, "rec");
    assert_eq!(progress.last(), Some(&(14, 14)), "{progress:?}");
    assert!(
        progress.iter().all(|&(step, total)| step <= total),
        "{progress:?}"
    );
    assert!(recommend_succeeded(&events, "whole"), "{events:#?}");
    assert_eq!(recommend_progress(&events, "whole"), []);
}

// A dependency the root's graph found installed is not checked again by a
// recommend that also needs it. The recommend says it settled earlier, and
// that changes nothing about the run.
#[tokio::test]
async fn a_dependency_the_root_settled_is_not_checked_again_by_a_recommend() {
    let temp = TempDir::new().unwrap();
    write_spec(
        temp.path(),
        "pkg",
        "    dependencies:\n      - dep\n    recommends:\n      - rec\n",
    );
    write_spec(temp.path(), "rec", "    dependencies:\n      - dep\n");
    write_spec(temp.path(), "dep", "    check: \"check-dep\"\n");
    let runner = installing(&["pkg", "rec"]).succeeding("check-dep", b"");

    let events = install(&temp, runner.clone()).await;

    assert_eq!(runs(&runner, "check-dep"), 1, "{:?}", runner.calls());
    assert_eq!(runs(&runner, "install-dep"), 0, "{:?}", runner.calls());
    assert_eq!(
        settled_earlier(&events),
        [settled("dep", "rec", Settled::AlreadyInstalled)]
    );
    assert!(recommend_succeeded(&events, "rec"), "{events:#?}");
    assert_eq!(outcome(&events), Outcome::Clean);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, PackageEvent::Warning { .. })),
        "{events:#?}"
    );
}

// Two recommends that need one package install it once, under the first; the
// second says it settled earlier.
#[tokio::test]
async fn two_recommends_sharing_a_dependency_install_it_once() {
    let temp = TempDir::new().unwrap();
    write_spec(
        temp.path(),
        "pkg",
        "    recommends:\n      - a\n      - b\n",
    );
    write_spec(temp.path(), "a", "    dependencies:\n      - e\n");
    write_spec(temp.path(), "b", "    dependencies:\n      - e\n");
    write_spec(temp.path(), "e", "");
    let runner = installing(&["pkg", "a", "b", "e"]);

    let events = install(&temp, runner.clone()).await;

    assert_eq!(runs(&runner, "install-e"), 1, "{:?}", runner.calls());
    assert!(recommend_succeeded(&events, "a"), "{events:#?}");
    assert!(recommend_succeeded(&events, "b"), "{events:#?}");
    assert_eq!(
        settled_earlier(&events),
        [settled("e", "b", Settled::Installed)]
    );
}

// A failure is never shared: the next recommend that needs the package runs
// its install again itself, and each failure is about a command that
// recommend ran.
#[tokio::test]
async fn a_dependency_that_failed_in_one_recommend_is_tried_again_by_the_next() {
    let temp = TempDir::new().unwrap();
    write_spec(
        temp.path(),
        "pkg",
        "    recommends:\n      - a\n      - b\n",
    );
    write_spec(temp.path(), "a", "    dependencies:\n      - e\n");
    write_spec(temp.path(), "b", "    dependencies:\n      - e\n");
    write_spec(temp.path(), "e", "");
    let runner = installing(&["pkg", "a", "b"]).failing("install-e", b"no such formula");

    let events = install(&temp, runner.clone()).await;

    assert_eq!(runs(&runner, "install-e"), 2, "{:?}", runner.calls());
    let failures = recommend_failures(&events);
    let failed: Vec<&str> = failures.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(failed, ["a", "b"], "{events:#?}");
    for (name, error) in &failures {
        assert!(error.contains("install-e"), "{name}: {error}");
    }
    assert_eq!(settled_earlier(&events), []);
}

// A recommend the root depends on settled with the root's graph, so it runs
// nothing and still reports success.
#[tokio::test]
async fn a_recommend_the_root_installed_runs_nothing_and_succeeds() {
    let temp = TempDir::new().unwrap();
    write_spec(
        temp.path(),
        "pkg",
        "    dependencies:\n      - a\n    recommends:\n      - a\n",
    );
    write_spec(temp.path(), "a", "");
    let runner = installing(&["pkg", "a"]);

    let events = install(&temp, runner.clone()).await;

    assert_eq!(runs(&runner, "install-a"), 1, "{:?}", runner.calls());
    assert_eq!(
        settled_earlier(&events),
        [settled("a", "a", Settled::Installed)]
    );
    assert!(recommend_succeeded(&events, "a"), "{events:#?}");
}

// A package whose install command is blank is installed only by already being
// installed. Once its check said so, no later graph asks again.
#[tokio::test]
async fn a_shared_check_first_dependency_is_checked_once() {
    let temp = TempDir::new().unwrap();
    write_spec(
        temp.path(),
        "pkg",
        "    dependencies:\n      - d\n    recommends:\n      - rec\n",
    );
    write_spec(temp.path(), "rec", "    dependencies:\n      - d\n");
    std::fs::write(
        temp.path().join("d.yml"),
        "name: d\nenvironments:\n  test:\n    install: \"\"\n    check: \"check-d\"\n",
    )
    .unwrap();
    let runner = installing(&["pkg", "rec"]).succeeding("check-d", b"");

    let events = install(&temp, runner.clone()).await;

    assert_eq!(runs(&runner, "check-d"), 1, "{:?}", runner.calls());
    assert_eq!(
        settled_earlier(&events),
        [settled("d", "rec", Settled::AlreadyInstalled)]
    );
}

// A recommend refused before it installs anything reused nothing, so it says
// nothing about the package it shares with the root.
#[tokio::test]
async fn a_refused_recommend_reports_no_reuse() {
    let temp = TempDir::new().unwrap();
    write_spec(
        temp.path(),
        "pkg",
        "    dependencies:\n      - dep\n    recommends:\n      - rec\n",
    );
    write_spec(
        temp.path(),
        "rec",
        "    dependencies:\n      - dep\n      - bad\n",
    );
    write_spec(temp.path(), "dep", "");
    // A blank install and no check: nothing can install it here.
    std::fs::write(
        temp.path().join("bad.yml"),
        "name: bad\nenvironments:\n  test:\n    install: \"\"\n",
    )
    .unwrap();
    let runner = installing(&["pkg", "rec", "dep"]);

    let events = install(&temp, runner.clone()).await;

    assert_eq!(runs(&runner, "install-dep"), 1, "{:?}", runner.calls());
    let failed: Vec<String> = recommend_failures(&events)
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(failed, ["rec"], "{events:#?}");
    assert_eq!(settled_earlier(&events), []);
}
