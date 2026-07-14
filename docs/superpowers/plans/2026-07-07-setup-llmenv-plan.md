<!-- markdownlint-disable MD013 -->
# Setup llmenv — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rework the `llmenv setup` command to handle mechanical bootstrap (config dir, identity, repo, engine probing, config scanning → enumeration JSON), then optionally hand off to an AI agent via an embedded skill (`claude -p` / `crush run`). The `--no-launch` flag makes the whole flow testable without an agent.

**Architecture:** The CLI command does file I/O and engine detection. An embedded skill markdown file (lives in-repo at `skills/setup-llmenv/SKILL.md`, compiled in via `include_str!()`) is written to the user's bundle and optionally piped to the AI agent for the evaluative part — sorting configs into bundles, suggesting MCP/hooks/plugin-collection mappings.

**Tech Stack:** Rust, `dialoguer`, `serde_json`, `serde_yaml`, `include_str!`, `std::process::Command` for engine detection/handoff.

## Global Constraints

- `scan_existing_configs` must read file *contents*, not just check existence
- `disabled_engines` in config.yaml must be set based on engine probing
- `.llmenv-setup-state.json` is the sole data interface between CLI and skill
- Skill text is embedded via `include_str!("../../skills/setup-llmenv/SKILL.md")`
- Engine handoff uses `claude -p` or `crush run` with `{STATE_PATH}` placeholder substitution
- Cursor is out of scope (no Cursor engine support yet)
- `--no-launch` flag skips the handoff entirely

---

## File Structure

| File | Responsibility | Status |
| ------ | --------------- | -------- |
| `src/cli/mod.rs` | CLI definition — add `--no-launch` flag | Modify |
| `src/cli/setup.rs` | Core logic: scanning, enumeration, engine probing, skill install, handoff, tests | Modify (major) |
| `skills/setup-llmenv/SKILL.md` | The skill text — the AI reads this to know what to do | Create |
| `docs/superpowers/specs/2026-07-07-setup-llmenv-design.md` | Design spec (reference only) | Done |
| `website/docs/changelog.md` | Synced from CHANGELOG.md | May update |

---

### Task 1: Add `--no-launch` flag to CLI enum and dispatch

**Files:**

- Modify: `src/cli/mod.rs` (Setup variant + dispatch lines)

**Interfaces:**

- Consumes: (none — new flag on existing Setup variant)
- Produces: `Command::Setup { path, repo, no_launch }` with new `no_launch: bool` field

**Details:**

The current `Setup` variant has `path` and `repo` fields. Add `no_launch`:

```rust
    /// Interactive setup wizard for new llmenv users
    Setup {
        /// Directory to set up (defaults to the standard config dir)
        path: Option<std::path::PathBuf>,
        /// Repository to store config in (optional)
        #[arg(long)]
        repo: Option<String>,
        /// Skip the AI engine handoff at the end
        #[arg(long)]
        no_launch: bool,
    },
```

Update dispatch to pass `no_launch`:

```rust
    Some(Command::Setup { path, repo, no_launch }) => {
        setup::run_setup(path, repo, no_launch)?;
    }
```

The existing `run_setup(path, repo)` signature in `setup.rs` changes to `run_setup(path, repo, no_launch)`.

- [ ] **Step 1: Add `no_launch` field to Setup variant in `src/cli/mod.rs`**

Edit the Setup variant to add `#[arg(long)] no_launch: bool`.

- [ ] **Step 2: Update dispatch to pass `no_launch` to `run_setup`**

Edit the `Some(Command::Setup { path, repo, no_launch })` dispatch arm.

- [ ] **Step 3: Update `run_setup` signature in `setup.rs`**

Change `pub(super) fn run_setup(path: Option<PathBuf>, repo: Option<String>)` to `pub(super) fn run_setup(path: Option<PathBuf>, repo: Option<String>, no_launch: bool)`.

- [ ] **Step 4: Verify compilation**

```bash
cargo check 2>&1
```

Expected: no errors.

- [ ] **Step 5: Commit**

```bash
git add src/cli/mod.rs src/cli/setup.rs
git commit -m "feat: add --no-launch flag to llmenv setup"
```

---

### Task 2: Engine probing + `disabled_engines` in config

**Files:**

- Modify: `src/cli/setup.rs`

**Interfaces:**

- Produces: `fn probe_engines() -> Vec<String>` (returns engine IDs found on PATH: `"claude_code"`, `"crush"`)
- Produces: `fn compute_disabled_engines(available: &[String]) -> Vec<String>` (ALL_SUPPORTED minus available)
- Consumes: `write_config()` now takes `disabled_engines: &[String]` parameter

**Details:**

`probe_engines()` checks `which claude` and `which crush` (uses `std::process::Command::new("which").arg("claude").output()`). Returns a vector of engine adapter IDs that are available. The full set of supported engines is `["claude_code", "crush"]`. Any not found get added to `disabled_engines`.

Call site in `run_setup`:

```rust
let available = probe_engines();
let disabled = compute_disabled_engines(&available);
// pass &disabled to write_config
```

- [ ] **Step 1: Write failing test for `probe_engines`**

The function checks PATH. It should return known engines that exist. Test by checking that at minimum it returns something (can't mock `which` easily, so test structurally — the function doesn't panic):

```rust
#[test]
fn test_probe_engines_does_not_panic() {
    let engines = probe_engines();
    // engines should be a Vec<String>, any is fine
    for e in &engines {
        assert!(e == "claude_code" || e == "crush", "unexpected engine: {e}");
    }
}

#[test]
fn test_compute_disabled_engines_none_available() {
    let disabled = compute_disabled_engines(&[]);
    assert_eq!(disabled.len(), 2);
    assert!(disabled.contains(&"claude_code".to_string()));
    assert!(disabled.contains(&"crush".to_string()));
}

#[test]
fn test_compute_disabled_engines_all_available() {
    let disabled = compute_disabled_engines(&["claude_code".to_string(), "crush".to_string()]);
    assert!(disabled.is_empty());
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p llmenv -- cli::setup 2>&1
```

Expected: compile error (`probe_engines` not defined) or test failure.

- [ ] **Step 3: Implement `probe_engines` and `compute_disabled_engines`**

```rust
/// Probe PATH for supported engines.
/// Returns adapter IDs of engines found (e.g. "claude_code", "crush").
fn probe_engines() -> Vec<String> {
    let mut found = Vec::new();
    // Map of binary name → engine adapter ID
    let probes: &[(&str, &str)] = &[("claude", "claude_code"), ("crush", "crush")];
    for (binary, engine_id) in probes {
        let output = std::process::Command::new("which")
            .arg(binary)
            .output();
        if let Ok(out) = output {
            if out.status.success() {
                found.push(engine_id.to_string());
            }
        }
    }
    found
}

/// Compute which engines should be disabled given which are available.
/// ALL_SUPPORTED minus available = disabled.
fn compute_disabled_engines(available: &[String]) -> Vec<String> {
    const ALL_SUPPORTED: &[&str] = &["claude_code", "crush"];
    ALL_SUPPORTED
        .iter()
        .filter(|e| !available.contains(&e.to_string()))
        .map(|e| e.to_string())
        .collect()
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p llmenv -- cli::setup 2>&1
```

Expected: all tests pass.

- [ ] **Step 5: Update `write_config` to accept and set `disabled_engines`**

Add parameter to `write_config`:

```rust
fn write_config(
    config_path: &Path,
    bundles: &[String],
    repo: Option<&str>,
    user_name: &str,
    disabled_engines: &[String],
) -> Result<()> {
```

And add near the end, before serialization:

```rust
    config.disabled_engines = disabled_engines.to_vec();
```

Update the call in `run_setup`:

```rust
write_config(&config_path, &bundles, repo_url.as_deref(), &user_name, &disabled)?;
```

Update the existing test `test_write_config_creates_valid_yaml` to pass `&[]` as the new parameter. Update `test_write_config_with_repo` similarly.

- [ ] **Step 6: Verify compilation + tests**

```bash
cargo test -p llmenv -- cli::setup 2>&1
```

- [ ] **Step 7: Commit**

```bash
git add -A && git commit -m "feat: add engine probing and disabled_engines to setup"
```

---

### Task 3: Read config contents and write `.llmenv-setup-state.json`

**Files:**

- Modify: `src/cli/setup.rs`

**Interfaces:**

- Produces: `fn read_claude_settings(home: &Path) -> Option<serde_json::Value>`
- Produces: `fn read_claude_plugins(home: &Path) -> Option<serde_json::Value>`
- Produces: `fn read_project_configs(home: &Path) -> BTreeMap<String, serde_json::Value>`
- Produces: `fn build_enumeration(available: &[String], config_dir: &Path) -> serde_json::Value`
- Produces: `fn write_enumeration_json(config_dir: &Path, enumeration: &serde_json::Value) -> Result<()>`
- Consumes: (called by `run_setup` after scan + probe)

**Details:**

`read_claude_settings` opens `~/.claude/settings.json`, parses as JSON, returns the value or None on error.

`read_claude_plugins` opens `~/.claude/plugins.json`, parses as JSON, returns the value or None.

`read_project_configs` reads `~/.claude/projects/*/settings.json` per subdirectory, returns a map of project name → settings content.

`build_enumeration` assembles the full JSON object:

```json
{
  "version": 1,
  "user": "username",
  "config_dir": "/path/to/config",
  "engines_available": ["claude_code"],
  "existing_configs": {
    "claude_code": {
      "settings": { ... parsed or null },
      "plugins": [ ... parsed or null ],
      "marketplaces": [ ... extracted from settings if present ],
      "claude_md": "string content or null",
      "gemini_md": "string content or null",
      "projects": { "name": { "settings": { ... } } }
    }
  },
  "created_bundles": ["base"]
}
```

- [ ] **Step 1: Write the failing test for enumeration**

```rust
#[test]
fn test_build_enumeration_has_required_fields() {
    let enumeration = build_enumeration(&["claude_code".to_string()], Path::new("/tmp/test"));
    let obj = enumeration.as_object().expect("enumeration must be an object");
    assert!(obj.contains_key("version"));
    assert!(obj.contains_key("engines_available"));
    assert!(obj.contains_key("existing_configs"));
    assert!(obj.contains_key("created_bundles"));
}

#[test]
fn test_read_claude_settings_file_not_found() {
    let dir = tempfile::tempdir().expect("temp dir");
    let result = read_claude_settings(dir.path());
    assert!(result.is_none(), "should be None when no settings file exists");
}

#[test]
fn test_write_enumeration_json_creates_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let enumeration = serde_json::json!({"version": 1});
    write_enumeration_json(dir.path(), &enumeration).expect("write");
    let path = dir.path().join(".llmenv-setup-state.json");
    assert!(path.is_file(), "enumeration file should exist");
    let content = std::fs::read_to_string(&path).expect("read");
    let parsed: serde_json::Value = serde_json::from_str(&content).expect("valid JSON");
    assert_eq!(parsed["version"], 1);
}
```

- [ ] **Step 2: Run tests to see them fail**

```bash
cargo test -p llmenv -- cli::setup 2>&1
```

- [ ] **Step 3: Implement the functions**

```rust
/// Read ~/.claude/settings.json if it exists.
fn read_claude_settings(home: &Path) -> Option<serde_json::Value> {
    let path = home.join(".claude").join("settings.json");
    if !path.is_file() { return None; }
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Read ~/.claude/plugins.json if it exists.
fn read_claude_plugins(home: &Path) -> Option<serde_json::Value> {
    let path = home.join(".claude").join("plugins.json");
    if !path.is_file() { return None; }
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Read ~/.claude/claude.md if it exists.
fn read_claude_md(home: &Path) -> Option<String> {
    let path = home.join(".claude").join("CLAUDE.md");
    if !path.is_file() { return None; }
    std::fs::read_to_string(&path).ok()
}

/// Read ~/.claude/gemini.md if it exists.
fn read_gemini_md(home: &Path) -> Option<String> {
    let path = home.join(".claude").join("GEMINI.md");
    if !path.is_file() { return None; }
    std::fs::read_to_string(&path).ok()
}

/// Read per-project settings from ~/.claude/projects/*/settings.json.
fn read_project_configs(home: &Path) -> BTreeMap<String, serde_json::Value> {
    use std::collections::BTreeMap;
    let mut projects = BTreeMap::new();
    let projects_dir = home.join(".claude").join("projects");
    let Ok(entries) = std::fs::read_dir(&projects_dir) else { return projects; };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let settings_path = entry.path().join("settings.json");
        if let Ok(bytes) = std::fs::read(&settings_path) {
            if let Ok(val) = serde_json::from_slice(&bytes) {
                projects.insert(name, val);
            }
        }
    }
    projects
}

/// Build the full enumeration JSON value.
fn build_enumeration(available: &[String], config_dir: &Path) -> serde_json::Value {
    let home = std::env::var("HOME").map(PathBuf::from).ok();
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());

    let claude_section = home.as_ref().map(|h| {
        let settings = read_claude_settings(h);
        let plugins = read_claude_plugins(h);
        // Extract marketplace refs from settings or plugins if present
        let marketplaces = settings.as_ref()
            .and_then(|s| s.get("marketplaces").cloned())
            .or_else(|| plugins.as_ref().and_then(|p| p.get("marketplaces").cloned()));

        serde_json::json!({
            "settings": settings,
            "plugins": plugins,
            "marketplaces": marketplaces,
            "claude_md": read_claude_md(h),
            "gemini_md": read_gemini_md(h),
            "projects": read_project_configs(h),
        })
    });

    serde_json::json!({
        "version": 1,
        "user": user,
        "config_dir": config_dir.to_string_lossy(),
        "engines_available": available,
        "existing_configs": {
            "claude_code": claude_section
        },
        "created_bundles": ["base"]
    })
}

/// Write the enumeration JSON to {config_dir}/.llmenv-setup-state.json.
fn write_enumeration_json(config_dir: &Path, enumeration: &serde_json::Value) -> Result<()> {
    let path = config_dir.join(".llmenv-setup-state.json");
    let json = serde_json::to_string_pretty(enumeration)
        .context("serializing enumeration JSON")?;
    paths::write_owner_only_atomic(&path, json.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p llmenv -- cli::setup 2>&1
```

- [ ] **Step 5: Integrate into `run_setup`**

After the scan-and-print section, add:

```rust
    let available = probe_engines();
    let disabled = compute_disabled_engines(&available);

    // Build and write enumeration JSON
    let enumeration = build_enumeration(&available, &config_dir);
    write_enumeration_json(&config_dir, &enumeration)?;
    eprintln!("✓ Environment snapshot written to .llmenv-setup-state.json");
```

Then pass `&disabled` to `write_config`.

- [ ] **Step 6: Verify compilation + tests**

```bash
cargo test -p llmenv -- cli::setup 2>&1
```

- [ ] **Step 7: Commit**

```bash
git add -A && git commit -m "feat: read config contents and write enumeration JSON"
```

---

### Task 4: Create the embedded setup skill + install to bundle

**Files:**

- Create: `skills/setup-llmenv/SKILL.md`
- Modify: `src/cli/setup.rs` (embed and install)

**Interfaces:**

- Produces: `const SETUP_SKILL: &str = include_str!(...)`
- Produces: `fn install_setup_skill(config_dir: &Path, bundles: &[String]) -> Result<()>`
- Produces: `fn resolve_skill_path(config_dir: &Path) -> PathBuf`

**Details:**

The skill is a markdown file that guides an AI agent through the setup process. It lives in the repo at `skills/setup-llmenv/SKILL.md` and is embedded into the binary via `include_str!`. The skill uses `{STATE_PATH}` as a placeholder for the enumeration JSON path which gets substituted at handoff time.

- [ ] **Step 1: Write the skill markdown**

Create `skills/setup-llmenv/SKILL.md`:

```markdown
---
name: setup-llmenv
description: >
  Interactive setup wizard for new llmenv users. Evaluates existing Claude Code
  configuration, helps sort into bundles, and writes llmenv-native equivalents.
---

# Setup llmenv

You are an interactive setup wizard for a new llmenv user. Your job is to
evaluate their existing tool configuration and help them create an llmenv
configuration that matches their needs.

## Context

Load the configuration snapshot from the file at `{STATE_PATH}`. This JSON file
contains what `llmenv setup` found:

- `existing_configs.claude_code.settings` — their current Claude Code settings
- `existing_configs.claude_code.plugins` — installed plugins
- `existing_configs.claude_code.marketplaces` — plugin marketplaces
- `existing_configs.claude_code.claude_md` — custom agent instructions
- `existing_configs.claude_code.projects` — per-project overrides
- `engines_available` — which AI engines are installed
- `config_dir` — where the llmenv config lives
- `user` — the user's name
- `created_bundles` — bundles already created by `llmenv setup`

The llmenv config directory is at `{config_dir}`.

## Walkthrough

### 1. Greeting

Greet the user by name. Summarize what the scan found — number of settings
keys, plugins, and projects detected. Give them a sense of what's about to
happen.

### 2. Settings Review

Walk through the settings found in `existing_configs.claude_code.settings`.
For each non-llmenv-owned key (keys not in `init.seeded_settings`):

- Explain what the setting does
- Ask: "Keep this in the `base` bundle? Move to a new bundle? Drop it?"
- If they want to keep it, record it for inclusion in a native passthrough
  section or in the bundle's own settings

### 3. Plugins & Marketplaces

For each plugin in `existing_configs.claude_code.plugins`:

- Show the plugin name and its marketplace source
- Ask: "This plugin came from `{marketplace}` — should I add that marketplace
  to your llmenv config and add the plugin to a plugin-collection?"
- If they say yes, record a marketplace entry and a plugin-collection entry
- If the marketplace is already known (e.g. `dev-commons`, `claude-plugins-official`),
  mention that and just add the plugin reference

For each marketplace in `existing_configs.claude_code.marketplaces`:
- Ask: "Add this marketplace to your llmenv config?"
- If yes, record a marketplace entry

### 4. Custom Instructions

If `claude_md` or `gemini_md` is present:

- Summarize the key directives
- Ask: "These instructions were specific to Claude Code. Should I merge them
  into your `base` bundle's AGENTS.md or create a separate bundle?"
- If merge, add the content to the bundle instructions
- If separate, create a new bundle

### 5. Project Configs

If `projects` is non-empty:

- List each project and how many overrides it has
- Explain: "Per-project overrides are very specific — they work best as
  native passthrough entries in config.yaml"
- Ask: "Keep these project configs or skip them?"

### 6. Bundle Organization

Ask the user about their workflow:

- "Do you have separate work and personal environments?"
- "What programming languages do you primarily use?"
- "Do you use any cloud platforms (AWS, GCP, Azure)?"
- "Do you use any specific tools you want scoped to certain directories?"

Based on their answers, suggest creating additional bundles (e.g. `work`,
`rust-dev`, `aws-tools`, etc.) with appropriate `when:` tags.

For each suggested bundle:
- Ask: "Should I create a `{name}` bundle?"
- If yes, create the bundle directory and add it to config.yaml's bundle list
- Set up the `when:` tag (or just create the skeleton and note the tag)

### 7. Scopes

Ask about scope conditions:

- "What hostname does this machine use? (We'll auto-detect if you're not sure)"
- "What WiFi networks do you switch between (home, office)?"
- "Is this setup for just you, or multiple users?"

Based on answers, add scope entries to config.yaml (host, network, user scopes).

### 8. Configuration Writing

Write out all recorded configuration:

- Update `config.yaml` with new bundles, marketplace entries, plugin-collections,
  scopes, and native passthrough settings
- Write bundle-level `CLAUDE.md`/`AGENTS.md` files for any bundle with custom
  instructions
- Run `llmenv regenerate` and report the result

### 9. Wrap-up

- Summarize everything that was created
- Recommend next steps (edit config.yaml, add project markers, run
  `llmenv status` to verify)
- Point to the docs: https://phaedrus1992.github.io/llmenv/
```

- [ ] **Step 2: Embed the skill in the binary**

Add to `setup.rs`:

```rust
/// The setup skill content, embedded from the source file in the repo.
pub(crate) const SETUP_SKILL_SOURCE: &str = include_str!("../../skills/setup-llmenv/SKILL.md");

/// Write the setup skill to the bundle's skills directory.
fn install_setup_skill(config_dir: &Path) -> Result<PathBuf> {
    let skill_dir = config_dir
        .join("bundles")
        .join("base")
        .join("skills")
        .join("setup-llmenv");
    std::fs::create_dir_all(&skill_dir)
        .with_context(|| format!("creating skill dir {}", skill_dir.display()))?;

    let skill_path = skill_dir.join("SKILL.md");
    // Substitute the {STATE_PATH} placeholder with the actual path
    // (the handoff also does this substitution, but installing it with
    // the correct path means the skill can also be invoked manually)
    let state_path = config_dir.join(".llmenv-setup-state.json");
    let content = SETUP_SKILL_SOURCE.replace("{STATE_PATH}", &state_path.to_string_lossy());
    paths::write_owner_only(&skill_path, content.as_bytes())
        .with_context(|| format!("writing skill {}", skill_path.display()))?;
    Ok(skill_path)
}
```

- [ ] **Step 3: Write failing test for skill install**

```rust
#[test]
fn test_install_setup_skill_creates_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let skill_path = install_setup_skill(dir.path()).expect("install");
    assert!(skill_path.is_file(), "SKILL.md should exist");
    let content = std::fs::read_to_string(&skill_path).expect("read");
    assert!(content.contains("Setup llmenv"), "should contain skill content");
    // The {STATE_PATH} placeholder should be resolved
    assert!(!content.contains("{STATE_PATH}"), "placeholder should be resolved");
    assert!(content.contains(".llmenv-setup-state.json"), "should reference state file");
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p llmenv -- cli::setup 2>&1
```

- [ ] **Step 5: Integrate into `run_setup`**

After writing config.yaml and before the handoff:

```rust
    // Install the setup skill
    install_setup_skill(&config_dir)?;
    eprintln!("✓ Setup skill installed to bundles/base/skills/setup-llmenv/");
```

- [ ] **Step 6: Verify compilation + tests**

```bash
cargo test -p llmenv -- cli::setup 2>&1
```

- [ ] **Step 7: Commit**

```bash
git add skills/setup-llmenv/SKILL.md src/cli/setup.rs
git commit -m "feat: embed and install setup-llmenv skill"
```

---

### Task 5: Engine handoff (the `--no-launch` path)

**Files:**

- Modify: `src/cli/setup.rs`

**Interfaces:**

- Produces: `fn engine_handoff_prompt(skill_path: &Path, state_path: &Path, available: &[String], no_launch: bool) -> Result<()>`
- Consumes: Called at end of `run_setup` when `no_launch` is false

**Details:**

`engine_handoff_prompt` checks if `no_launch` is true → skip. Otherwise, if there's at least one engine available:

1. Build a list of choices: each available engine (display name) + "Skip"
2. Prompt with `dialoguer::Select`
3. On selection, pipe the skill + state path to the engine

For piping: read the installed skill file (with `{STATE_PATH}` already resolved from install step), and use `std::process::Command` to invoke:

```rust
let engine_cmd = match chosen_engine {
    "claude_code" => "claude",
    "crush" => "crush",
    _ => unreachable!(),
};
let args = match chosen_engine {
    "claude_code" => &["-p", &skill_content] as &[&str],
    "crush" => &["run", "-p", &skill_content] as &[&str],
    _ => unreachable!(),
};
let status = std::process::Command::new(engine_cmd)
    .args(args)
    .stdin(std::process::Stdio::inherit())
    .stdout(std::process::Stdio::inherit())
    .stderr(std::process::Stdio::inherit())
    .status()
    .context("launching engine")?;
```

If no engines available and handoff not skipped: print a message saying no engines found, suggest installing one.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn test_engine_handoff_no_launch_skips() {
    // With no_launch=true, should return Ok without prompting
    let result = engine_handoff_prompt(
        Path::new("/tmp/skill.md"),
        Path::new("/tmp/state.json"),
        &["claude_code".to_string()],
        true, // no_launch
    );
    assert!(result.is_ok(), "no_launch should skip without error");
}

#[test]
fn test_engine_handoff_no_engines_prints_message() {
    // With no engines available and no_launch=false, should print a message
    // and return Ok (can't test the eprint easily, but shouldn't error)
    let result = engine_handoff_prompt(
        Path::new("/tmp/skill.md"),
        Path::new("/tmp/state.json"),
        &[], // no engines
        false,
    );
    assert!(result.is_ok(), "no engines should not be an error");
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p llmenv -- cli::setup 2>&1
```

- [ ] **Step 3: Implement `engine_handoff_prompt`**

```rust
/// Present the engine handoff prompt and optionally launch the AI agent.
fn engine_handoff_prompt(
    _skill_path: &Path,
    _state_path: &Path,
    available: &[String],
    no_launch: bool,
) -> Result<()> {
    if no_launch {
        return Ok(());
    }

    if available.is_empty() {
        eprintln!();
        eprintln!("No AI engines found on PATH. Install Claude Code or Crush to");
        eprintln!("run the interactive setup skill. The skill file is already");
        eprintln!("installed — invoke it manually when ready.");
        return Ok(());
    }

    use dialoguer::Select;
    let mut choices: Vec<String> = available
        .iter()
        .map(|e| match e.as_str() {
            "claude_code" => "Claude Code".to_string(),
            "crush" => "Crush".to_string(),
            other => other.to_string(),
        })
        .collect();
    choices.push("Skip (I'll run the skill later)".to_string());

    let selection = Select::new()
        .with_prompt("Launch the interactive setup skill?")
        .items(&choices)
        .default(0)
        .interact()
        .context("handoff prompt failed")?;

    if selection >= available.len() {
        // User chose "Skip"
        eprintln!("You can run the skill later by invoking `setup-llmenv` in your AI agent.");
        return Ok(());
    }

    let engine_id = &available[selection];
    let skill_content = std::fs::read_to_string(_skill_path)
        .context("reading skill file")?;
    // {STATE_PATH} should already be resolved from the install step,
    // but resolve again just in case
    let state_path_str = _state_path.to_string_lossy();
    let skill_content = skill_content.replace("{STATE_PATH}", &state_path_str);

    let (cmd, args): (&str, Vec<&str>) = match engine_id.as_str() {
        "claude_code" => ("claude", vec!["-p", &skill_content]),
        "crush" => ("crush", vec!["run", "-p", &skill_content]),
        other => anyhow::bail!("unsupported engine: {other}"),
    };

    eprintln!();
    eprintln!("🚀 Launching {engine_id} with the setup skill...");
    eprintln!("(The AI will guide you through the rest of the setup.)");
    eprintln!();

    let status = std::process::Command::new(cmd)
        .args(&args)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .with_context(|| format!("launching {cmd}"))?;

    if !status.success() {
        anyhow::bail!("{cmd} exited with status {status}");
    }

    eprintln!("✓ Setup complete!");
    Ok(())
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p llmenv -- cli::setup 2>&1
```

- [ ] **Step 5: Integrate into `run_setup` at the very end**

```rust
    // --- Phase 7: Engine handoff ---
    let skill_path = config_dir.join("bundles").join("base").join("skills")
        .join("setup-llmenv").join("SKILL.md");
    let state_path = config_dir.join(".llmenv-setup-state.json");
    engine_handoff_prompt(&skill_path, &state_path, &available, no_launch)?;
```

- [ ] **Step 6: Verify compilation + all tests**

```bash
cargo test -p llmenv -- cli::setup 2>&1
cargo clippy --all-targets --all-features -- -D warnings 2>&1
```

- [ ] **Step 7: Commit**

```bash
git add -A && git commit -m "feat: add engine handoff with --no-launch support"
```

---

### Task 6: Comprehensive smoke tests for the `--no-launch` path

**Files:**

- Modify: `src/cli/setup.rs` (test module)

**Details:**

All tests run in `tempfile::TempDir`, use `--no-launch` semantics (pass `no_launch: true` to `run_setup`), and mock Claude Code settings as temp files.

- [ ] **Step 1: Fresh setup smoke test**

Tests that a basic `--no-launch` run creates the expected files:

```rust
#[test]
fn test_setup_no_launch_creates_files() {
    let dir = tempfile::tempdir().expect("temp dir");
    // Run setup with no_launch=true in a temp dir
    let result = run_setup(Some(dir.path().to_path_buf()), None, true);
    assert!(result.is_ok(), "setup should succeed: {:?}", result.err());

    // Verify expected files exist
    let config_yaml = dir.path().join("config.yaml");
    assert!(config_yaml.is_file(), "config.yaml should exist");

    let agents_md = dir.path().join("AGENTS.md");
    assert!(agents_md.is_file(), "AGENTS.md should exist");

    let state_json = dir.path().join(".llmenv-setup-state.json");
    assert!(state_json.is_file(), ".llmenv-setup-state.json should exist");

    let skill = dir.path().join("bundles/base/skills/setup-llmenv/SKILL.md");
    assert!(skill.is_file(), "SKILL.md should exist");
}

#[test]
fn test_setup_no_launch_enumeration_is_valid() {
    let dir = tempfile::tempdir().expect("temp dir");
    let result = run_setup(Some(dir.path().to_path_buf()), None, true);
    assert!(result.is_ok());

    let state_path = dir.path().join(".llmenv-setup-state.json");
    let content = std::fs::read_to_string(&state_path).expect("read state");
    let parsed: serde_json::Value = serde_json::from_str(&content).expect("valid JSON");
    assert_eq!(parsed["version"], 1);
    assert!(parsed["config_dir"].as_str().unwrap_or("").contains(dir.path().to_str().unwrap()));
}
```

- [ ] **Step 2: Setup with `--repo` flag smoke test**

```rust
#[test]
fn test_setup_with_repo_flag() {
    let dir = tempfile::tempdir().expect("temp dir");
    let result = run_setup(
        Some(dir.path().to_path_buf()),
        Some("https://github.com/user/config".to_string()),
        true,
    );
    assert!(result.is_ok(), "setup with repo should succeed");

    let config = Config::load(&dir.path().join("config.yaml")).expect("load config");
    assert!(
        !config.marketplace.is_empty(),
        "marketplace should be set when repo is given"
    );
}
```

- [ ] **Step 3: Setup with existing Claude Code settings**

```rust
#[test]
fn test_setup_detects_existing_claude_settings() {
    let dir = tempfile::tempdir().expect("temp dir");

    // Create mock ~/.claude/settings.json
    let home = std::env::var("HOME").expect("HOME set");
    let claude_dir = PathBuf::from(&home).join(".claude");
    let settings_path = claude_dir.join("settings.json");
    let had_settings = settings_path.is_file();

    // Write a test settings file
    let test_settings = serde_json::json!({
        "permissions": { "allow": [{"command": "git"}] },
        "customKey": "customValue"
    });
    std::fs::create_dir_all(&claude_dir).expect("create claude dir");
    std::fs::write(&settings_path, test_settings.to_string()).expect("write settings");

    let result = run_setup(Some(dir.path().to_path_buf()), None, true);
    assert!(result.is_ok());

    // Verify enumeration includes what we wrote
    let state_path = dir.path().join(".llmenv-setup-state.json");
    let content = std::fs::read_to_string(&state_path).expect("read");
    let parsed: serde_json::Value = serde_json::from_str(&content).expect("parse");
    let cc = &parsed["existing_configs"]["claude_code"];
    assert_eq!(cc["settings"]["customKey"], "customValue");

    // Cleanup: restore original state (or remove what we created)
    if !had_settings {
        let _ = std::fs::remove_file(&settings_path);
    }
}
```

- [ ] **Step 4: Setup with no existing configs**

```rust
#[test]
fn test_setup_no_existing_configs() {
    let dir = tempfile::tempdir().expect("temp dir");
    let result = run_setup(Some(dir.path().to_path_buf()), None, true);
    assert!(result.is_ok());

    let state_path = dir.path().join(".llmenv-setup-state.json");
    let content = std::fs::read_to_string(&state_path).expect("read");
    let parsed: serde_json::Value = serde_json::from_str(&content).expect("parse");
    let cc = &parsed["existing_configs"]["claude_code"];
    assert!(cc["settings"].is_null(), "no settings should be null");
}
```

- [ ] **Step 5: Engine probing test**

```rust
#[test]
fn test_compute_disabled_engines_partial() {
    let disabled = compute_disabled_engines(&["claude_code".to_string()]);
    assert_eq!(disabled.len(), 1);
    assert_eq!(disabled[0], "crush");
}

#[test]
fn test_disabled_engines_in_config() {
    let dir = tempfile::tempdir().expect("temp dir");
    // Probe engines, then write config with disabled
    let available = probe_engines();
    let disabled = compute_disabled_engines(&available);
    let bundles = vec!["base".to_string()];
    let config_path = dir.path().join("config.yaml");
    write_config(&config_path, &bundles, None, "testuser", &disabled).expect("write");

    let config = Config::load(&config_path).expect("load");
    // disabled_engines should match what probe says
    for e in &disabled {
        assert!(config.disabled_engines.contains(e), "should disable {e}");
    }
}
```

- [ ] **Step 6: Install skill smoke test**

```rust
#[test]
fn test_installed_skill_is_valid_markdown() {
    let dir = tempfile::tempdir().expect("temp dir");
    let skill_path = install_setup_skill(dir.path()).expect("install");
    let content = std::fs::read_to_string(&skill_path).expect("read");

    // Verify it has frontmatter
    assert!(content.starts_with("---"), "skill should have frontmatter");

    // Verify it mentions key walkthrough sections
    assert!(content.contains("### 1. Greeting") || content.contains("Greeting"));
    assert!(content.contains("Settings Review") || content.contains("settings"));
    assert!(content.contains("Bundle Organization") || content.contains("bundle"));
}
```

- [ ] **Step 7: Run all tests and clippy**

```bash
cargo test -p llmenv -- cli::setup 2>&1
cargo clippy --all-targets --all-features -- -D warnings 2>&1
```

- [ ] **Step 8: Commit**

```bash
git add -A && git commit -m "test: add comprehensive --no-launch smoke tests"
```

---

### Task 7: Remove Cursor from `scan_existing_configs`

**Files:**

- Modify: `src/cli/setup.rs`

**Details:**

The current `scan_existing_configs` has a Cursor check at the bottom. Per the spec, Cursor is out of scope. Remove that block and the `DetectedConfig` struct if no longer needed (the struct was used for the print-display only; now that we read full contents, the display path is separate).

Actually, the `scan_existing_configs` function becomes purely for the display-printout (what we show the user during setup). The actual content reading happens in `read_claude_settings` etc. So `scan_existing_configs` can be simplified to just print what phases we'll scan.

Simplify: remove `DetectedConfig` struct, remove `scan_existing_configs` function, and inline the print statements into `run_setup`.

- [ ] **Step 1: Remove `DetectedConfig` struct and `scan_existing_configs` function**

Delete the struct and function. Replace the call site in `run_setup` with direct `eprintln!` statements.

- [ ] **Step 2: Inline the display**

```rust
    // --- Phase 1: Scan existing configs ---
    eprintln!();
    eprintln!("🔍 Scanning for existing tool configurations...");
    let home = std::env::var("HOME").ok().map(PathBuf::from);
    if let Some(ref h) = home {
        let claude_settings = h.join(".claude").join("settings.json");
        if claude_settings.is_file() {
            eprintln!("  ✓ Found Claude Code settings");
        }
        let plugins = h.join(".claude").join("plugins.json");
        if plugins.is_file() {
            eprintln!("  ✓ Found Claude Code plugins");
        }
        let projects_dir = h.join(".claude").join("projects");
        if projects_dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&projects_dir) {
                let count = entries.flatten().count();
                if count > 0 {
                    eprintln!("  ✓ Found {count} Claude Code project config(s)");
                }
            }
        }
    }
```

- [ ] **Step 3: Update tests**

Remove the `test_scan_existing_configs_does_not_panic` test (no longer needed — the display is trivial). Keep all other tests.

- [ ] **Step 4: Verify compilation**

```bash
cargo test -p llmenv -- cli::setup 2>&1
```

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "refactor: remove Cursor scope, simplify scan display"
```

---

### Task 8: Final CI verification

- [ ] **Step 1: Push all changes**

```bash
git push
```

- [ ] **Step 2: Watch CI**

```bash
bash "/Users/ranger/.cache/llmenv/marketplaces/dev-commons/claude-plugins/nbl-dev/skills/pre-pr-review/scripts/ci-watch.sh"
```

Expected: all checks green. If coverage fails due to `--fail-under-lines 64`, check the coverage report and add missing tests. If docs_sync fails, sync the changelog.

- [ ] **Step 3: Update PR body**

```bash
gh pr edit 575 --title "feat: llmenv setup with enumeration, skill, and engine handoff" \
  --body "<!-- describe all changes -->"
```

- [ ] **Step 4: Move issue to In Review**

```bash
gh issue comment 561 --body "Updated implementation in PR #575 with enumeration, embedded skill, engine handoff, and --no-launch testability."
```
