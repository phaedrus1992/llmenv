//! Adds the `--task` titles of `llmenv task session start` after the session exists.

use anyhow::Context as _;

/// Add each title through `add`, and return the slugs that were added. The session is already
/// started when this runs, so a failure names the tasks added before it. Without that list the
/// user cannot tell which tasks exist (#3750).
///
/// # Errors
/// Errors on the first title that `add` rejects.
pub(super) fn add_session_tasks(
    session_id: &str,
    titles: &[String],
    mut add: impl FnMut(&str) -> anyhow::Result<crate::task::Task>,
) -> anyhow::Result<Vec<crate::task::Task>> {
    let mut added: Vec<crate::task::Task> = Vec::new();
    for title in titles {
        match add(title) {
            Ok(task) => added.push(task),
            Err(e) => {
                let done = if added.is_empty() {
                    "no task was added".to_owned()
                } else {
                    let slugs: Vec<&str> = added.iter().map(|t| t.slug.as_str()).collect();
                    format!("tasks already added: {}", slugs.join(", "))
                };
                return Err(e).with_context(|| {
                    format!(
                        "session '{session_id}' started, but task '{title}' was not added \
                         ({done}). Fix: add the rest with `llmenv task add`."
                    )
                });
            }
        }
    }
    Ok(added)
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

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
        assert!(text.contains("tasks already added: one"), "{text}");
        assert!(text.contains("save failed"), "{text}");
        assert_eq!(calls, 2, "the loop must stop at the first failure");
    }

    #[test]
    fn a_failed_first_add_says_none_were_added() {
        let titles = vec!["one".to_owned()];
        let err = add_session_tasks("s", &titles, |_| anyhow::bail!("boom")).unwrap_err();
        assert!(format!("{err:#}").contains("no task was added"));
    }
}
