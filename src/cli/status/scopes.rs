//! `llmenv status scopes`: each configured scope, shown as active, unknown, or inactive.

use crate::config::Config;
use crate::paths;
use crate::scope::ActiveScopes;
use std::collections::HashSet;

/// One row of the listing: the scope name and its state (#3793).
#[derive(Debug, PartialEq, Eq)]
struct ScopeRow {
    name: String,
    is_active: bool,
    is_orphan: bool,
    is_unknown: bool,
}

pub(super) fn run_scope_ls(use_color: bool) -> anyhow::Result<()> {
    let config_path = paths::config_path()?;
    let config = Config::load(&config_path)?;
    let env = crate::scope::matcher::Env::detect_for_config(&config)?;
    let active = crate::scope::evaluate(&config, &env);
    let rows = scope_rows(&config, &active);
    if rows.is_empty() {
        println!("(none configured)");
        return Ok(());
    }
    for row in &rows {
        println!("{}", scope_line(row, use_color));
    }
    Ok(())
}

/// The rows of the listing, sorted by name. A content scope is unknown when the walk could not
/// read part of the tree (#3793).
fn scope_rows(config: &Config, active: &ActiveScopes) -> Vec<ScopeRow> {
    let consumed = crate::cli::all_consumed_tags(config);
    let active_ids: HashSet<(&str, &str)> = active
        .scopes
        .iter()
        .map(|s| (s.kind, s.id.as_str()))
        .collect();
    let mut rows = Vec::new();
    let mut push = |kind: &str, id: &str, tags: &[String]| {
        rows.push(ScopeRow {
            name: format!("{kind}:{id}"),
            is_active: active_ids.contains(&(kind, id)),
            is_orphan: !tags.iter().any(|t| consumed.contains(t)),
            is_unknown: kind == "content" && active.unknown_content.contains(id),
        });
    };
    for s in &config.scope.network {
        push("network", &s.id, &s.tags);
    }
    for s in &config.scope.host {
        push("host", &s.id, &s.tags);
    }
    for s in &config.scope.user {
        push("user", &s.id, &s.tags);
    }
    // #845: content scopes were omitted here entirely — `llmenv status scopes`
    // never listed a configured content scope, active or not.
    for s in &config.scope.content {
        push("content", &s.id, &s.tags);
    }
    for scope in &active.scopes {
        if scope.kind == "project" {
            push("project", &scope.id, &scope.tags);
        }
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

/// One line of the listing: the active mark, the name, and the annotation.
fn scope_line(row: &ScopeRow, use_color: bool) -> String {
    let mark = if row.is_active {
        crate::cli::active_marker(use_color)
    } else {
        " ".to_string()
    };
    let annotation = if !row.is_active && row.is_unknown {
        format!(" {}", crate::cli::style::unknown_annotation(use_color))
    } else {
        crate::cli::annotate(row.is_active, row.is_orphan, use_color)
    };
    format!("{mark} {}{annotation}", row.name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn config_with_content(id: &str) -> Config {
        let mut config = Config::default();
        config.scope.content.push(crate::config::ContentScope {
            id: id.to_string(),
            r#match: crate::config::ContentMatch {
                glob: "*.md".to_string(),
                depth: None,
            },
            tags: Vec::new(),
        });
        config
    }

    #[test]
    fn an_unknown_content_scope_shows_unknown_not_inactive() {
        let config = config_with_content("md");
        let active = ActiveScopes {
            unknown_content: BTreeSet::from(["md".to_string()]),
            ..Default::default()
        };
        let rows = scope_rows(&config, &active);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].is_unknown);
        assert_eq!(scope_line(&rows[0], false), "  content:md (unknown)");
    }

    #[test]
    fn a_known_inactive_content_scope_is_not_unknown() {
        let config = config_with_content("md");
        let rows = scope_rows(&config, &ActiveScopes::default());
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].is_unknown);
        assert!(!scope_line(&rows[0], false).contains("(unknown)"));
    }

    #[test]
    fn an_active_content_scope_has_no_unknown_mark() {
        let config = config_with_content("md");
        let active = ActiveScopes {
            scopes: vec![crate::scope::ActiveScope {
                id: "md".to_string(),
                kind: "content",
                ..Default::default()
            }],
            ..Default::default()
        };
        let rows = scope_rows(&config, &active);
        assert!(rows[0].is_active);
        assert!(!scope_line(&rows[0], false).contains("(unknown)"));
    }

    #[test]
    fn no_configured_scope_gives_no_rows() {
        assert!(scope_rows(&Config::default(), &ActiveScopes::default()).is_empty());
    }
}
