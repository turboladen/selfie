// Which progress steps wait on something outside selfie.
//
// A step is `Waiting` only where a configured command actually starts, and its
// message names what it waits on. Each site has a control where no command
// runs, so a site that marks every step `Waiting` fails too.

use tempfile::TempDir;
use test_common::{collect_events, create_service_test_service};

use selfie::package::{
    event::{PackageEvent, StepEnding, StepKind},
    service::{InstallOptions, PackageService},
};

fn write_spec(dir: &TempDir, name: &str, environment_body: &str) {
    std::fs::write(
        dir.path().join(format!("{name}.yaml")),
        format!("name: {name}\nenvironments:\n  test:\n{environment_body}"),
    )
    .unwrap();
}

// The message of each waiting step. Also asserts that every waiting step ends
// exactly once, after it starts, so each test pins that for its site.
fn waiting(events: &[PackageEvent]) -> Vec<String> {
    let mut messages = Vec::new();
    for (at, event) in events.iter().enumerate() {
        if let PackageEvent::Progress {
            kind: StepKind::Waiting(id),
            message,
            ..
        } = event
        {
            let ends: Vec<usize> = events
                .iter()
                .enumerate()
                .filter(|(_, e)| matches!(e, PackageEvent::StepEnded { step, .. } if step == id))
                .map(|(i, _)| i)
                .collect();
            assert_eq!(ends.len(), 1, "{message:?} must end once: {events:#?}");
            assert!(ends[0] > at, "{message:?} ends before it starts");
            messages.push(message.clone());
        }
    }
    messages
}

fn endings(events: &[PackageEvent]) -> Vec<StepEnding> {
    events
        .iter()
        .filter_map(|e| match e {
            PackageEvent::StepEnded { ending, .. } => Some(*ending),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_check_waits_on_its_check_command_and_names_the_package() {
    let dir = TempDir::new().unwrap();
    write_spec(&dir, "bat", "    install: \"true\"\n    check: \"true\"\n");
    let service = create_service_test_service(&dir);

    let events = collect_events(service.check("bat").await).await;

    let waits = waiting(&events);
    assert_eq!(waits.len(), 1, "{events:#?}");
    assert!(waits[0].contains("check command for bat"), "{waits:?}");
}

// Control: an install asks whether the package is already installed, through
// the same check step, before it knows whether a check command exists. With
// none, that step runs nothing, so the install command is the only wait.
#[tokio::test]
async fn a_check_step_with_no_check_command_does_not_wait() {
    let dir = TempDir::new().unwrap();
    write_spec(&dir, "bat", "    install: \"true\"\n");
    let service = create_service_test_service(&dir);

    let events = collect_events(service.install("bat", InstallOptions::default()).await).await;

    let waits = waiting(&events);
    assert_eq!(waits.len(), 1, "{events:#?}");
    assert!(waits[0].contains("install command for bat"), "{waits:?}");
}

#[tokio::test]
async fn an_audit_waits_on_its_audit_command_and_names_the_package() {
    let dir = TempDir::new().unwrap();
    write_spec(
        &dir,
        "bat",
        "    install: \"true\"\n    audit: \"echo brew\"\n",
    );
    let service = create_service_test_service(&dir);

    let events = collect_events(service.audit("bat").await).await;

    let waits = waiting(&events);
    assert_eq!(waits.len(), 1, "{events:#?}");
    assert!(waits[0].contains("audit command for bat"), "{waits:?}");
}

// Control: with no audit command nothing runs, so nothing waits.
#[tokio::test]
async fn an_audit_with_no_audit_command_does_not_wait() {
    let dir = TempDir::new().unwrap();
    write_spec(&dir, "bat", "    install: \"true\"\n");
    let service = create_service_test_service(&dir);

    let events = collect_events(service.audit("bat").await).await;

    assert!(waiting(&events).is_empty(), "{events:#?}");
}

// One waiting line per package, each naming its own, so N lines are not N copies
// of one.
#[tokio::test]
async fn an_audit_of_every_package_names_each_one() {
    let dir = TempDir::new().unwrap();
    write_spec(
        &dir,
        "bat",
        "    install: \"true\"\n    audit: \"echo brew\"\n",
    );
    write_spec(
        &dir,
        "fd",
        "    install: \"true\"\n    audit: \"echo brew\"\n",
    );
    let service = create_service_test_service(&dir);

    let events = collect_events(service.audit_all().await).await;

    let mut waits = waiting(&events);
    waits.sort();
    assert_eq!(waits.len(), 2, "{events:#?}");
    assert!(
        waits[0].contains("for bat") && waits[1].contains("for fd"),
        "{waits:?}"
    );
}

#[tokio::test]
async fn an_install_waits_on_its_install_command_and_names_the_package() {
    let dir = TempDir::new().unwrap();
    write_spec(&dir, "bat", "    install: \"true\"\n");
    let service = create_service_test_service(&dir);

    let events = collect_events(service.install("bat", InstallOptions::default()).await).await;

    assert!(
        waiting(&events)
            .iter()
            .any(|message| message.contains("install command for bat")),
        "{events:#?}"
    );
}

#[tokio::test]
async fn a_status_waits_on_the_check_commands_it_runs() {
    let dir = TempDir::new().unwrap();
    write_spec(&dir, "bat", "    install: \"true\"\n    check: \"true\"\n");
    let service = create_service_test_service(&dir);

    let events = collect_events(service.status("bat").await).await;

    let waits = waiting(&events);
    assert_eq!(waits.len(), 1, "{events:#?}");
    assert!(waits[0].contains("bat"), "{waits:?}");
}

// Control: no check command, and nothing it depends on, so nothing runs.
#[tokio::test]
async fn a_status_with_nothing_to_check_does_not_wait() {
    let dir = TempDir::new().unwrap();
    write_spec(&dir, "bat", "    install: \"true\"\n");
    let service = create_service_test_service(&dir);

    let events = collect_events(service.status("bat").await).await;

    assert!(waiting(&events).is_empty(), "{events:#?}");
}

#[tokio::test]
async fn a_listing_waits_on_its_check_commands_and_counts_them() {
    let dir = TempDir::new().unwrap();
    write_spec(&dir, "bat", "    install: \"true\"\n    check: \"true\"\n");
    write_spec(&dir, "fd", "    install: \"true\"\n");
    let service = create_service_test_service(&dir);

    let events = collect_events(service.list(false).await).await;

    assert_eq!(
        waiting(&events),
        vec!["Checking 1 package (2/2)".to_string()]
    );
}

// Control: no package has a check command, so the listing runs none.
#[tokio::test]
async fn a_listing_with_no_check_commands_does_not_wait() {
    let dir = TempDir::new().unwrap();
    write_spec(&dir, "fd", "    install: \"true\"\n");
    let service = create_service_test_service(&dir);

    let events = collect_events(service.list(false).await).await;

    assert!(waiting(&events).is_empty(), "{events:#?}");
}

// A check that reports "not installed" gave its answer: its step succeeded.
#[tokio::test]
async fn a_check_that_answers_no_still_succeeds() {
    let dir = TempDir::new().unwrap();
    write_spec(&dir, "bat", "    install: \"true\"\n    check: \"false\"\n");
    let service = create_service_test_service(&dir);

    let events = collect_events(service.check("bat").await).await;

    waiting(&events);
    assert_eq!(endings(&events), vec![StepEnding::Succeeded], "{events:#?}");
}

// An install command that exits non-zero ends its step as failed.
#[tokio::test]
async fn a_failing_install_ends_its_step_as_failed() {
    let dir = TempDir::new().unwrap();
    write_spec(&dir, "bat", "    install: \"false\"\n");
    let service = create_service_test_service(&dir);

    let events = collect_events(service.install("bat", InstallOptions::default()).await).await;

    waiting(&events);
    assert_eq!(endings(&events), vec![StepEnding::Failed], "{events:#?}");
}

// A step ends before the answer it produced is sent, so a consumer closes the
// step's line before it shows the answer.
#[tokio::test]
async fn a_step_ends_before_its_answer_is_sent() {
    let dir = TempDir::new().unwrap();
    write_spec(&dir, "bat", "    install: \"true\"\n    check: \"true\"\n");
    let service = create_service_test_service(&dir);

    let position = |events: &[PackageEvent], answer: fn(&PackageEvent) -> bool| {
        let ended = events
            .iter()
            .position(|e| matches!(e, PackageEvent::StepEnded { .. }))
            .expect("a step ended");
        let answered = events.iter().position(answer).expect("an answer was sent");
        (ended, answered)
    };

    let events = collect_events(service.check("bat").await).await;
    let (ended, answered) = position(&events, |e| {
        matches!(e, PackageEvent::CheckResultCompleted { .. })
    });
    assert!(ended < answered, "{events:#?}");

    let events = collect_events(service.status("bat").await).await;
    let (ended, answered) = position(&events, |e| {
        matches!(e, PackageEvent::EnvironmentStatusChecked { .. })
    });
    assert!(ended < answered, "{events:#?}");
}
