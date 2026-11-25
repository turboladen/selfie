// What an install command prints reaches an MCP client only through a failed
// install's bounded pipes. These run the real tool over a real shell.

use futures::StreamExt as _;
use selfie::package::{
    event::PackageEvent,
    service::{InstallOptions, PackageService as _},
};
use tokio_util::sync::CancellationToken;

use super::{
    cancellation_tests::{install_params, server_over, write_spec},
    tests::tool_json,
};

const SECRET: &str = "s3cr3t-v4lue-DO-NOT-LEAK";

#[tokio::test]
async fn a_successful_install_sends_the_client_no_output() {
    let temp = tempfile::TempDir::new().unwrap();
    write_spec(
        temp.path(),
        "chatty",
        &format!("echo TOKEN={SECRET}; echo TOKEN={SECRET} >&2"),
    );
    let server = server_over(temp.path(), None);

    let result = server
        .for_request(CancellationToken::new())
        .package_install(install_params("chatty"))
        .await
        .unwrap();

    let json = tool_json(&result);
    test_common::assert_secret_free(&json.to_string(), SECRET, "the tool result");
    assert!(
        !json["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["type"] == "output"),
        "{json}"
    );
    assert_eq!(json["result"]["status"], "success", "{json}");

    // Positive control: the same install's event stream does relay the secret,
    // so the tool result above is clean because the MCP server dropped it.
    let events: Vec<PackageEvent> = server
        .service
        .install("chatty", InstallOptions::default())
        .await
        .collect()
        .await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            PackageEvent::Info { output, .. } if output.text().contains(SECRET)
        )),
        "the install relayed no output, so the test observed nothing"
    );
}

#[tokio::test]
async fn a_failed_install_sends_the_bounded_tail() {
    let temp = tempfile::TempDir::new().unwrap();
    write_spec(
        temp.path(),
        "broken",
        "echo tail-line; echo 'error: boom' >&2; exit 3",
    );
    let server = server_over(temp.path(), None);

    let result = server
        .for_request(CancellationToken::new())
        .package_install(install_params("broken"))
        .await
        .unwrap();

    let json = tool_json(&result);
    let failure = &json["result"];
    assert_eq!(failure["status"], "failure", "{json}");
    assert_eq!(
        failure["stdout"].as_str().unwrap().trim(),
        "tail-line",
        "{json}"
    );
    assert_eq!(
        failure["stderr"].as_str().unwrap().trim(),
        "error: boom",
        "{json}"
    );
}
