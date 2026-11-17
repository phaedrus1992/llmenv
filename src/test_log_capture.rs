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
///
/// A process-global registry subscriber is installed once, so every callsite is registered as
/// "always interested". Without it, a callsite that another thread registers while only the no-op
/// dispatcher is active can cache "never" and skip this scoped subscriber (#3444).
pub(crate) fn capture_logs(f: impl FnOnce()) -> String {
    static GLOBAL: std::sync::Once = std::sync::Once::new();
    GLOBAL.call_once(|| {
        // A global default that another test set first has the same effect, so the error is fine.
        let _ = tracing::subscriber::set_global_default(tracing_subscriber::registry());
    });
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

    /// #3444: every captured call must see its own log line, even when other threads create their
    /// scoped subscribers and register new callsites at the same time.
    #[test]
    fn a_scoped_capture_never_loses_a_line_to_a_parallel_registration() {
        let calls: Vec<fn()> = vec![
            || tracing::info!("callsite-line-0"),
            || tracing::info!("callsite-line-1"),
            || tracing::info!("callsite-line-2"),
            || tracing::info!("callsite-line-3"),
            || tracing::info!("callsite-line-4"),
            || tracing::info!("callsite-line-5"),
            || tracing::info!("callsite-line-6"),
            || tracing::info!("callsite-line-7"),
            || tracing::info!("callsite-line-8"),
            || tracing::info!("callsite-line-9"),
            || tracing::info!("callsite-line-10"),
            || tracing::info!("callsite-line-11"),
            || tracing::info!("callsite-line-12"),
            || tracing::info!("callsite-line-13"),
            || tracing::info!("callsite-line-14"),
            || tracing::info!("callsite-line-15"),
            || tracing::info!("callsite-line-16"),
            || tracing::info!("callsite-line-17"),
            || tracing::info!("callsite-line-18"),
            || tracing::info!("callsite-line-19"),
            || tracing::info!("callsite-line-20"),
            || tracing::info!("callsite-line-21"),
            || tracing::info!("callsite-line-22"),
            || tracing::info!("callsite-line-23"),
            || tracing::info!("callsite-line-24"),
            || tracing::info!("callsite-line-25"),
            || tracing::info!("callsite-line-26"),
            || tracing::info!("callsite-line-27"),
            || tracing::info!("callsite-line-28"),
            || tracing::info!("callsite-line-29"),
            || tracing::info!("callsite-line-30"),
            || tracing::info!("callsite-line-31"),
            || tracing::info!("callsite-line-32"),
            || tracing::info!("callsite-line-33"),
            || tracing::info!("callsite-line-34"),
            || tracing::info!("callsite-line-35"),
            || tracing::info!("callsite-line-36"),
            || tracing::info!("callsite-line-37"),
            || tracing::info!("callsite-line-38"),
            || tracing::info!("callsite-line-39"),
            || tracing::info!("callsite-line-40"),
            || tracing::info!("callsite-line-41"),
            || tracing::info!("callsite-line-42"),
            || tracing::info!("callsite-line-43"),
            || tracing::info!("callsite-line-44"),
            || tracing::info!("callsite-line-45"),
            || tracing::info!("callsite-line-46"),
            || tracing::info!("callsite-line-47"),
            || tracing::info!("callsite-line-48"),
            || tracing::info!("callsite-line-49"),
            || tracing::info!("callsite-line-50"),
            || tracing::info!("callsite-line-51"),
            || tracing::info!("callsite-line-52"),
            || tracing::info!("callsite-line-53"),
            || tracing::info!("callsite-line-54"),
            || tracing::info!("callsite-line-55"),
            || tracing::info!("callsite-line-56"),
            || tracing::info!("callsite-line-57"),
            || tracing::info!("callsite-line-58"),
            || tracing::info!("callsite-line-59"),
            || tracing::info!("callsite-line-60"),
            || tracing::info!("callsite-line-61"),
            || tracing::info!("callsite-line-62"),
            || tracing::info!("callsite-line-63"),
            || tracing::info!("callsite-line-64"),
            || tracing::info!("callsite-line-65"),
            || tracing::info!("callsite-line-66"),
            || tracing::info!("callsite-line-67"),
            || tracing::info!("callsite-line-68"),
            || tracing::info!("callsite-line-69"),
            || tracing::info!("callsite-line-70"),
            || tracing::info!("callsite-line-71"),
            || tracing::info!("callsite-line-72"),
            || tracing::info!("callsite-line-73"),
            || tracing::info!("callsite-line-74"),
            || tracing::info!("callsite-line-75"),
            || tracing::info!("callsite-line-76"),
            || tracing::info!("callsite-line-77"),
            || tracing::info!("callsite-line-78"),
            || tracing::info!("callsite-line-79"),
            || tracing::info!("callsite-line-80"),
            || tracing::info!("callsite-line-81"),
            || tracing::info!("callsite-line-82"),
            || tracing::info!("callsite-line-83"),
            || tracing::info!("callsite-line-84"),
            || tracing::info!("callsite-line-85"),
            || tracing::info!("callsite-line-86"),
            || tracing::info!("callsite-line-87"),
            || tracing::info!("callsite-line-88"),
            || tracing::info!("callsite-line-89"),
            || tracing::info!("callsite-line-90"),
            || tracing::info!("callsite-line-91"),
            || tracing::info!("callsite-line-92"),
            || tracing::info!("callsite-line-93"),
            || tracing::info!("callsite-line-94"),
            || tracing::info!("callsite-line-95"),
            || tracing::info!("callsite-line-96"),
            || tracing::info!("callsite-line-97"),
            || tracing::info!("callsite-line-98"),
            || tracing::info!("callsite-line-99"),
            || tracing::info!("callsite-line-100"),
            || tracing::info!("callsite-line-101"),
            || tracing::info!("callsite-line-102"),
            || tracing::info!("callsite-line-103"),
            || tracing::info!("callsite-line-104"),
            || tracing::info!("callsite-line-105"),
            || tracing::info!("callsite-line-106"),
            || tracing::info!("callsite-line-107"),
            || tracing::info!("callsite-line-108"),
            || tracing::info!("callsite-line-109"),
            || tracing::info!("callsite-line-110"),
            || tracing::info!("callsite-line-111"),
            || tracing::info!("callsite-line-112"),
            || tracing::info!("callsite-line-113"),
            || tracing::info!("callsite-line-114"),
            || tracing::info!("callsite-line-115"),
            || tracing::info!("callsite-line-116"),
            || tracing::info!("callsite-line-117"),
            || tracing::info!("callsite-line-118"),
            || tracing::info!("callsite-line-119"),
            || tracing::info!("callsite-line-120"),
            || tracing::info!("callsite-line-121"),
            || tracing::info!("callsite-line-122"),
            || tracing::info!("callsite-line-123"),
            || tracing::info!("callsite-line-124"),
            || tracing::info!("callsite-line-125"),
            || tracing::info!("callsite-line-126"),
            || tracing::info!("callsite-line-127"),
            || tracing::info!("callsite-line-128"),
            || tracing::info!("callsite-line-129"),
            || tracing::info!("callsite-line-130"),
            || tracing::info!("callsite-line-131"),
            || tracing::info!("callsite-line-132"),
            || tracing::info!("callsite-line-133"),
            || tracing::info!("callsite-line-134"),
            || tracing::info!("callsite-line-135"),
            || tracing::info!("callsite-line-136"),
            || tracing::info!("callsite-line-137"),
            || tracing::info!("callsite-line-138"),
            || tracing::info!("callsite-line-139"),
            || tracing::info!("callsite-line-140"),
            || tracing::info!("callsite-line-141"),
            || tracing::info!("callsite-line-142"),
            || tracing::info!("callsite-line-143"),
            || tracing::info!("callsite-line-144"),
            || tracing::info!("callsite-line-145"),
            || tracing::info!("callsite-line-146"),
            || tracing::info!("callsite-line-147"),
            || tracing::info!("callsite-line-148"),
            || tracing::info!("callsite-line-149"),
            || tracing::info!("callsite-line-150"),
            || tracing::info!("callsite-line-151"),
            || tracing::info!("callsite-line-152"),
            || tracing::info!("callsite-line-153"),
            || tracing::info!("callsite-line-154"),
            || tracing::info!("callsite-line-155"),
            || tracing::info!("callsite-line-156"),
            || tracing::info!("callsite-line-157"),
            || tracing::info!("callsite-line-158"),
            || tracing::info!("callsite-line-159"),
            || tracing::info!("callsite-line-160"),
            || tracing::info!("callsite-line-161"),
            || tracing::info!("callsite-line-162"),
            || tracing::info!("callsite-line-163"),
            || tracing::info!("callsite-line-164"),
            || tracing::info!("callsite-line-165"),
            || tracing::info!("callsite-line-166"),
            || tracing::info!("callsite-line-167"),
            || tracing::info!("callsite-line-168"),
            || tracing::info!("callsite-line-169"),
            || tracing::info!("callsite-line-170"),
            || tracing::info!("callsite-line-171"),
            || tracing::info!("callsite-line-172"),
            || tracing::info!("callsite-line-173"),
            || tracing::info!("callsite-line-174"),
            || tracing::info!("callsite-line-175"),
            || tracing::info!("callsite-line-176"),
            || tracing::info!("callsite-line-177"),
            || tracing::info!("callsite-line-178"),
            || tracing::info!("callsite-line-179"),
            || tracing::info!("callsite-line-180"),
            || tracing::info!("callsite-line-181"),
            || tracing::info!("callsite-line-182"),
            || tracing::info!("callsite-line-183"),
            || tracing::info!("callsite-line-184"),
            || tracing::info!("callsite-line-185"),
            || tracing::info!("callsite-line-186"),
            || tracing::info!("callsite-line-187"),
            || tracing::info!("callsite-line-188"),
            || tracing::info!("callsite-line-189"),
            || tracing::info!("callsite-line-190"),
            || tracing::info!("callsite-line-191"),
            || tracing::info!("callsite-line-192"),
            || tracing::info!("callsite-line-193"),
            || tracing::info!("callsite-line-194"),
            || tracing::info!("callsite-line-195"),
            || tracing::info!("callsite-line-196"),
            || tracing::info!("callsite-line-197"),
            || tracing::info!("callsite-line-198"),
            || tracing::info!("callsite-line-199"),
            || tracing::info!("callsite-line-200"),
            || tracing::info!("callsite-line-201"),
            || tracing::info!("callsite-line-202"),
            || tracing::info!("callsite-line-203"),
            || tracing::info!("callsite-line-204"),
            || tracing::info!("callsite-line-205"),
            || tracing::info!("callsite-line-206"),
            || tracing::info!("callsite-line-207"),
            || tracing::info!("callsite-line-208"),
            || tracing::info!("callsite-line-209"),
            || tracing::info!("callsite-line-210"),
            || tracing::info!("callsite-line-211"),
            || tracing::info!("callsite-line-212"),
            || tracing::info!("callsite-line-213"),
            || tracing::info!("callsite-line-214"),
            || tracing::info!("callsite-line-215"),
            || tracing::info!("callsite-line-216"),
            || tracing::info!("callsite-line-217"),
            || tracing::info!("callsite-line-218"),
            || tracing::info!("callsite-line-219"),
            || tracing::info!("callsite-line-220"),
            || tracing::info!("callsite-line-221"),
            || tracing::info!("callsite-line-222"),
            || tracing::info!("callsite-line-223"),
            || tracing::info!("callsite-line-224"),
            || tracing::info!("callsite-line-225"),
            || tracing::info!("callsite-line-226"),
            || tracing::info!("callsite-line-227"),
            || tracing::info!("callsite-line-228"),
            || tracing::info!("callsite-line-229"),
            || tracing::info!("callsite-line-230"),
            || tracing::info!("callsite-line-231"),
            || tracing::info!("callsite-line-232"),
            || tracing::info!("callsite-line-233"),
            || tracing::info!("callsite-line-234"),
            || tracing::info!("callsite-line-235"),
            || tracing::info!("callsite-line-236"),
            || tracing::info!("callsite-line-237"),
            || tracing::info!("callsite-line-238"),
            || tracing::info!("callsite-line-239"),
            || tracing::info!("callsite-line-240"),
            || tracing::info!("callsite-line-241"),
            || tracing::info!("callsite-line-242"),
            || tracing::info!("callsite-line-243"),
            || tracing::info!("callsite-line-244"),
            || tracing::info!("callsite-line-245"),
            || tracing::info!("callsite-line-246"),
            || tracing::info!("callsite-line-247"),
            || tracing::info!("callsite-line-248"),
            || tracing::info!("callsite-line-249"),
            || tracing::info!("callsite-line-250"),
            || tracing::info!("callsite-line-251"),
            || tracing::info!("callsite-line-252"),
            || tracing::info!("callsite-line-253"),
            || tracing::info!("callsite-line-254"),
            || tracing::info!("callsite-line-255"),
            || tracing::info!("callsite-line-256"),
            || tracing::info!("callsite-line-257"),
            || tracing::info!("callsite-line-258"),
            || tracing::info!("callsite-line-259"),
            || tracing::info!("callsite-line-260"),
            || tracing::info!("callsite-line-261"),
            || tracing::info!("callsite-line-262"),
            || tracing::info!("callsite-line-263"),
            || tracing::info!("callsite-line-264"),
            || tracing::info!("callsite-line-265"),
            || tracing::info!("callsite-line-266"),
            || tracing::info!("callsite-line-267"),
            || tracing::info!("callsite-line-268"),
            || tracing::info!("callsite-line-269"),
            || tracing::info!("callsite-line-270"),
            || tracing::info!("callsite-line-271"),
            || tracing::info!("callsite-line-272"),
            || tracing::info!("callsite-line-273"),
            || tracing::info!("callsite-line-274"),
            || tracing::info!("callsite-line-275"),
            || tracing::info!("callsite-line-276"),
            || tracing::info!("callsite-line-277"),
            || tracing::info!("callsite-line-278"),
            || tracing::info!("callsite-line-279"),
            || tracing::info!("callsite-line-280"),
            || tracing::info!("callsite-line-281"),
            || tracing::info!("callsite-line-282"),
            || tracing::info!("callsite-line-283"),
            || tracing::info!("callsite-line-284"),
            || tracing::info!("callsite-line-285"),
            || tracing::info!("callsite-line-286"),
            || tracing::info!("callsite-line-287"),
            || tracing::info!("callsite-line-288"),
            || tracing::info!("callsite-line-289"),
            || tracing::info!("callsite-line-290"),
            || tracing::info!("callsite-line-291"),
            || tracing::info!("callsite-line-292"),
            || tracing::info!("callsite-line-293"),
            || tracing::info!("callsite-line-294"),
            || tracing::info!("callsite-line-295"),
            || tracing::info!("callsite-line-296"),
            || tracing::info!("callsite-line-297"),
            || tracing::info!("callsite-line-298"),
            || tracing::info!("callsite-line-299"),
        ];
        let barrier = std::sync::Barrier::new(8);
        let lost = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for thread in 0..8usize {
                let (calls, barrier, lost) = (&calls, &barrier, &lost);
                scope.spawn(move || {
                    barrier.wait();
                    for step in 0..calls.len() {
                        let index = (step * 7 + thread * 37) % calls.len();
                        // Half of the threads log with no scoped subscriber, the way a test that
                        // does not capture does, so they register callsites in the race.
                        if thread % 2 == 1 {
                            calls[index]();
                            continue;
                        }
                        let logs = capture_logs(calls[index]);
                        if !logs.contains(&format!("callsite-line-{index}")) {
                            lost.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        }
                    }
                });
            }
        });
        assert_eq!(
            lost.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "lost log lines"
        );
    }
}
