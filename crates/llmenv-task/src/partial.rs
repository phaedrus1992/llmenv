//! The error of a bulk task write that failed after it had changed some tasks (#3749).

use super::Task;

/// A save failed part way through a list of tasks. `changed` holds the tasks that were saved
/// before the failure, so the caller can record its audit note and touch their sessions before
/// it reports the error. A caller that uses `?` still gets the failure text.
#[derive(Debug, thiserror::Error)]
#[error("{cause:#}")]
pub struct PartialWrite {
    changed: Vec<Task>,
    cause: anyhow::Error,
}

impl PartialWrite {
    /// Wrap `cause` as an [`anyhow::Error`] that carries the tasks that were already saved.
    pub fn error(changed: Vec<Task>, cause: anyhow::Error) -> anyhow::Error {
        anyhow::Error::new(Self { changed, cause })
    }
}

/// The tasks that `error` says were saved before it failed. Empty for any other error, and for
/// a [`PartialWrite`] that a caller rebuilt as a new error, which loses the list. Every layer
/// between the writer and the caller MUST keep the error with `?` or `.context()`.
pub fn changed_before_failure(error: &anyhow::Error) -> &[Task] {
    error
        .downcast_ref::<PartialWrite>()
        .map_or(&[], |p| p.changed.as_slice())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn task(slug: &str) -> Task {
        serde_json::from_value(serde_json::json!({
            "slug": slug,
            "title": slug,
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
        }))
        .unwrap()
    }

    fn wrap(mut error: anyhow::Error, layers: usize) -> anyhow::Error {
        for n in 0..layers {
            error = error.context(format!("layer {n}"));
        }
        error
    }

    proptest! {
        /// The list survives any number of `.context()` layers, and a plain error has none.
        #[test]
        fn the_list_survives_context_layers(count in 0usize..5, layers in 0usize..5) {
            let tasks: Vec<Task> = (0..count).map(|n| task(&format!("t{n}"))).collect();
            let slugs: Vec<String> = tasks.iter().map(|t| t.slug.clone()).collect();
            let error = wrap(PartialWrite::error(tasks, anyhow::anyhow!("save failed")), layers);
            let got: Vec<String> =
                changed_before_failure(&error).iter().map(|t| t.slug.clone()).collect();
            prop_assert_eq!(got, slugs);
            let text = format!("{error:#}");
            prop_assert!(text.contains("save failed"), "{}", text);
            prop_assert!(changed_before_failure(&wrap(anyhow::anyhow!("plain"), layers)).is_empty());
        }
    }

    #[test]
    fn the_list_survives_the_store_lock() {
        let dir = tempfile::TempDir::new().unwrap();
        let error = crate::with_store_lock(dir.path(), || -> anyhow::Result<()> {
            Err(PartialWrite::error(
                vec![task("a")],
                anyhow::anyhow!("save failed"),
            ))
        })
        .unwrap_err();
        assert_eq!(changed_before_failure(&error).len(), 1);
    }
}
