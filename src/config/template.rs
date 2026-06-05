/// Generate a YAML config template.
///
/// Returns a static template with inline comments. When `Config` and nested
/// types derive `JsonSchema`, this will walk the schema tree instead. Until
/// then the template and schema must be kept in sync manually.
pub fn generate_template() -> String {
    r#"# llmenv configuration
# See https://phaedrus1992.github.io/llmenv/docs/configuration for the complete schema

cache:
  cache_dir: "~/.cache/llmenv"
  sync_interval_minutes: 60
  # Optional: minutes to retain cached materializations (default: 168 = 7 days)
  # cache_retention_hours: 168
  # Cache-folder strictness dial (default: normal). One knob, three positions,
  # ordered by how aggressively a folder is reused: loose ⊂ normal ⊂ strict.
  #   loose  — folder = <shape>. Selection-addressed only; a binary upgrade
  #            reuses the same folder. Fewest folders.
  #   normal — folder = <major.minor>/<shape>. Config edits re-render into the
  #            SAME folder, so a running agent only picks up changes on its next
  #            launch; a minor version bump or selection change mints a new one.
  #            Preserves in-session agent/plugin state across re-renders.
  #   strict — folder = <version>-<content_hash>; every input change makes a new
  #            folder. Strongest isolation, but fragments the cache.
  # hashing: normal

# Scopes: match environment conditions (network, host, user, project) and emit tags.
# Uncomment and fill in as needed.
# scope:
#   network:
#     - id: office
#       match: { gateway_mac: "aa:bb:cc:dd:ee:ff" }
#       tags: [office]
#     - id: home
#       match: { ssid: "MyHomeWiFi" }
#       tags: [home]
#   host:
#     - id: laptop
#       match: { hostname: "my-laptop" }
#       tags: [laptop]
#   user:
#     - id: me
#       match: { user: "alice" }
#       tags: [me]
#
# Project scopes are NOT declared here. Drop a `.llmenv.yaml` marker file in a
# project directory instead; llmenv discovers it by walking the current
# directory upward to $HOME. Example `.llmenv.yaml`:
#   id: myapp
#   name: MyApp
#   description: "Optional description"
#   tags: [myapp, rust]
#   enable_bundles: [base]   # optional: force-enable bundles regardless of tags

# Bundles: named collections of config that fire on tag match.
# Uncomment and edit as needed.
bundle:
  - name: base
    tags: [me]
    env:
      AGENT: "claude"
  # - name: office-only
  #   tags: [office]
  #   vars:
  #     WORK_MCP: "office-server"

# Capabilities: permissions, hooks, plugins. Merged per-scope.
# capabilities:
#   permissions:
#     allow:
#       - command: /usr/bin/git
#         args: [".*"]

# MCP servers: selected onto scopes by tag intersection, rendered into the agent's
# MCP config. Each is either stdio (command) or remote (url).
# mcp:
#   - name: playwright
#     tags: [me]
#     type: stdio           # optional, default is stdio
#     command: npx
#     args: ["-y", "@playwright/mcp@latest"]
#   # - name: office-internal
#   #   tags: [office]
#   #   type: remote
#   #   url: http://office-mcp.internal:3000

# Plugin marketplaces: git sources or local paths for agent plugins.
# marketplace:
#   - name: github-marketplace
#     url: https://github.com/you/llmenv-plugins.git
#   - name: local-plugins
#     path: ~/.config/llmenv/local-plugins

# Plugin collections: named bags of plugins selected by tag, like bundles.
# plugin-collection:
#   - name: base-plugins
#     tags: [me]
#     plugins:
#       - github-marketplace:useful-plugin
#       - github-marketplace:another-plugin

# Per-engine native config (passthrough). Keys match engine names (e.g. claude_code).
# llmenv does not interpret these — adapters merge them verbatim into the engine's
# native settings. Escape hatch for non-portable keys.
# native:
#   claude_code:
#     customSetting: value

# llmenv's memory backend (ICM). Optional. One host runs the daemon,
# others connect as network clients.
# features:
#   memory:
#     server_host: my-laptop  # must exist in the host: table below
#     port: 7878
#     tags: [me]

# Host directory: maps logical host names to reachable addresses.
# Used by the memory backend topology to build client connection URLs.
# host:
#   my-laptop:
#     addr: "my-laptop.local"
#   office-server:
#     addr: "office-server.internal"

# Durable state relocation: tools that persist state to CLAUDE_CONFIG_DIR
# lose it when llmenv re-materializes (on config edits or version bumps).
# Declare tools here to point them at a stable, hash-independent state directory.
# state:
#   tools:
#     - env: CONTEXT_MODE_DATA_DIR   # tool reads this to find its state
#       subdir: context-mode         # → $LLMENV_STATE_DIR/context-mode
#     - env: MY_TOOL_STATE
#       subdir: my-tool
"#
    .to_string()
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn test_template_is_valid_yaml() {
        let template = generate_template();
        let result: Result<serde_yaml::Value, _> = serde_yaml::from_str(&template);
        assert!(
            result.is_ok(),
            "Template should be valid YAML: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_template_roundtrips_as_config() {
        use crate::config::Config;
        let template = generate_template();
        let cfg: Config =
            serde_yaml::from_str(&template).expect("template must deserialize as Config");
        let re_serialized = serde_yaml::to_string(&cfg).expect("re-serialize");
        let cfg2: Config =
            serde_yaml::from_str(&re_serialized).expect("roundtrip must deserialize");
        assert_eq!(cfg, cfg2, "Config must roundtrip through YAML");
    }

    #[test]
    fn test_template_has_expected_sections() {
        let template = generate_template();
        assert!(template.contains("# llmenv configuration"));
        assert!(template.contains("cache:"));
        assert!(template.contains("# Scopes:"));
        assert!(template.contains("# Capabilities:"));
        assert!(template.contains("bundle:"));
    }
}
