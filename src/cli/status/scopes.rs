//! `llmenv status scopes`: each configured scope, shown as active, unknown, or inactive.

use crate::config::Config;
use crate::paths;
use std::collections::HashSet;

pub(super) fn run_scope_ls(use_color: bool) -> anyhow::Result<()> {
    let config_path = paths::config_path()?;
    let config = Config::load(&config_path)?;
    let env = crate::scope::matcher::Env::detect_for_config(&config)?;
    let active = crate::scope::evaluate(&config, &env);
    let consumed = crate::cli::all_consumed_tags(&config);

    let active_ids: HashSet<(&str, &str)> = active
        .scopes
        .iter()
        .map(|s| (s.kind, s.id.as_str()))
        .collect();

    let mut rows: Vec<(String, bool, bool)> = Vec::new();
    let push = |rows: &mut Vec<(String, bool, bool)>,
                kind: &str,
                id: &str,
                tags: &[String],
                active_ids: &HashSet<(&str, &str)>,
                consumed: &HashSet<String>| {
        let is_active = active_ids.contains(&(kind, id));
        let is_orphan = !tags.iter().any(|t| consumed.contains(t));
        rows.push((format!("{}:{}", kind, id), is_active, is_orphan));
    };
    for s in &config.scope.network {
        push(&mut rows, "network", &s.id, &s.tags, &active_ids, &consumed);
    }
    for s in &config.scope.host {
        push(&mut rows, "host", &s.id, &s.tags, &active_ids, &consumed);
    }
    for s in &config.scope.user {
        push(&mut rows, "user", &s.id, &s.tags, &active_ids, &consumed);
    }
    // #845: content scopes were omitted here entirely — `llmenv status scopes`
    // never listed a configured content scope, active or not.
    for s in &config.scope.content {
        push(&mut rows, "content", &s.id, &s.tags, &active_ids, &consumed);
    }
    for scope in &active.scopes {
        if scope.kind == "project" {
            push(
                &mut rows,
                "project",
                &scope.id,
                &scope.tags,
                &active_ids,
                &consumed,
            );
        }
    }

    rows.sort_by(|a, b| a.0.cmp(&b.0));
    if rows.is_empty() {
        println!("(none configured)");
        return Ok(());
    }
    // A content scope is unknown when the walk could not read part of the tree (#3793).
    let unknown_rows: HashSet<String> = active
        .unknown_content
        .iter()
        .map(|id| format!("content:{id}"))
        .collect();
    for (name, is_active, is_orphan) in rows {
        let mark = if is_active {
            crate::cli::active_marker(use_color)
        } else {
            " ".to_string()
        };
        let annotation = if !is_active && unknown_rows.contains(&name) {
            format!(" {}", crate::cli::style::unknown_annotation(use_color))
        } else {
            crate::cli::annotate(is_active, is_orphan, use_color)
        };
        println!("{} {}{}", mark, name, annotation);
    }
    Ok(())
}
