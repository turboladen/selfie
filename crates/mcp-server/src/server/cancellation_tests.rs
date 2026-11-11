// A client that cancels a tool call, or disconnects during one, stops the
// commands that call started.

use std::{path::Path, time::Duration};

use rmcp::{
    ServiceExt as _,
    model::{CallToolRequest, CallToolRequestParams, ClientRequest},
    service::PeerRequestOptions,
};
use selfie::commands::ShellCommandRunner;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use super::{
    InstallParam, SelfieServer,
    tests::{server_running, tool_json},
    wait_for_calls,
};

// A command that keeps printing never trips the silence limit, so only a
// cancellation can end it. The marker is how a test finds the processes: it is
// on the shell and on a long-lived child, so a cancellation that stopped only the
// shell would leave the child for the test to find. The child ignores SIGHUP,
// which is what closing the shell's terminal sends; without that it would end
// on the hangup alone and the kill would go untested.
fn chatty_command(marker: &str) -> String {
    format!(
        "sh -c 'trap \"\" HUP; sleep 600 # {marker}' & while :; do echo tick; sleep 0.1; done # {marker}"
    )
}

fn marker() -> String {
    format!("cancel-marker-{}", uuid::Uuid::new_v4().simple())
}

fn write_spec(packages: &Path, name: &str, install: &str) {
    std::fs::write(
        packages.join(format!("{name}.yml")),
        format!("name: {name}\nenvironments:\n  test:\n    install: {install:?}\n"),
    )
    .unwrap();
}

// A server whose package service runs commands with `/bin/sh`, so no test
// depends on the login profile of whoever runs it.
fn server_over(packages: &Path, state: Option<&Path>) -> SelfieServer {
    server_running(packages, None, state, |_| {
        ShellCommandRunner::new(ShellCommandRunner::default_shell(), Duration::from_secs(60))
    })
}

// How many processes have `marker` in their command line.
fn processes_with(marker: &str) -> usize {
    std::process::Command::new("pgrep")
        .args(["-f", marker])
        .output()
        .map_or(0, |output| {
            output
                .stdout
                .split(|b| *b == b'\n')
                .filter(|l| !l.is_empty())
                .count()
        })
}

fn is_running(marker: &str) -> bool {
    processes_with(marker) > 0
}

// The shell and the long-lived child `chatty_command` starts are both there.
fn chatty_is_running(marker: &str) -> bool {
    processes_with(marker) >= 2
}

// Ends whatever still carries the marker when a test finishes or fails, so a
// regression leaves nothing running on the machine.
struct Cleanup(String);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::process::Command::new("pkill")
            .args(["-9", "-f", &self.0])
            .output();
    }
}

// Poll until `condition` holds or `limit` passes, and say which.
async fn within(limit: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + limit;
    while tokio::time::Instant::now() < deadline {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    condition()
}

fn install_params(package: &str) -> rmcp::handler::server::wrapper::Parameters<InstallParam> {
    rmcp::handler::server::wrapper::Parameters(InstallParam {
        package: package.to_string(),
        skip_recommends: false,
    })
}

// Cancel `ct` once `started` says the command carrying `marker` is running, and
// say whether it ever was. The answer is the control: a cancellation that came
// before the command started would pass for the wrong reason.
fn cancel_once_running(
    ct: &CancellationToken,
    marker: &str,
    started: fn(&str) -> bool,
) -> tokio::task::JoinHandle<bool> {
    let (ct, marker) = (ct.clone(), marker.to_string());
    tokio::spawn(async move {
        let running = within(Duration::from_secs(5), || started(&marker)).await;
        ct.cancel();
        running
    })
}

#[tokio::test]
async fn canceling_a_request_stops_an_install_that_keeps_printing() {
    let temp = tempfile::TempDir::new().unwrap();
    let marker = marker();
    let _cleanup = Cleanup(marker.clone());
    write_spec(temp.path(), "chatty", &chatty_command(&marker));
    let server = server_over(temp.path(), None);
    let ct = CancellationToken::new();

    let canceler = cancel_once_running(&ct, &marker, chatty_is_running);
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        server
            .for_request(ct)
            .package_install(install_params("chatty")),
    )
    .await
    .expect("the call returns once it is canceled")
    .unwrap();

    assert!(canceler.await.unwrap(), "the install never started");
    assert_eq!(tool_json(&result)["result"]["outcome"], "canceled");
    assert!(
        within(Duration::from_secs(5), || !is_running(&marker)).await,
        "the install outlived its canceled call"
    );
}

#[tokio::test]
async fn a_request_canceled_before_it_runs_starts_no_command() {
    let temp = tempfile::TempDir::new().unwrap();
    let made = temp.path().join("made");
    write_spec(
        temp.path(),
        "touches",
        &format!("touch '{}'", made.display()),
    );
    let server = server_over(temp.path(), None);
    let ct = CancellationToken::new();
    ct.cancel();

    let result = server
        .for_request(ct)
        .package_install(install_params("touches"))
        .await
        .unwrap();

    assert_eq!(tool_json(&result)["result"]["outcome"], "canceled");
    assert!(!made.exists(), "a canceled request still ran its command");

    // The control: the same install under a live token does create the file.
    server
        .for_request(CancellationToken::new())
        .package_install(install_params("touches"))
        .await
        .unwrap();
    assert!(made.exists());
}

#[tokio::test]
async fn canceling_a_request_stops_a_dotfile_command() {
    let temp = tempfile::TempDir::new().unwrap();
    let packages = temp.path().join("packages");
    let state = temp.path().join("state");
    std::fs::create_dir_all(&packages).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    let marker = marker();
    let _cleanup = Cleanup(marker.clone());
    let target = temp.path().join("out");
    std::fs::write(
        packages.join("dots.yml"),
        format!(
            "name: dots\nenvironments:\n  test:\n    install: \"true\"\n    dotfiles:\n      \
             - command: \"sleep 30 # {marker}\"\n        target: {}\n",
            target.display()
        ),
    )
    .unwrap();
    let server = server_over(&packages, Some(&state));
    let ct = CancellationToken::new();

    let canceler = cancel_once_running(&ct, &marker, is_running);
    let result =
        tokio::time::timeout(
            Duration::from_secs(10),
            server.for_request(ct).selfie_apply_dotfiles(
                rmcp::handler::server::wrapper::Parameters(super::ApplyParam {
                    name: None,
                    dry_run: false,
                    auto_accept: false,
                }),
            ),
        )
        .await
        .expect("the call returns once it is canceled")
        .unwrap();

    assert!(canceler.await.unwrap(), "the command never started");
    assert_eq!(tool_json(&result)["result"]["outcome"], "canceled");
    assert!(
        within(Duration::from_secs(5), || !is_running(&marker)).await,
        "the command outlived its canceled call"
    );
    assert!(!target.exists());
}

// A client and a server joined by an in-memory pipe, speaking the real protocol.
async fn connected(
    server: &SelfieServer,
) -> (
    rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tokio::task::JoinHandle<()>,
) {
    let (server_io, client_io) = tokio::io::duplex(1 << 16);
    let running = server.clone().serve(server_io);
    let serving = tokio::spawn(async move {
        let _ = running.await.unwrap().waiting().await;
    });
    let client = ().serve(client_io).await.unwrap();
    (client, serving)
}

// Ask the server, over the protocol, to install the `chatty` package, and return
// once the install is running.
async fn start_chatty_install(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    marker: &str,
) -> rmcp::service::RequestHandle<rmcp::RoleClient> {
    let arguments = serde_json::json!({ "package": "chatty" });
    let handle = client
        .peer()
        .send_request_with_option(
            ClientRequest::CallToolRequest(CallToolRequest::new(
                CallToolRequestParams::new("selfie_package_install")
                    .with_arguments(arguments.as_object().cloned().unwrap()),
            )),
            PeerRequestOptions::no_options(),
        )
        .await
        .unwrap();
    assert!(
        within(Duration::from_secs(10), || chatty_is_running(marker)).await,
        "the install never started"
    );
    handle
}

#[tokio::test]
async fn a_cancel_notification_from_the_client_stops_the_install() {
    let temp = tempfile::TempDir::new().unwrap();
    let marker = marker();
    let _cleanup = Cleanup(marker.clone());
    write_spec(temp.path(), "chatty", &chatty_command(&marker));
    let server = server_over(temp.path(), None);
    let in_flight = server.in_flight();
    let (client, _serving) = connected(&server).await;

    let handle = start_chatty_install(&client, &marker).await;
    // A running call holds a token, which is what shutdown waits on.
    assert_eq!(in_flight.len(), 1);
    handle.cancel(None).await.unwrap();

    assert!(
        within(Duration::from_secs(10), || !is_running(&marker)).await,
        "the install outlived the client's cancellation"
    );
    assert!(
        within(Duration::from_secs(10), || in_flight.is_empty()).await,
        "the call kept its token after it was canceled"
    );
}

#[tokio::test]
async fn shutdown_waits_for_a_call_that_is_still_stopping_its_command() {
    let in_flight = TaskTracker::new();
    let token = in_flight.token();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(token);
    });
    let started = tokio::time::Instant::now();

    wait_for_calls(&in_flight, Duration::from_secs(5)).await;

    // It returned because the call finished, not because the limit ran out.
    assert!(started.elapsed() >= Duration::from_millis(100));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(in_flight.is_empty());
}

#[tokio::test]
async fn shutdown_gives_up_on_a_call_that_never_finishes() {
    let in_flight = TaskTracker::new();
    let _stuck = in_flight.token();

    tokio::time::timeout(
        Duration::from_secs(5),
        wait_for_calls(&in_flight, Duration::from_millis(100)),
    )
    .await
    .expect("the wait ends at its limit");

    assert_eq!(in_flight.len(), 1);
}

#[tokio::test]
async fn a_client_that_disconnects_mid_install_leaves_nothing_running() {
    let temp = tempfile::TempDir::new().unwrap();
    let marker = marker();
    let _cleanup = Cleanup(marker.clone());
    write_spec(temp.path(), "chatty", &chatty_command(&marker));
    let server = server_over(temp.path(), None);
    let in_flight = server.in_flight();
    let (client, serving) = connected(&server).await;

    let _handle = start_chatty_install(&client, &marker).await;
    let _ = client.cancel().await;

    // The server gives a call that is still running a few seconds to answer
    // before it cancels the call, and `main` waits for it afterwards.
    tokio::time::timeout(Duration::from_secs(15), serving)
        .await
        .expect("the server stopped after the client left")
        .unwrap();
    wait_for_calls(&in_flight, super::SHUTDOWN_WAIT).await;

    assert!(
        !is_running(&marker),
        "the install outlived the client that started it"
    );
}
