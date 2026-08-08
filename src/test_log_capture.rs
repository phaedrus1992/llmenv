//! Test-only tracing log capture helper (#1207).
//!
//! Lets a unit test assert that a specific `tracing` event fired, without
//! pulling in the `tracing-test` crate for a handful of call sites —
//! `tracing-subscriber` is already a full (non-dev) workspace dependency and
//! already ships a `Mutex<W>` `MakeWriter` impl, so this just reuses it.

#![expect(clippy::expect_used, reason = "test-only helper")]

use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::writer::MutexGuardWriter;

#[derive(Clone, Default)]
struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

impl<'a> MakeWriter<'a> for CapturedLogs {
    type Writer = MutexGuardWriter<'a, Vec<u8>>;

    fn make_writer(&'a self) -> Self::Writer {
        self.0.make_writer()
    }
}

/// Run `f` with a `tracing` subscriber installed for the duration of the
/// call (current-thread scoped, doesn't affect other tests), returning
/// whatever it logged. Captures every level a test might assert on — the
/// subscriber's max level is a test-harness knob, unrelated to production's
/// `RUST_LOG`-driven filter.
pub(crate) fn capture_logs(f: impl FnOnce()) -> String {
    let captured = CapturedLogs::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_max_level(tracing::Level::TRACE)
        .without_time()
        .finish();
    tracing::subscriber::with_default(subscriber, f);
    let buf = captured.0.lock().expect("test log buffer mutex poisoned");
    String::from_utf8_lossy(&buf).into_owned()
}
