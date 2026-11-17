//! Adds the `--task` titles of `llmenv task session start` after the session exists.

use anyhow::Context as _;

/// Add each title through `add`. The session is already started when this runs, so a failure
/// names the tasks added before it. Without that list the user cannot tell which tasks exist
/// (#3750).
///
/// # Errors
/// Errors on the first title that `add` rejects.
pub(super) fn add_session_tasks(
    session_id: &str,
    titles: &[String],
    mut add: impl FnMut(&str) -> anyhow::Result<crate::task::Task>,
) -> anyhow::Result<()> {
    let mut added: Vec<String> = Vec::new();
    for title in titles {
        match add(title) {
            Ok(task) => added.push(task.slug),
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "session '{session_id}' started, but task '{title}' was not added; tasks \
                         already added: [{}]. Fix: add the rest with `llmenv task add --session \
                         {session_id} <title>`.",
                        added.join(", ")
                    )
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn task(slug: &str) -> crate::task::Task {
        serde_json::from_value(serde_json::json!({
            "slug": slug,
            "title": slug,
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
        }))
        .unwrap_or_else(|e| panic!("test task must deserialize: {e}"))
    }

    #[test]
    fn a_failed_second_add_names_the_first_task() {
        let titles = vec!["one".to_owned(), "two".to_owned(), "three".to_owned()];
        let mut calls = 0;
        let err = add_session_tasks("s", &titles, |t| {
            calls += 1;
            if t == "two" {
                anyhow::bail!("save failed")
            }
            Ok(task(t))
        })
        .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("task 'two' was not added"), "{text}");
        assert!(text.contains("tasks already added: [one]"), "{text}");
        assert!(text.contains("--session s"), "{text}");
        assert!(text.contains("save failed"), "{text}");
        assert_eq!(calls, 2, "the loop must stop at the first failure");
    }

    proptest! {
        /// The error names exactly the tasks added before the failing title.
        #[test]
        fn the_error_names_the_tasks_before_the_failure(count in 1usize..6, fail_at in 0usize..6) {
            let titles: Vec<String> = (0..count).map(|n| format!("t{n}")).collect();
            let result = add_session_tasks("s", &titles, |t| {
                if t == format!("t{fail_at}") { anyhow::bail!("boom") }
                Ok(task(t))
            });
            if fail_at >= count {
                prop_assert!(result.is_ok());
            } else {
                let text = format!("{:#}", result.unwrap_err());
                let before = titles[..fail_at].join(", ");
                let want = format!("tasks already added: [{before}]");
                prop_assert!(text.contains(&want), "{}", text);
            }
        }
    }
}
