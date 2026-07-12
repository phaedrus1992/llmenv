use crate::config::{Bundle, Config};
use crate::paths;
use anyhow::Context;
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum StatusSection {
    Scopes,
    Tags,
    Bundles,
    Mcps,
    Plugins,
    Marketplaces,
    ReadOnce,
    All,
}

pub fn run_status(section: Option<StatusSection>, use_color: bool) -> anyhow::Result<()> {
    match section {
        Some(StatusSection::Scopes) => run_scope_ls(use_color),
        Some(StatusSection::Tags) => run_tag_ls(use_color),
        Some(StatusSection::Bundles) => run_bundle_ls(use_color),
        Some(StatusSection::Mcps) => run_mcp_ls(use_color),
        Some(StatusSection::Plugins) => run_plugin_ls(use_color),
        Some(StatusSection::Marketplaces) => run_marketplace_ls(use_color),
        Some(StatusSection::ReadOnce) => run_read_once_status(use_color),
        Some(StatusSection::All) => {
            run_scope_ls(use_color)?;
            run_tag_ls(use_color)?;
            run_bundle_ls(use_color)?;
            run_mcp_ls(use_color)?;
            run_plugin_ls(use_color)?;
            run_marketplace_ls(use_color)?;
            run_read_once_status(use_color)
        }
        None => run_status_overview(use_color),
    }
}

fn run_status_overview(use_color: bool) -> anyhow::Result<()> {
    let config_path = paths::config_path()?;
    let config = Config::load(&config_path)?;
    eprintln!(
        "{} Configuration loaded from {}",
        super::doctor_pass(use_color),
        config_path.display()
    );
    eprintln!("  Scopes:");
    eprintln!("    Network: {}", config.scope.network.len());
    eprintln!("    Host: {}", config.scope.host.len());
    eprintln!("    User: {}", config.scope.user.len());
    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);
    if let Some(proj) = active.scopes.iter().find(|s| s.kind == "project") {
        let label = proj.name.as_deref().unwrap_or(&proj.id);
        if let Some(desc) = &proj.description {
            eprintln!("    Project: {label} — {desc}");
        } else {
            eprintln!("    Project: {label}");
        }
    } else {
        eprintln!("    Project: (none)");
    }
    eprintln!("  Bundles: {}", config.bundle.len());
    Ok(())
}

fn run_scope_ls(use_color: bool) -> anyhow::Result<()> {
    let config_path = paths::config_path()?;
    let config = Config::load(&config_path)?;
    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);
    let consumed = super::all_consumed_tags(&config);

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
    for (name, is_active, is_orphan) in rows {
        let mark = if is_active {
            super::active_marker(use_color)
        } else {
            " ".to_string()
        };
        println!(
            "{} {}{}",
            mark,
            name,
            super::annotate(is_active, is_orphan, use_color)
        );
    }
    Ok(())
}

fn run_tag_ls(use_color: bool) -> anyhow::Result<()> {
    let config_path = paths::config_path()?;
    let config = Config::load(&config_path)?;
    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);

    let emitted = super::all_emitted_tags(&config);
    let consumed = super::all_consumed_tags(&config);

    let mut universe: HashSet<String> = HashSet::new();
    universe.extend(emitted.iter().cloned());
    universe.extend(consumed.iter().cloned());
    universe.extend(active.tags.iter().cloned());

    let mut tags: Vec<String> = universe.into_iter().collect();
    tags.sort();
    if tags.is_empty() {
        println!("(none configured)");
        return Ok(());
    }
    for tag in tags {
        let is_active = active.tags.contains(&tag);
        let emitted_anywhere = emitted.contains(&tag) || active.tags.contains(&tag);
        let consumed_anywhere = consumed.contains(&tag);
        let is_orphan = !(emitted_anywhere && consumed_anywhere);
        let mark = if is_active {
            super::active_marker(use_color)
        } else {
            " ".to_string()
        };
        println!(
            "{} {}{}",
            mark,
            tag,
            super::annotate(is_active, is_orphan, use_color)
        );
    }
    Ok(())
}

fn run_bundle_ls(use_color: bool) -> anyhow::Result<()> {
    let config_path = paths::config_path()?;
    let config = Config::load(&config_path)?;
    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);

    let mut emitted = super::all_emitted_tags(&config);
    emitted.extend(active.tags.iter().cloned());
    let marker_enabled = super::marker_enabled_bundle_names(&active);

    let firing_names: HashSet<&str> = config
        .bundle
        .iter()
        .filter(|b| {
            b.when.iter().any(|t| active.tags.contains(t))
                || marker_enabled.contains(b.name.as_str())
        })
        .map(|b| b.name.as_str())
        .collect();

    let mut rows: Vec<(String, bool, bool)> = config
        .bundle
        .iter()
        .map(|b| {
            let is_active = firing_names.contains(b.name.as_str());
            let has_emitted_tag = b.when.iter().any(|t| emitted.contains(t));
            let is_orphan = !has_emitted_tag && !marker_enabled.contains(&b.name);
            (b.name.clone(), is_active, is_orphan)
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    if rows.is_empty() {
        println!("(none configured)");
        return Ok(());
    }
    for (name, is_active, is_orphan) in rows {
        let mark = if is_active {
            super::active_marker(use_color)
        } else {
            " ".to_string()
        };
        println!(
            "{} {}{}",
            mark,
            name,
            super::annotate(is_active, is_orphan, use_color)
        );
    }
    Ok(())
}

fn run_mcp_ls(use_color: bool) -> anyhow::Result<()> {
    use crate::mcp::resolve::MEMORY_MCP_NAME;
    use crate::mcp::resolve::{ResolvedKind, resolve_bundle_mcps, resolve_mcps};

    let config_path = paths::config_path()?;
    let config_dir = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent"))?;
    let config = Config::load(&config_path)?;
    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);
    let mut emitted = super::all_emitted_tags(&config);
    emitted.extend(active.tags.iter().cloned());

    let manually_enabled: std::collections::HashSet<&str> = active
        .scopes
        .iter()
        .flat_map(|s| s.enable_bundles.iter().map(String::as_str))
        .collect();
    let firing: Vec<&Bundle> = config
        .bundle
        .iter()
        .filter(|b| {
            b.when.iter().any(|bt| active.tags.contains(bt))
                || manually_enabled.contains(b.name.as_str())
        })
        .collect();
    let bundle_refs = super::build_bundle_refs(config_dir, &active, &firing);
    let bundle_caps = if !bundle_refs.is_empty() {
        crate::merge::merge(&config.capabilities, &config.native, &bundle_refs)
            .context("merging bundles for mcp-ls")?
            .capabilities
    } else {
        crate::config::Capabilities::default()
    };

    let top_memory_ls = config
        .features
        .as_ref()
        .map(|f| f.memory.as_slice())
        .unwrap_or_default();
    let bundle_memory_ls = bundle_caps
        .features
        .as_ref()
        .map(|f| f.memory.as_slice())
        .unwrap_or_default();
    let mut all_memory_ls: Vec<crate::config::Memory> = top_memory_ls
        .iter()
        .chain(bundle_memory_ls.iter())
        .cloned()
        .collect();
    crate::util::dedup(&mut all_memory_ls);
    let mut all_host_ls = bundle_caps.host.clone();
    for (k, v) in &config.host {
        all_host_ls.insert(k.clone(), v.clone());
    }

    let mut all_resolved: std::collections::HashMap<String, ResolvedKind> =
        resolve_mcps(&config.mcp, &all_memory_ls, &all_host_ls, &active.tags)
            .context("resolving MCP servers for listing")?
            .into_iter()
            .map(|m| (m.name, m.kind))
            .collect();

    let bundle_mcp_entries = bundle_caps.mcp;
    let bundle_resolved = resolve_bundle_mcps(&bundle_mcp_entries, &active.tags)
        .context("resolving bundle MCP servers for listing")?;
    for m in bundle_resolved {
        all_resolved.entry(m.name).or_insert(m.kind);
    }

    let mut rows: Vec<(String, bool, bool, String)> = config
        .mcp
        .iter()
        .map(|m| {
            let is_active = m.when.is_empty() || m.when.iter().any(|t| active.tags.contains(t));
            let is_orphan = !m.when.is_empty() && !m.when.iter().any(|t| emitted.contains(t));
            let detail = mcp_kind_detail(
                &m.name,
                &format!("{:?}", m.transport).to_lowercase(),
                &all_resolved,
            );
            (m.name.clone(), is_active, is_orphan, detail)
        })
        .collect();

    for m in &bundle_mcp_entries {
        let is_active = m.when.is_empty() || m.when.iter().any(|t| active.tags.contains(t));
        let is_orphan = !m.when.is_empty() && !m.when.iter().any(|t| emitted.contains(t));
        let detail = format!(
            "{} (bundle)",
            mcp_kind_detail(&m.name, "stdio server", &all_resolved)
        );
        rows.push((m.name.clone(), is_active, is_orphan, detail));
    }

    for mem in &all_memory_ls {
        let is_active = mem.when.is_empty() || mem.when.iter().any(|t| active.tags.contains(t));
        let is_orphan = !mem.when.is_empty() && !mem.when.iter().any(|t| emitted.contains(t));
        let detail = mcp_kind_detail(MEMORY_MCP_NAME, "memory", &all_resolved);
        let name = format!("{} ({})", MEMORY_MCP_NAME, mem.server_host);
        rows.push((name, is_active, is_orphan, detail));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    print_detail_rows(&rows, use_color);
    Ok(())
}

fn mcp_kind_detail(
    name: &str,
    fallback: &str,
    resolved: &std::collections::HashMap<String, crate::mcp::resolve::ResolvedKind>,
) -> String {
    use crate::mcp::resolve::ResolvedKind;
    match resolved.get(name) {
        Some(ResolvedKind::Stdio { .. }) => "stdio server".to_string(),
        Some(ResolvedKind::Remote { transport, .. }) => {
            format!("{} client", format!("{transport:?}").to_lowercase())
        }
        None => fallback.to_string(),
    }
}

fn print_detail_rows(rows: &[(String, bool, bool, String)], use_color: bool) {
    if rows.is_empty() {
        println!("(none configured)");
        return;
    }
    for (name, is_active, is_orphan, detail) in rows {
        let mark = if *is_active {
            super::active_marker(use_color)
        } else {
            " ".to_string()
        };
        println!(
            "{} {} ({}){}",
            mark,
            name,
            detail,
            super::annotate(*is_active, *is_orphan, use_color)
        );
    }
}

fn run_marketplace_ls(use_color: bool) -> anyhow::Result<()> {
    use crate::config::split_plugin_ref;

    let config_path = paths::config_path()?;
    let config = Config::load(&config_path)?;
    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);
    let mut emitted = super::all_emitted_tags(&config);
    emitted.extend(active.tags.iter().cloned());

    let active_refs: std::collections::HashSet<&str> = config
        .plugin_collection
        .iter()
        .filter(|c| c.when.iter().any(|t| active.tags.contains(t)))
        .flat_map(|c| c.plugins.iter())
        .filter_map(|p| split_plugin_ref(p).map(|(m, _)| m))
        .collect();
    let referenceable: std::collections::HashSet<&str> = config
        .plugin_collection
        .iter()
        .filter(|c| c.when.iter().any(|t| emitted.contains(t)))
        .flat_map(|c| c.plugins.iter())
        .filter_map(|p| split_plugin_ref(p).map(|(m, _)| m))
        .collect();

    let mut rows: Vec<(String, bool, bool, String)> = config
        .marketplace
        .iter()
        .map(|m| {
            let is_active = active_refs.contains(m.name.as_str());
            let is_orphan = !referenceable.contains(m.name.as_str());
            let kind = match m.classify_source() {
                crate::config::MarketplaceSource::Git => "git",
                crate::config::MarketplaceSource::Path => "path",
            };
            (m.name.clone(), is_active, is_orphan, kind.to_string())
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    print_detail_rows(&rows, use_color);
    Ok(())
}

fn run_plugin_ls(use_color: bool) -> anyhow::Result<()> {
    use crate::config::split_plugin_ref;

    let config_path = paths::config_path()?;
    let config = Config::load(&config_path)?;
    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);
    let mut emitted = super::all_emitted_tags(&config);
    emitted.extend(active.tags.iter().cloned());

    let mut rows: Vec<(String, bool, bool, String)> = Vec::new();
    for collection in &config.plugin_collection {
        let is_active = collection.when.iter().any(|t| active.tags.contains(t));
        let is_orphan = !collection.when.iter().any(|t| emitted.contains(t));
        for plugin in &collection.plugins {
            let display = split_plugin_ref(plugin)
                .map_or_else(|| plugin.clone(), |(m, p)| format!("{p}@{m}"));
            rows.push((display, is_active, is_orphan, collection.name.clone()));
        }
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    if rows.is_empty() {
        println!("(none configured)");
        return Ok(());
    }
    for (name, is_active, is_orphan, collection) in rows {
        let mark = if is_active {
            super::active_marker(use_color)
        } else {
            " ".to_string()
        };
        println!(
            "{} {} (from {}){}",
            mark,
            name,
            collection,
            super::annotate(is_active, is_orphan, use_color)
        );
    }
    Ok(())
}

fn run_read_once_status(use_color: bool) -> anyhow::Result<()> {
    let state_dir = paths::state_dir()?;
    let ro_dir = crate::hook_run::read_once::read_once_state_dir(&state_dir);
    let _ = use_color;
    if ro_dir.exists() {
        let count = std::fs::read_dir(&ro_dir)
            .map(|e| {
                e.flatten()
                    .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
                    .count()
            })
            .unwrap_or(0);
        if count > 0 {
            println!("  ReadOnce: {count} cached session(s)");
        } else {
            println!("  ReadOnce: (empty)");
        }
    } else {
        println!("  ReadOnce: (none)");
    }
    Ok(())
}
