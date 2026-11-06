//! The exclusive lock on the whole task store, which also covers the session files.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

thread_local! {
    // The task store folders whose lock this thread holds. Each open of the lock file gets its
    // own `flock`, so a nested call in one thread would wait for itself. A nested call runs in
    // the lock that the thread holds, so a command can check a task and change it under one
    // lock (#3478).
    static HELD: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
}

/// Removes the folder from [`HELD`] when the outer call ends, also on an early return.
struct Held(PathBuf);

impl Drop for Held {
    fn drop(&mut self) {
        HELD.with_borrow_mut(|held| {
            if let Some(i) = held.iter().rposition(|d| *d == self.0) {
                held.swap_remove(i);
            }
        });
    }
}

/// Run `f` while holding an exclusive lock on the whole task store, so
/// concurrent `llmenv task` invocations (e.g. from multiple Claude Code
/// sessions on the same project) serialize their read-modify-write cycles
/// instead of racing on the same file. Blocks until the lock is acquired.
///
/// A call inside `f` for the same `state_dir` on the same thread runs at once, in the lock
/// that the outer call holds. Callers pass one spelling of `state_dir` for one store, because
/// the check compares paths, not files.
///
/// # Errors
/// Errors if the store folder or the lock file cannot be made or opened, if the lock fails, or
/// with the error of `f`.
pub(crate) fn with_store_lock<T>(
    state_dir: &Path,
    f: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let dir = super::tasks_dir(state_dir);
    if HELD.with_borrow(|held| held.contains(&dir)) {
        return f();
    }
    crate::paths::create_dir_owner_only(&dir)?;
    let mut open_options = std::fs::OpenOptions::new();
    open_options.create(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open_options.mode(0o600);
    }
    let lock_file = crate::paths::refuse_symlink(&mut open_options).open(dir.join(".lock"))?;
    lock_file.lock()?;
    HELD.with_borrow_mut(|held| held.push(dir.clone()));
    // Declared after `lock_file`, so it drops first: the folder leaves HELD before the unlock.
    let _held = Held(dir);
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
}
