//! A `tracing` subscriber that records every event into a buffer.

use std::{
    io,
    sync::{Arc, Mutex},
};

use tracing_subscriber::fmt::MakeWriter;

/// The records a [`capture_tracing`] subscriber has written, while it is the
/// thread's default.
///
/// The subscriber covers only the thread that called [`capture_tracing`], so
/// use it on a current-thread runtime and keep the code under test off spawned
/// tasks on other threads.
///
/// Reach each logging call under test once before capturing. A call site first
/// reached on another thread while the capture starts can cache the answer
/// that nothing listens to it, and its records are then missed.
pub struct CapturedTracing {
    buffer: Arc<Mutex<Vec<u8>>>,
    _guard: tracing::subscriber::DefaultGuard,
}

impl CapturedTracing {
    /// Everything recorded so far, decoded lossily.
    ///
    /// # Panics
    ///
    /// If a thread panicked while writing a record.
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.buffer.lock().unwrap()).into_owned()
    }
}

/// Record every event at `level` and above on this thread until the returned
/// value is dropped.
#[must_use]
// The miss the caller is warned about: tracing-core computes a call site's
// interest from a snapshot of the dispatchers when it is first reached, and a
// snapshot taken just before this dispatcher registered can overwrite the
// rebuild that included it.
pub fn capture_tracing(level: tracing::Level) -> CapturedTracing {
    let buffer: Arc<Mutex<Vec<u8>>> = Arc::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(Writer(Arc::clone(&buffer)))
        .with_max_level(level)
        .with_ansi(false)
        .finish();
    CapturedTracing {
        buffer,
        _guard: tracing::subscriber::set_default(subscriber),
    }
}

#[derive(Clone)]
struct Writer(Arc<Mutex<Vec<u8>>>);

impl io::Write for Writer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("capture buffer poisoned"))?
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl MakeWriter<'_> for Writer {
    type Writer = Self;

    fn make_writer(&self) -> Self::Writer {
        self.clone()
    }
}
