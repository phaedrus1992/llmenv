//! The exclusive lock on the whole task store, which also covers the session files.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use anyhow::Context;

thread_local! {
    // The canonical task store folders whose lock this thread holds. Each open of the lock file gets its
    // own `flock`, so a nested call in one thread would wait for itself. A nested call runs in
    // the lock that the thread holds, so a command can check a task and change it under one
    // lock (#3478).
    static HELD: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
}

/// Removes the folder from [`HELD`] when the outer call ends, also on an early return. Only an
/// outer call pushes, so the folders leave in the reverse order of their push.
struct Held;

impl Drop for Held {
    fn drop(&mut self) {
        HELD.with_borrow_mut(Vec::pop);
    }
}

/// Run `f` while holding an exclusive lock on the whole task store, so
/// concurrent `llmenv task` invocations (e.g. from multiple Claude Code
/// sessions on the same project) serialize their read-modify-write cycles
/// instead of racing on the same file. Blocks until the lock is acquired.
///
/// A call inside `f` for the same store on the same thread runs at once, in the lock that the
/// outer call holds. The check compares canonical folders, so two spellings of one store (a
/// link, a relative path) re-enter instead of waiting for themselves.
///
/// # Errors
/// Errors if the store folder or the lock file cannot be made or opened, if the lock fails, or
/// with the error of `f`.
pub(crate) fn with_store_lock<T>(
    state_dir: &Path,
    f: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let dir = super::tasks_dir(state_dir);
    crate::paths::create_dir_owner_only(&dir)
        .with_context(|| format!("cannot make the task store folder {}", dir.display()))?;
    let dir = std::fs::canonicalize(&dir)
        .with_context(|| format!("cannot resolve the task store folder {}", dir.display()))?;
    if HELD.with_borrow(|held| held.contains(&dir)) {
        return f();
    }
    let mut open_options = std::fs::OpenOptions::new();
    open_options.create(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open_options.mode(0o600);
    }
    let lock_path = dir.join(".lock");
    let lock_file = crate::paths::refuse_symlink(&mut open_options)
        .open(&lock_path)
        .with_context(|| {
            format!(
                "cannot open the task store lock {}. Fix: remove it if it is a link",
                lock_path.display()
            )
        })?;
    lock_file
        .lock()
        .with_context(|| format!("cannot lock the task store at {}", lock_path.display()))?;
    HELD.with_borrow_mut(|held| held.push(dir));
    // Declared after `lock_file`, so it drops first: the folder leaves HELD before the unlock.
    let _held = Held;
    f()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn a_nested_call_for_the_same_store_runs_in_the_outer_lock() {
        let dir = TempDir::new().unwrap();
        let inner = with_store_lock(dir.path(), || with_store_lock(dir.path(), || Ok(7))).unwrap();
        assert_eq!(inner, 7);
    }

    #[test]
    fn a_nested_call_through_another_spelling_of_the_store_runs_in_the_outer_lock() {
        let dir = TempDir::new().unwrap();
        let link = dir.path().join("link");
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let dotted = real.join(".");
        let got = with_store_lock(&real, || {
            with_store_lock(&link, || with_store_lock(&dotted, || Ok(3)))
        })
        .unwrap();
        assert_eq!(got, 3);
    }

    #[test]
    fn the_lock_is_released_after_an_error_so_a_later_call_gets_it() {
        let dir = TempDir::new().unwrap();
        let err = with_store_lock(dir.path(), || -> anyhow::Result<()> {
            anyhow::bail!("boom")
        })
        .unwrap_err();
        assert_eq!(err.to_string(), "boom");
        assert!(HELD.with_borrow(Vec::is_empty));
        // Another thread can take the lock, so the file lock is free too.
        let path = dir.path().to_path_buf();
        let got = std::thread::spawn(move || with_store_lock(&path, || Ok(1)))
            .join()
            .unwrap()
            .unwrap();
        assert_eq!(got, 1);
    }

    #[test]
    fn another_thread_waits_for_the_lock() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_path_buf();
        let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (taken_tx, taken_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder_order = std::sync::Arc::clone(&order);
        let holder_path = path.clone();
        let holder = std::thread::spawn(move || {
            with_store_lock(&holder_path, || {
                taken_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                holder_order.lock().unwrap().push("holder");
                Ok(())
            })
            .unwrap();
        });
        taken_rx.recv().unwrap();
        let waiter_order = std::sync::Arc::clone(&order);
        let waiter = std::thread::spawn(move || {
            with_store_lock(&path, || {
                waiter_order.lock().unwrap().push("waiter");
                Ok(())
            })
            .unwrap();
        });
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        waiter.join().unwrap();
        assert_eq!(*order.lock().unwrap(), ["holder", "waiter"]);
    }

    fn nest(dir: &Path, level: usize, depth: usize, fail_at: usize) -> anyhow::Result<usize> {
        with_store_lock(dir, || {
            if level == fail_at {
                anyhow::bail!("fail at {level}");
            }
            if level == depth {
                Ok(level)
            } else {
                nest(dir, level + 1, depth, fail_at)
            }
        })
    }

    proptest::proptest! {
        // Each case takes a file lock, so the case count stays low.
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(32))]

        // At any nesting depth, with an error at any depth or none, the outer call returns the
        // error or the value, HELD is empty after it, and another thread can take the lock.
        #[test]
        fn any_nesting_depth_leaves_the_lock_free(depth in 1usize..=6, fail_at in 0usize..=6) {
            let dir = TempDir::new().unwrap();
            let got = nest(dir.path(), 1, depth, fail_at).map_err(|e| e.to_string());
            let expected = if (1..=depth).contains(&fail_at) {
                Err(format!("fail at {fail_at}"))
            } else {
                Ok(depth)
            };
            proptest::prop_assert_eq!(got, expected);
            proptest::prop_assert!(HELD.with_borrow(Vec::is_empty));
            let path = dir.path().to_path_buf();
            let other = std::thread::spawn(move || with_store_lock(&path, || Ok(1)))
                .join()
                .unwrap()
                .unwrap();
            proptest::prop_assert_eq!(other, 1);
        }
    }
}
