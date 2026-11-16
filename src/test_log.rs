//! Log capture for tests, shared by every module's tests (as `store::fail_writes` is): run code
//! under a `tracing` subscriber that writes plain text (no ANSI colour) into a buffer, then read
//! what it logged. [`capture`] runs a closure, [`capture_async`] a future.

use std::future::Future;
use std::sync::{Arc, Mutex};
use tracing::instrument::WithSubscriber;

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
