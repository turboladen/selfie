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
        test_common::relays_output_containing(&events, SECRET),
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

// A server whose install commands are stopped after 2 seconds of silence. The
// secret is in a file, so no install command names it.
fn quiet_server(packages: &std::path::Path, name: &str, install: &str) -> super::tests::LiveServer {
    std::fs::write(packages.join("secret.txt"), format!("TOKEN={SECRET}\n")).unwrap();
    write_spec(packages, name, install);
    let config = selfie::config::SelfieConfigBuilder::default()
        .environment("test")
        .package_directory(packages)
        .command_timeout_unchecked(2)
        .build();
    super::tests::server_configured(&config, super::cancellation_tests::sh_runner, |live| live)
}

#[tokio::test]
async fn a_timed_out_install_sends_its_bounded_tail() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = quiet_server(temp.path(), "stuck", test_common::LONG_THEN_QUIET);

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        server
            .for_request(CancellationToken::new())
            .package_install(install_params("stuck")),
    )
    .await
    .unwrap()
    .unwrap();

    let json = tool_json(&result);
    test_common::assert_secret_free(&json.to_string(), SECRET, "the tool result");
    // Positive control: this run's command printed the secret, from the file
    // `quiet_server` wrote.
    let file = std::fs::read_to_string(temp.path().join("secret.txt")).unwrap();
    assert!(file.contains(SECRET), "{file}");
    let failure = &json["result"];
    assert_eq!(failure["status"], "failure", "{json}");
    assert_eq!(failure["kind"], "timed_out", "{json}");
    assert_eq!(failure["limit"], "silence", "{json}");
    assert_eq!(failure["timeout_seconds"], 2, "{json}");
    for pipe in ["stdout", "stderr"] {
        let tail = failure[pipe].as_str().unwrap();
        assert!(tail.contains("bytes elided"), "{pipe}: {tail}");
        assert!(tail.starts_with("1\n2\n"), "{pipe}: {tail}");
        assert!(tail.ends_with("1300\n"), "{pipe}: {tail}");
    }
    assert!(
        !json["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["type"] == "output"),
        "{json}"
    );

    // Positive control on this same run: what the command printed is rebuilt
    // here, and each pipe names exactly its middle as elided, which is where the
    // secret was printed.
    let seq = |range: std::ops::RangeInclusive<u32>| -> String {
        range.map(|n| format!("{n}\n")).collect()
    };
    let printed = format!("{}TOKEN={SECRET}\n{}", seq(1..=300), seq(301..=1300));
    let secret_at = printed.find(SECRET).unwrap();
    assert!(1000 < secret_at && secret_at + SECRET.len() < printed.len() - 1000);
    let elided = format!("({} bytes elided)", printed.len() - 2000);
    for pipe in ["stdout", "stderr"] {
        assert!(
            failure[pipe].as_str().unwrap().contains(&elided),
            "{pipe} does not name {elided}: {json}"
        );
    }
}

#[tokio::test]
async fn a_timed_out_install_forwards_a_short_tail_whole() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = quiet_server(
        temp.path(),
        "stuck",
        "echo out-line; echo err-line >&2; sleep 4",
    );

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        server
            .for_request(CancellationToken::new())
            .package_install(install_params("stuck")),
    )
    .await
    .unwrap()
    .unwrap();

    let json = tool_json(&result);
    let failure = &json["result"];
    assert_eq!(failure["kind"], "timed_out", "{json}");
    assert_eq!(failure["stdout"], "out-line\n", "{json}");
    assert_eq!(failure["stderr"], "err-line\n", "{json}");
}
