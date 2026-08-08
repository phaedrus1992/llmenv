//! Test-only tracing log capture helper (#1207).
//!
//! Lets a unit test assert that a specific `tracing::debug!` event fired,
//! without pulling in the `tracing-test` crate for two call sites —
//! `tracing-subscriber` is already a full (non-dev) workspace dependency, so
//! this reuses its `fmt` layer with a buffer-backed writer instead.

#![expect(clippy::expect_used, reason = "test-only helper")]

use std::io;
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;

/// Logs captured by [`capture_debug_logs`], queryable by substring.
#[derive(Clone, Default)]
pub(crate) struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

impl CapturedLogs {
    pub(crate) fn contains(&self, needle: &str) -> bool {
        let buf = self.0.lock().expect("test log buffer mutex poisoned");
        String::from_utf8_lossy(&buf).contains(needle)
    }
}

pub(crate) struct Writer(Arc<Mutex<Vec<u8>>>);

impl io::Write for Writer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("test log buffer mutex poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CapturedLogs {
    type Writer = Writer;

    fn make_writer(&'a self) -> Self::Writer {
        Writer(self.0.clone())
    }
}

/// Run `f` with a debug-level `tracing` subscriber installed for the
/// duration of the call (current-thread scoped, doesn't affect other
/// tests), then return whatever it logged.
pub(crate) fn capture_debug_logs(f: impl FnOnce()) -> CapturedLogs {
    let captured = CapturedLogs::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_max_level(tracing::Level::DEBUG)
        .without_time()
        .finish();
    tracing::subscriber::with_default(subscriber, f);
    captured
}
