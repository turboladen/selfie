//! Writing executable test fixtures.

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

/// Write `body` to `path` and make it executable.
///
/// **The write happens in a subprocess, and it has to.** A test binary runs its
/// tests on several threads, and several of them spawn processes. `File::create`
/// here would leave this process holding a write descriptor to the file across
/// the write, any concurrent `spawn` would fork a child that inherits it — the
/// descriptor is `O_CLOEXEC`, so the child holds it only until its own `exec`,
/// but that window is enough — and Linux refuses to `execve` a file that any
/// process has open for writing, with `ETXTBSY`. Doing the write in a child that
/// has exited by the time this returns means no descriptor exists to inherit.
///
/// macOS does not enforce that rule, so getting this wrong fails only on CI, and
/// only sometimes.
///
/// # Panics
///
/// If the script cannot be written or made executable.
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
