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

/// Install the process-global registry subscriber once. Every scoped capture calls it first. The
/// error is dropped on purpose: a test that installs its own global default in a child process
/// does not reach this path, and a global set by any other code that runs here has no level
/// filter in the test binary.
pub(crate) fn ensure_global_registry() {
    static GLOBAL: std::sync::Once = std::sync::Once::new();
    GLOBAL.call_once(|| {
        let _ = tracing::subscriber::set_global_default(tracing_subscriber::registry());
    });
}

/// Run `f` with a `tracing` subscriber installed for the duration of the
/// call (current-thread scoped, doesn't affect other tests), returning
/// whatever it logged. Captures every level a test might assert on — the
/// subscriber's max level is a test-harness knob, unrelated to production's
/// `RUST_LOG`-driven filter.
///
/// A process-global registry subscriber is installed once, so every callsite is registered as
/// "always interested". Without it, a callsite that another thread registers while only the no-op
/// dispatcher is active can cache "never" and skip this scoped subscriber (#3444).
pub(crate) fn capture_logs(f: impl FnOnce()) -> String {
    ensure_global_registry();
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

#[cfg(test)]
mod tests {
    use super::capture_logs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Barrier, Mutex};

    /// A list of closures that each log from a callsite of their own. Every repetition of the macro
    /// body expands to a separate callsite, and a callsite registers on its first call.
    macro_rules! sites {
        ($($n:literal)*) => {
            vec![$((|| tracing::info!("callsite-line-{}", $n)) as fn()),*]
        };
    }

    /// Run `rounds` captures on each of `threads` threads. Odd threads log with no scoped
    /// subscriber, so they register callsites while the even threads create scoped ones. Returns the
    /// first lost `(thread, index)`, if any.
    fn first_lost_line(calls: &[fn()], threads: usize) -> Option<(usize, usize)> {
        let barrier = Barrier::new(threads);
        let first = Mutex::new(None);
        let lost = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for thread in 0..threads {
                let (barrier, first, lost) = (&barrier, &first, &lost);
                scope.spawn(move || {
                    barrier.wait();
                    for step in 0..calls.len() {
                        let index = (step * 7 + thread * 37) % calls.len();
                        if thread % 2 == 1 {
                            calls[index]();
                            continue;
                        }
                        let logs = capture_logs(calls[index]);
                        if !logs.contains(&format!("callsite-line-{index}")) {
                            lost.fetch_add(1, Ordering::SeqCst);
                            if let Ok(mut f) = first.lock() {
                                f.get_or_insert((thread, index));
                            }
                        }
                    }
                });
            }
        });
        first.into_inner().ok().flatten()
    }

    /// #3444: every captured call sees its own log line, even when other threads create scoped
    /// subscribers and register new callsites at the same time. Without the global registry this
    /// lost a line in about 1 of 250 runs, so this test is a probabilistic guard.
    #[test]
    fn a_scoped_capture_never_loses_a_line_to_a_parallel_registration() {
        let calls: Vec<fn()> = sites!(
            0 1 2 3 4 5 6 7 8 9 10 11 12 13 14
            15 16 17 18 19 20 21 22 23 24 25 26 27 28 29
            30 31 32 33 34 35 36 37 38 39 40 41 42 43 44
            45 46 47 48 49 50 51 52 53 54 55 56 57 58 59
            60 61 62 63 64 65 66 67 68 69 70 71 72 73 74
            75 76 77 78 79 80 81 82 83 84 85 86 87 88 89
            90 91 92 93 94 95 96 97 98 99 100 101 102 103 104
            105 106 107 108 109 110 111 112 113 114 115 116 117 118 119
            120 121 122 123 124 125 126 127 128 129 130 131 132 133 134
            135 136 137 138 139 140 141 142 143 144 145 146 147 148 149
            150 151 152 153 154 155 156 157 158 159 160 161 162 163 164
            165 166 167 168 169 170 171 172 173 174 175 176 177 178 179
            180 181 182 183 184 185 186 187 188 189 190 191 192 193 194
            195 196 197 198 199 200 201 202 203 204 205 206 207 208 209
            210 211 212 213 214 215 216 217 218 219 220 221 222 223 224
            225 226 227 228 229 230 231 232 233 234 235 236 237 238 239
            240 241 242 243 244 245 246 247 248 249 250 251 252 253 254
            255 256 257 258 259 260 261 262 263 264 265 266 267 268 269
            270 271 272 273 274 275 276 277 278 279 280 281 282 283 284
            285 286 287 288 289 290 291 292 293 294 295 296 297 298 299
        );
        assert_eq!(
            first_lost_line(&calls, 8),
            None,
            "a log line was lost: (thread, index)"
        );
    }

    /// A capture sees the line of its own closure, and a closure that logs nothing captures nothing.
    #[test]
    fn a_capture_holds_exactly_its_own_lines() {
        let seen = capture_logs(|| tracing::warn!("only-this-line"));
        assert!(seen.contains("only-this-line"));
        assert!(capture_logs(|| ()).is_empty());
    }
}
