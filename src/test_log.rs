//! Log capture for tests, shared by every module's tests (as `store::fail_writes` is): run code
//! under a `tracing` subscriber that writes plain text (no ANSI colour) into a buffer, then read
//! what it logged. [`capture`] runs a closure, [`capture_async`] a future.

use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};
use tracing::instrument::WithSubscriber;

/// A dispatcher with no subscriber, registered on the first capture and kept for the life of the
/// test process. tracing-core caches whether each log line is enabled when the line is first hit.
/// While only one dispatcher is registered it asks just the hitting thread's, so a line another
/// test's thread (one with no subscriber) hits first while a capture runs is cached as disabled,
/// and the capture misses it. With this one registered too there are always at least two, and
/// tracing-core asks every live dispatcher: the line is then checked on each hit.
static ANCHOR: OnceLock<tracing::Dispatch> = OnceLock::new();

/// The buffer the subscriber writes into. Cloned into the subscriber's writer, so the test keeps a
/// handle to read it back.
#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Buf {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

/// A subscriber that writes every event at `level` or above into `buf`.
fn subscriber(
    level: tracing::Level,
    buf: &Buf,
) -> impl tracing::Subscriber + Send + Sync + 'static {
    ANCHOR.get_or_init(|| tracing::Dispatch::new(tracing::subscriber::NoSubscriber::new()));
    let writer = buf.clone();
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish()
}

/// Everything logged at `level` or above while `f` runs, on this thread.
pub(crate) fn capture(level: tracing::Level, f: impl FnOnce()) -> String {
    let buf = Buf::default();
    tracing::subscriber::with_default(subscriber(level, &buf), f);
    buf.text()
}

/// `fut`'s output, and everything logged at `level` or above while it runs (the subscriber goes
/// with the future, whichever thread polls it).
pub(crate) async fn capture_async<T>(
    level: tracing::Level,
    fut: impl Future<Output = T>,
) -> (T, String) {
    let buf = Buf::default();
    let out = fut.with_subscriber(subscriber(level, &buf)).await;
    (out, buf.text())
}

#[cfg(test)]
mod tests {
    /// A log line no other test reaches, so this test is the first to register it.
    fn probe_line() {
        tracing::warn!("test_log probe line");
    }

    #[test]
    fn a_line_first_reached_on_a_thread_with_no_subscriber_is_still_captured() {
        let logs = super::capture(tracing::Level::WARN, || {
            std::thread::spawn(probe_line).join().unwrap();
            probe_line();
        });
        assert!(logs.contains("test_log probe line"), "{logs:?}");
    }
}
