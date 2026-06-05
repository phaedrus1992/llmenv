use crate::config::Config;
use schemars::JsonSchema;
use serde_json::json;

/// Generate a YAML config template with doc comments from schema.
///
/// This pulls `description` from JsonSchema (derived from `///` doc comments)
/// and formats it as YAML comments.
pub fn generate_template() -> String {
    // For now, return a static template that matches current structure.
    // Once Config and all nested types derive JsonSchema, this will walk the
    // schema tree and emit annotated YAML automatically.
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
#       match: {}
#       tags: [home]
#   host:
#     - id: server
#       match: { hostname: server.example.com }
#       tags: [remote]
#   user:
#     - id: personal
#       match: { user: alice }
#       tags: [personal]
#   user_tag:
#     - id: dev_alice
#       match: { user: alice, tags: [work] }
#       tags: [work-dev]

# Capabilities: permissions, hooks, plugins. Merged per-scope.
# capabilities:
#   permissions:
#     allow:
#       - command: /usr/bin/git
#         args: [".*"]

# MCP servers: selected by tag intersection (same model as bundles)
# mcp:
#   - name: fetch
#     transport: stdio
#     command: uvx
#     args: ["mcp-server-fetch"]

# Feature toggles (experimental)
# features:
#   memory:
#     daemon_host: localhost
#     daemon_port: 5000
#     client_hosts:
#       - host1.example.com
#       - host2.example.com

# Agent plugin marketplaces
# marketplace:
#   - name: official
#     url: "https://github.com/anthropics/claude-plugins"

# Plugin collections
# plugin-collection:
#   - name: default
#     plugins: []
#     tags: [default]

# State relocation: per-tool env vars pointing to stable, hash-independent dirs
# state:
#   llmenv_state_dir: LLMENV_STATE_DIR

# Host directory mapping (for memory topology)
# host:
#   localhost:
#     address: "127.0.0.1:5000"
#   server:
#     address: "server.example.com:5000"
"#
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_template_generation_basic() {
        let template = generate_template();
        assert!(template.contains("cache:"));
        assert!(template.contains("cache_dir:"));
        assert!(template.contains("sync_interval_minutes:"));
    }

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
    fn test_template_includes_sections() {
        let template = generate_template();
        assert!(template.contains("# llmenv configuration"));
        assert!(template.contains("cache:"));
        assert!(template.contains("# Scopes:"));
        assert!(template.contains("# Capabilities:"));
    }
}
