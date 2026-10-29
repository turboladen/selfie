//! Running work under a deadline, for a test whose failure mode is a hang.

use std::os::unix::fs::OpenOptionsExt as _;
use std::path::PathBuf;

/// A fifo a test made, released and removed when this is dropped.
///
/// Dropping it opens the fifo for writing without blocking, which releases a
/// read blocked on it, then removes it, so a test that fails does not leave a
/// thread stuck.
pub struct ReleaseFifo(pub PathBuf);

impl Drop for ReleaseFifo {
    fn drop(&mut self) {
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(nix::fcntl::OFlag::O_NONBLOCK.bits())
            .open(&self.0);
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Run `work` on a thread and runtime of its own, and give up after `deadline`:
/// `None` for a timeout, and a panic in `work` raised again here.
///
/// For a test whose failure mode is a read blocked on a fifo. On a timeout the
/// blocked thread is abandoned, and ends with the test binary.
///
/// # Panics
///
/// If the runtime cannot be built, or with `work`'s own panic.
// That read cannot be canceled, and a runtime whose worker is stuck in it never
// finishes dropping, so a `tokio::time::timeout` inside the test's own runtime
// fires and then hangs the test at teardown: the deadline turns a hang into a
// later hang, not a failure.
pub fn within_deadline<T, Fut, F>(deadline: std::time::Duration, work: F) -> Option<T>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T>,
    T: Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let _ = tx.send(runtime.block_on(work()));
    });
    match rx.recv_timeout(deadline) {
        Ok(value) => Some(value),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
        // The work panicked, which dropped the sender: raise that panic here, so a
        // failing fixture reads as itself rather than as a hang.
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => match worker.join() {
            Err(panic) => std::panic::resume_unwind(panic),
            Ok(()) => unreachable!("the worker returned without sending"),
        },
    }
}
