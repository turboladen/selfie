//! Writing executable test fixtures.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Write `body` to `path` and make it executable.
///
/// # Panics
///
/// If the script cannot be written or made executable.
// The write happens in a subprocess, and it has to. A test binary runs tests on
// several threads, several of which spawn processes. `File::create` here would
// leave this process holding a write descriptor across the write, and a
// concurrent `spawn` would fork a child that inherits it. The descriptor is
// `O_CLOEXEC`, so the child holds it only until its own `exec`, but Linux refuses
// to `execve` a file any process has open for writing, with `ETXTBSY`.
//
// macOS does not enforce that rule, so getting it wrong fails only on CI, and
// only sometimes.
pub fn write_executable(path: &Path, body: &str) {
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(r#"cat > "$1" && chmod 755 "$1""#)
        .arg("sh")
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawning /bin/sh to write a fixture");

    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(body.as_bytes())
        .expect("writing the fixture body");

    let status = child.wait().expect("waiting for the fixture writer");
    assert!(status.success(), "could not write {}", path.display());
}

/// A two-line shell command whose lines each create a marker file in
/// `markers`, returned with the paths of the two markers.
///
/// `markers` must exist, so either line succeeds wherever it runs. A marker that
/// exists afterwards is proof that its line ran.
///
/// # Panics
///
/// If `markers` is not valid UTF-8 or cannot be quoted for a shell.
#[must_use]
pub fn two_marking_lines(markers: &Path) -> (String, PathBuf, PathBuf) {
    let first = markers.join("first");
    let second = markers.join("second");
    let quote = |path: &Path| {
        shlex::try_quote(path.to_str().expect("a UTF-8 marker path"))
            .expect("a quotable marker path")
            .into_owned()
    };
    let command = format!("touch {}\ntouch {}", quote(&first), quote(&second));
    (command, first, second)
}
