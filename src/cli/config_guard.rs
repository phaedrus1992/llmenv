//! `llmenv config-guard`: warn when a tool call edits a path inside the llmenv cache (#289).

use std::path::PathBuf;

use crate::paths;

/// The variable that holds the engine's config folder, and the adapter folder name in the cache
/// path. opencode does not set `CLAUDE_CONFIG_DIR`, so its guard reads `OPENCODE_CONFIG_DIR`
/// (#2841).
fn engine_config_var(engine: &str) -> (&'static str, &'static str) {
    match engine {
        "opencode" => ("OPENCODE_CONFIG_DIR", "opencode"),
        _ => ("CLAUDE_CONFIG_DIR", "claude-code"),
    }
}

/// Read the engine's config folder variable `name` for the config guard. Unset means the default
/// cache root is used. A value that is not Unicode is an error, because the guard would
/// otherwise read a different cache from the one the engine started from (#2559).
fn config_dir_from_var(
    name: &str,
    value: Result<String, std::env::VarError>,
) -> Result<Option<String>, String> {
    match value {
        Ok(dir) => Ok(Some(dir)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(format!(
            "{name} is not valid UTF-8, so the guard did not run. Export the directory \
             again as a plain path."
        )),
    }
}

/// `llmenv config-guard`: warn on PreToolUse Write/Edit to managed cache paths (#289).
///
/// Reads the Claude Code hook payload from stdin. If the target path is inside the
/// llmenv cache directory, prints a redirection hint. Always exits 0 (fail-soft).
/// The default cache root, `~/.cache/llmenv`. On failure it prints that the guard is skipped and
/// returns `None`. Stdout reaches the agent from a hook. Stderr from an exit-0 hook does not.
fn config_guard_default_cache() -> Option<PathBuf> {
    match paths::expand_tilde("~/.cache/llmenv") {
        Ok(dir) => Some(PathBuf::from(dir)),
        Err(e) => {
            println!("\u{26a0} llmenv config-guard skipped: {e:#}");
            None
        }
    }
}

pub(super) fn run(engine: &str) {
    use std::io::Read;

    let mut stdin_buf = String::new();
    // Fix C: surface stdin read failures via stderr instead of silently discarding
    // them — the guard becomes a no-op on failure, but the operator can see why.
    if let Err(e) = std::io::stdin().read_to_string(&mut stdin_buf) {
        eprintln!("llmenv config-guard: failed to read stdin: {e}");
        return;
    }

    // Locate the adapter dir (`claude-code`, `opencode`) in the engine's config dir path
    // and take its parent as cache_root. This works for all hashing modes:
    //   Normal  → <root>/claude-code/<version>/<shape>  (3 levels below root)
    //   Loose   → <root>/claude-code/<shape>            (2 levels below root)
    //   Strict  → <root>/claude-code/<VERSION>-<hash>   (2 levels below root)
    // Walking up to find the adapter dir and taking its parent is invariant to depth.
    let (var, adapter_dir) = engine_config_var(engine);
    let cache_root = match config_dir_from_var(var, std::env::var(var)) {
        Ok(None) => {
            // Expected when not running inside a hook.
            let Some(default_cache) = config_guard_default_cache() else {
                return;
            };
            default_cache
        }
        Err(msg) => {
            // Stdout reaches the agent from a hook. Stderr from an exit-0 hook does not.
            println!("\u{26a0} llmenv config-guard: {msg}");
            return;
        }
        Ok(Some(dir)) => {
            let path = PathBuf::from(&dir);
            match path
                .ancestors()
                .skip(1)
                .find(|p| p.file_name().is_some_and(|n| n == adapter_dir))
                .and_then(|p| p.parent().map(PathBuf::from))
            {
                Some(root) => root,
                None => {
                    eprintln!(
                        "llmenv config-guard: could not locate cache root from \
                         {var}={dir}; falling back to default"
                    );
                    let Some(default_cache) = config_guard_default_cache() else {
                        return;
                    };
                    default_cache
                }
            }
        }
    };

    for path_str in extract_hook_file_paths(&stdin_buf) {
        warn_if_cached(&cache_root, &path_str);
    }
}

/// Warn the agent when `path_str` is inside the managed cache.
fn warn_if_cached(cache_root: &std::path::Path, path_str: &str) {
    let expanded = match paths::expand_tilde(path_str) {
        Ok(path) => path,
        Err(e) => {
            // Stdout reaches the agent from a hook. Stderr from an exit-0 hook does not.
            println!("\u{26a0} llmenv config-guard skipped: {e:#}");
            return;
        }
    };
    if crate::util::is_within(cache_root, &expanded) {
        let config_path = paths::config_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "~/.config/llmenv/config.yaml".to_string());
        println!(
            "\u{26a0} llmenv: {path_str} is inside the managed cache and will be overwritten \
             on the next config regeneration.\n\
             Edit your source config instead: {config_path}\n\
             Then run: llmenv regenerate"
        );
    }
}

/// The files that a hook payload writes: `tool_input.file_path`, or each file of an opencode
/// `patch` call, made absolute against the payload's `cwd`.
fn extract_hook_file_paths(stdin_buf: &str) -> Vec<String> {
    if let Some(path) = extract_hook_file_path(stdin_buf) {
        return vec![path];
    }
    let Ok(payload) = serde_json::from_str::<serde_json::Value>(stdin_buf) else {
        return Vec::new();
    };
    if payload["tool_name"] != "patch" {
        return Vec::new();
    }
    let cwd = std::path::Path::new(payload["cwd"].as_str().unwrap_or_default());
    crate::util::patch_paths::patch_paths(
        payload["tool_input"]["patchText"]
            .as_str()
            .unwrap_or_default(),
    )
    .into_iter()
    .map(|p| cwd.join(p).display().to_string())
    .collect()
}

/// Extract `tool_input.file_path` from a raw hook payload JSON string.
///
/// Returns `None` on parse failure (logged to stderr unless the buffer is
/// empty, which is expected outside a hook invocation) or when the field is
/// missing/non-string.
fn extract_hook_file_path(stdin_buf: &str) -> Option<String> {
    match serde_json::from_str::<serde_json::Value>(stdin_buf) {
        Ok(v) => v
            .get("tool_input")
            .and_then(|ti| ti.get("file_path"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        Err(e) => {
            if !stdin_buf.trim().is_empty() {
                eprintln!("llmenv config-guard: failed to parse hook payload: {e}");
            }
            None
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;
