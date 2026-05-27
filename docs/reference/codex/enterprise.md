# Enterprise / managed configuration

On managed machines, an organization can enforce constraints that **override or
cap** what `config.toml` requests, via a separate `requirements.toml`. There's no
Claude Code equivalent in this reference set. This matters for an adapter because
**generated config can be silently rejected or clamped**.

## `requirements.toml`

A managed file (placed by an admin/MDM) that pins or forbids values. Examples:

- Disallow `approval_policy = "never"`.
- Disallow `sandbox_mode = "danger-full-access"`.
- `features.<name>` — require a feature to stay enabled/disabled (e.g.
  `features.browser_use = false`, `features.computer_use = false`,
  `features.in_app_browser = false`).
- `guardian_policy_config` — managed Markdown review policy; **takes precedence**
  over a local `[auto_review].policy`.
- `[experimental_network]` — managed sandboxed-network policy: `socks_port`,
  `unix_sockets` (`map<string, allow | none>`), and a flag that, when true, makes
  only admin-managed allow rules effective (user allowlist additions ignored).

## Gaps vs llmenv

- A `CodexAdapter` must treat its output as a **request, not a guarantee**. If
  the environment ships a `requirements.toml`, generated `approval_policy` /
  `sandbox_mode` / feature flags may be overridden. The adapter shouldn't assume
  its values take effect, and any llmenv-side validation can't fully predict the
  managed outcome.
- llmenv has no concept of an external policy layer constraining its output.
  This is informational for now — there's no action required beyond *not*
  generating values that a managed environment is likely to reject (e.g. avoid
  defaulting to `danger-full-access`).
- If llmenv ever grows org/enterprise scoping, `requirements.toml` is the natural
  Codex target — but that's well beyond current scope.
