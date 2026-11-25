// Drives the built selfie-mcp binary over stdio, the way an MCP client does:
// one JSON-RPC value per line, in a sandbox HOME with a cleared environment.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

// The protocol version the test asks for. rmcp shapes `tools/list` by the
// version the client asks for, so the test pins one.
const PROTOCOL_VERSION: &str = "2025-06-18";

const RESPONSE_DEADLINE: Duration = Duration::from_secs(30);
const EXIT_DEADLINE: Duration = Duration::from_secs(10);

// Every tool the server registers, sorted.
const TOOLS: [&str; 23] = [
    "selfie_apply_dotfiles",
    "selfie_config_get",
    "selfie_dotfiles_drift",
    "selfie_dotfiles_list",
    "selfie_dotfiles_track",
    "selfie_package_audit",
    "selfie_package_audit_all",
    "selfie_package_check",
    "selfie_package_install",
    "selfie_package_list",
    "selfie_package_status",
    "selfie_package_track_dotfile",
    "selfie_spec_create",
    "selfie_spec_info",
    "selfie_spec_list",
    "selfie_spec_remove",
    "selfie_spec_update",
    "selfie_spec_update_batch",
    "selfie_spec_validate",
    "selfie_spec_validate_all",
    "selfie_sync_pull",
    "selfie_sync_push",
    "selfie_sync_status",
];

// A running selfie-mcp. Dropping it kills the process group and reaps the
// child, so a failing or hung test leaves nothing running.
struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    stderr: Option<JoinHandle<String>>,
    next_id: u64,
}

impl Server {
    fn spawn(home: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_selfie-mcp"))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("SELFIE_CONFIG_DIR", home.join(".config/selfie"))
            .env("SHELL", "/bin/sh")
            .env("TERM", "dumb")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .expect("selfie-mcp starts");
        let stdout = child.stdout.take().unwrap();
        let (sender, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        // Drained on its own thread, so a full pipe can never stall the server.
        let mut stderr = child.stderr.take().unwrap();
        let stderr = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            text
        });
        Self {
            stdin: child.stdin.take(),
            child,
            lines,
            stderr: Some(stderr),
            next_id: 1,
        }
    }

    fn send(&mut self, message: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin is open");
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    }

    fn notify(&mut self, method: &str) {
        self.send(&json!({ "jsonrpc": "2.0", "method": method }));
    }

    // Sends a request and returns its `result`, failing on an error response
    // or on no response within the deadline.
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        let deadline = Instant::now() + RESPONSE_DEADLINE;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self.lines.recv_timeout(left).unwrap_or_else(|e| {
                panic!("no response to {method} ({e}); stderr:\n{}", self.stop())
            });
            let response: Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("{method}: stdout line is not JSON ({e}): {line}"));
            // A notification from the server carries no id, and a request from
            // it carries a `method` and an id of its own numbering; skip both.
            if response["id"] != id || response.get("method").is_some() {
                continue;
            }
            assert!(
                response.get("error").is_none(),
                "{method} failed: {response}"
            );
            return response["result"].clone();
        }
    }

    // Calls a tool and returns the JSON its one text block holds, with its
    // `isError` flag.
    fn call(&mut self, tool: &str) -> (Value, bool) {
        let result = self.request("tools/call", json!({ "name": tool, "arguments": {} }));
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{tool}: no text block in {result}"));
        let body = serde_json::from_str(text).unwrap_or_else(|e| panic!("{tool}: {e}: {text}"));
        (body, result["isError"] == true)
    }

    // Kills the server if it is still running and returns what it wrote to
    // stderr.
    fn stop(&mut self) -> String {
        let group = nix::unistd::Pid::from_raw(self.child.id().try_into().unwrap());
        let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL);
        let _ = self.child.wait();
        self.stderr
            .take()
            .map(|handle| handle.join().unwrap_or_default())
            .unwrap_or_default()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

// A HOME holding a config and a package directory with one spec.
fn sandbox() -> (tempfile::TempDir, std::path::PathBuf) {
    let temp = tempfile::TempDir::new().unwrap();
    // macOS gives a temp dir under `/var`, a symlink to `/private/var`.
    let home = temp.path().canonicalize().unwrap();
    let packages = home.join("packages");
    let config = home.join(".config/selfie");
    std::fs::create_dir_all(&packages).unwrap();
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("config.yaml"),
        format!(
            "environment: sandbox\npackage_directory: {}\n",
            packages.display()
        ),
    )
    .unwrap();
    std::fs::write(
        packages.join("sandbox-sentinel.yaml"),
        "name: sandbox-sentinel\nenvironments:\n  sandbox:\n    install: \"true\"\n",
    )
    .unwrap();
    (temp, home)
}

#[test]
fn a_client_lists_the_tools_and_calls_them_over_stdio() {
    let (_temp, home) = sandbox();
    let mut server = Server::spawn(&home);

    let init = server.request(
        "initialize",
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "stdio-test", "version": "0.1" },
        }),
    );
    assert_eq!(
        init["serverInfo"]["name"], "selfie-mcp",
        "initialize: {init}"
    );
    assert!(
        init["capabilities"]["tools"].is_object(),
        "initialize: {init}"
    );
    server.notify("notifications/initialized");

    let listed = server.request("tools/list", json!({}));
    let mut names: Vec<&str> = listed["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list: {listed}"))
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(names, TOOLS, "tools/list");

    let (config, failed) = server.call("selfie_config_get");
    assert!(!failed, "selfie_config_get: {config}");
    let mut keys: Vec<&str> = config
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "command_timeout_secs",
            "dotfiles_directory",
            "environment",
            "ignored_config_keys",
            "package_directory",
            "shell_fallback",
            "state_directory",
        ],
        "selfie_config_get: {config}"
    );
    assert_eq!(config["environment"], "sandbox", "{config}");
    assert_eq!(
        config["package_directory"],
        home.join("packages").display().to_string(),
        "{config}"
    );
    assert_eq!(config["ignored_config_keys"], json!([]), "{config}");
    assert_eq!(config["shell_fallback"], Value::Null, "{config}");
    assert!(config["command_timeout_secs"].is_u64(), "{config}");

    let (listing, failed) = server.call("selfie_spec_list");
    assert!(!failed, "selfie_spec_list: {listing}");
    let specs: Vec<&Value> = listing["data"]
        .as_array()
        .unwrap_or_else(|| panic!("selfie_spec_list: {listing}"))
        .iter()
        .filter(|row| row["type"] == "spec_list_item")
        .map(|row| &row["name"])
        .collect();
    assert_eq!(
        specs,
        [&json!("sandbox-sentinel")],
        "selfie_spec_list: {listing}"
    );

    // Closing stdin ends the session, and the server exits cleanly.
    drop(server.stdin.take());
    let deadline = Instant::now() + EXIT_DEADLINE;
    let status = loop {
        if let Some(status) = server.child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "selfie-mcp did not exit after stdin closed; stderr:\n{}",
            server.stop()
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "{status}; stderr:\n{}", server.stop());
}
