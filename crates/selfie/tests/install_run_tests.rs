// How one install run treats its recommends: the progress each one reports.

use std::path::Path;

use selfie::package::{
    event::{OperationResult, PackageEvent},
    service::{InstallOptions, PackageService},
};
use tempfile::TempDir;
use test_common::{
    FakeCommandRunner, collect_events, create_service_test_service_with_runner as service,
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
