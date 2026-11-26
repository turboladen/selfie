//! A `tracing` subscriber that records each thread's events into its own
//! buffer.

use std::{cell::RefCell, io, rc::Rc, sync::OnceLock};

use tracing_subscriber::{Layer, filter::dynamic_filter_fn, layer::SubscriberExt};

type Buffer = Rc<RefCell<Vec<u8>>>;

thread_local! {
    // The capture this thread is running, if any: its level and its buffer.
    static CAPTURE: RefCell<Option<(tracing::Level, Buffer)>> = const { RefCell::new(None) };
}

/// The records a [`capture_tracing`] call has collected, until it is dropped.
///
/// A capture covers only the thread that started it, so use it on a
/// current-thread runtime and keep the code under test off spawned tasks on
/// other threads. It cannot be sent to another thread.
pub struct CapturedTracing {
    buffer: Buffer,
}

impl CapturedTracing {
    /// Everything recorded so far, decoded lossily.
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.buffer.borrow()).into_owned()
    }
}

impl Drop for CapturedTracing {
    fn drop(&mut self) {
        // The slot is gone once the thread's locals are destroyed, and so is
        // anything left to end.
        let _ = CAPTURE.try_with(|capture| capture.borrow_mut().take());
    }
}

/// Record every event at `level` and above on this thread until the returned
/// value is dropped.
///
/// The first call installs a global subscriber for the rest of the test
/// binary, so no other global subscriber can be set after it.
///
/// A call site first reached on another thread while the binary's first
/// capture is being installed can still be missed by that capture. Every
/// later capture repairs it before it starts.
///
/// # Panics
///
/// If another global subscriber is already installed in this test binary, if
/// this thread has a scoped default subscriber, which would take the records,
/// or if this thread is already capturing.
#[must_use]
pub fn capture_tracing(level: tracing::Level) -> CapturedTracing {
    install();
    assert!(
        tracing::dispatcher::get_default(tracing::Dispatch::is::<Capturing>),
        "this thread has a scoped default tracing subscriber, so a capture would record nothing"
    );
    // Registering the global rebuilds every known call site's interest before
    // `set_global_default` makes it readable. A call site first reached on
    // another thread in between caches "never": tracing-core's
    // `Rebuilder::JustOne` asks `get_default`, which finds no global yet.
    // Rebuilding here repairs it before each capture starts.
    tracing::callsite::rebuild_interest_cache();
    let buffer = Buffer::default();
    CAPTURE.with_borrow_mut(|capture| {
        assert!(
            capture.is_none(),
            "this thread is already capturing tracing"
        );
        *capture = Some((level, Rc::clone(&buffer)));
    });
    CapturedTracing { buffer }
}

// One global subscriber serves every capture. A thread-local default would let
// a call site first reached on a thread that is not capturing cache the answer
// that nobody listens, and the capturing thread would then miss its records.
// So the subscriber must answer `Interest::sometimes` for every call site.
// `dynamic_filter_fn` does. `filter_fn` would cache `enabled`'s answer for the
// thread and level that reached the call site first. The unfiltered
// `Capturing` layer turns a cached "never" into "sometimes", but a cached
// "always" stands, so one thread's verbose capture would widen another's level.
fn install() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let on_a_capturing_thread = dynamic_filter_fn(|metadata, _| {
            CAPTURE
                .try_with(|capture| {
                    capture
                        .borrow()
                        .as_ref()
                        .is_some_and(|(level, _)| metadata.level() <= level)
                })
                .unwrap_or(false)
        });
        let layer = tracing_subscriber::fmt::layer()
            .with_writer(|| ThisThread)
            .with_ansi(false)
            .with_filter(on_a_capturing_thread);
        let subscriber = tracing_subscriber::registry().with(layer).with(Capturing);
        tracing::subscriber::set_global_default(subscriber)
            .expect("another global tracing subscriber is installed in this test binary");
    });
}

// Marks the global subscriber, so a capture can tell it from a scoped default.
struct Capturing;

impl<S: tracing::Subscriber> Layer<S> for Capturing {}

// Writes to the calling thread's capture, and nowhere when it has none.
struct ThisThread;

impl io::Write for ThisThread {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = CAPTURE.try_with(|capture| {
            if let Some((_, buffer)) = &*capture.borrow() {
                buffer.borrow_mut().extend_from_slice(buf);
            }
        });
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::capture_tracing;
    use tracing::Level;

    // Each test logs from call sites of its own, so no other test reaches them.

    // A call site another thread reaches first, while this one is capturing,
    // still delivers this thread's record.
    #[test]
    fn a_call_site_first_reached_on_another_thread_is_still_captured() {
        fn emit() {
            tracing::info!("reached-elsewhere-first");
        }
        let log = capture_tracing(Level::INFO);
        std::thread::spawn(emit).join().unwrap();

        emit();

        let text = log.text();
        assert_eq!(text.matches("reached-elsewhere-first").count(), 1, "{text}");
    }

    #[test]
    fn another_threads_records_are_not_captured() {
        let log = capture_tracing(Level::INFO);
        std::thread::spawn(|| tracing::info!("from-another-thread"))
            .join()
            .unwrap();

        tracing::info!("from-this-thread");

        let text = log.text();
        assert!(!text.contains("from-another-thread"), "{text}");
        // Positive control: the capture is recording at all.
        assert!(text.contains("from-this-thread"), "{text}");
    }

    #[test]
    fn records_below_the_level_are_dropped() {
        let log = capture_tracing(Level::INFO);

        tracing::debug!("below-the-level");
        tracing::info!("at-the-level");

        let text = log.text();
        assert!(!text.contains("below-the-level"), "{text}");
        assert!(text.contains("at-the-level"), "{text}");
    }

    #[test]
    fn a_thread_can_capture_again_after_a_capture_ends() {
        drop(capture_tracing(Level::INFO));
        let log = capture_tracing(Level::INFO);

        tracing::info!("second-capture");

        let text = log.text();
        assert!(text.contains("second-capture"), "{text}");
    }

    // A call site a more verbose capture on another thread reaches first
    // still honors this capture's level.
    #[test]
    fn another_threads_verbose_capture_does_not_widen_this_ones_level() {
        fn emit() {
            tracing::debug!("debug-from-shared-site");
        }
        let log = capture_tracing(Level::INFO);
        std::thread::spawn(|| {
            let _verbose = capture_tracing(Level::TRACE);
            emit();
        })
        .join()
        .unwrap();

        emit();
        tracing::info!("info-control");

        let text = log.text();
        assert!(!text.contains("debug-from-shared-site"), "{text}");
        // Positive control: the capture is recording at all.
        assert!(text.contains("info-control"), "{text}");
    }

    #[test]
    #[should_panic(expected = "scoped default tracing subscriber")]
    fn a_capture_under_a_scoped_default_refuses_to_start() {
        let _scoped = tracing::subscriber::set_default(tracing_subscriber::registry());

        let _log = capture_tracing(Level::INFO);
    }
}
