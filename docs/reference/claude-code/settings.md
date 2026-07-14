<!-- markdownlint-disable MD013 -->
# settings.json reference

Source: <https://code.claude.com/docs/en/settings> (fetched 2026-05-27).

`settings.json` is Claude Code's primary configuration file. It controls
permissions, hooks, environment variables, model defaults, UI, sandbox, and
enterprise policy. JSON schema for editor validation:
`https://json.schemastore.org/claude-code-settings.json` (add a `$schema` line).

## Settings files and precedence

| Feature | User | Project | Local |
| --- | --- | --- | --- |
| Settings | `~/.claude/settings.json` | `.claude/settings.json` | `.claude/settings.local.json` |
| Subagents | `~/.claude/agents/` | `.claude/agents/` | — |
| MCP servers | `~/.claude.json` | `.mcp.json` | `~/.claude.json` (per-project) |
| Plugins | `~/.claude/settings.json` | `.claude/settings.json` | `.claude/settings.local.json` |
| CLAUDE.md | `~/.claude/CLAUDE.md` | `CLAUDE.md` or `.claude/CLAUDE.md` | `CLAUDE.local.md` |

Precedence (highest first):

1. **Managed settings** — server-managed > MDM/OS policy > file-based
   (`managed-settings.d/*.json` + `managed-settings.json`) > HKCU registry
   (Windows). Locations:
   - macOS: `/Library/Application Support/ClaudeCode/managed-settings.json`
   - Linux/WSL: `/etc/claude-code/managed-settings.json`
   - Windows: `C:\Program Files\ClaudeCode\managed-settings.json`
2. **Command-line args** (`--settings <file-or-json>`) — session overrides
3. **Local project** (`.claude/settings.local.json`)
4. **Project** (`.claude/settings.json`)
5. **User** (`~/.claude/settings.json`)

Merge semantics: scalar values from higher-priority scopes override; **array
values (e.g. `permissions.allow`, `sandbox.filesystem.allowWrite`) concatenate
and deduplicate across scopes**, they do not replace. Settings are hot-reloaded
on edit (a `ConfigChange` hook fires); `model` and `outputStyle` are read once at
session start / `/clear`.

`managed-settings.d/` supports numeric-prefixed drop-in fragments
(`10-telemetry.json`, `20-security.json`) for independent team policies.

Other config lives in `~/.claude.json`: OAuth session, user/local MCP servers,
per-project trust/allowed-tools state, caches. Five timestamped backups retained.

## Available settings (complete key list)

Grouped for readability. ~120 keys; `(M)` marks managed-settings-only.

### Auth & model

| Key | Description |
| --- | --- |
| `apiKeyHelper` | `/bin/sh` script generating an auth value (sent as `X-Api-Key` + `Authorization: Bearer`). TTL via `CLAUDE_CODE_API_KEY_HELPER_TTL_MS`. |
| `awsAuthRefresh` | Script that modifies the `.aws` directory. |
| `awsCredentialExport` | Script outputting JSON AWS credentials. |
| `gcpAuthRefresh` | Script refreshing GCP ADC. |
| `forceLoginMethod` | `claudeai` or `console` — restrict login account type. |
| `forceLoginOrgUUID` | Require login to a specific org (UUID or array). |
| `model` | Default model override (`--model`/`ANTHROPIC_MODEL` override per session). |
| `availableModels` | Restrict models selectable via `/model`, `--model`, `ANTHROPIC_MODEL`. |
| `modelOverrides` | Map Anthropic model IDs → provider IDs (e.g. Bedrock ARNs). |
| `effortLevel` | Persist effort: `low`/`medium`/`high`/`xhigh`. |
| `otelHeadersHelper` | Script generating dynamic OTel headers. |

### Permissions & sandbox

See [permissions.md](./permissions.md) for the full `permissions.*` and
`sandbox.*` sub-trees. Top-level keys: `permissions`, `sandbox`.

### Hooks

| Key | Description |
| --- | --- |
| `hooks` | Lifecycle-event commands. See [hooks.md](./hooks.md). |
| `disableAllHooks` | Disable all hooks **and** any custom status line. |
| `allowManagedHooksOnly` | (M) Only managed/SDK/force-enabled-plugin hooks load. |
| `allowedHttpHookUrls` | Allowlist of URL patterns HTTP hooks may target (`*` wildcard). |
| `httpHookAllowedEnvVars` | Allowlist of env var names HTTP hooks may interpolate into headers. |

### MCP

| Key | Description |
| --- | --- |
| `enableAllProjectMcpServers` | Auto-approve all servers in project `.mcp.json`. |
| `enabledMcpjsonServers` | Specific `.mcp.json` servers to approve. |
| `disabledMcpjsonServers` | Specific `.mcp.json` servers to reject. |
| `allowedMcpServers` | (M) Allowlist of configurable MCP servers (all scopes). |
| `deniedMcpServers` | (M) Denylist (precedence over allowlist). |
| `allowManagedMcpServersOnly` | (M) Only managed `allowedMcpServers` respected. |
| `allowAllClaudeAiMcps` | (M) Load claude.ai connectors alongside `managed-mcp.json`. |

> Note: there is **no `mcp` array key** in `settings.json`. MCP servers are
> defined in `.mcp.json` (project) or the `mcpServers` object of `~/.claude.json`
> (user/local); `settings.json` only governs *which* of them are enabled/denied.

### Memory & instructions

| Key | Description |
| --- | --- |
| `autoMemoryEnabled` | Enable auto memory (default true). |
| `autoMemoryDirectory` | Custom auto-memory storage dir. |
| `claudeMd` | (M) Org-managed CLAUDE.md-style instructions. |
| `claudeMdExcludes` | Glob/abs paths of CLAUDE.md files to skip. |
| `includeGitInstructions` | Include built-in commit/PR workflow + git status in system prompt (default true). |
| `language` | Preferred response/dictation language. |

### UI / output

| Key | Description |
| --- | --- |
| `outputStyle` | Output style name (system-prompt adjustment). See [statusline-and-output-styles.md](./statusline-and-output-styles.md). |
| `statusLine` | Custom status line (`{type:"command", command, padding}`). |
| `attribution` | `{commit, pr}` git/PR attribution strings (empty = hidden). |
| `includeCoAuthoredBy` | **Deprecated** — use `attribution`. |
| `spinnerTipsEnabled` / `spinnerTipsOverride` / `spinnerVerbs` | Spinner customization. |
| `editorMode` | `normal` or `vim`. |
| `tui` | `fullscreen` or `default` renderer. |
| `viewMode` | `default`/`verbose`/`focus`. |
| `showThinkingSummaries`, `showTurnDuration`, `autoScrollEnabled`, `prefersReducedMotion`, `syntaxHighlightingDisabled`, `terminalProgressBarEnabled` | Display toggles. |
| `alwaysThinkingEnabled` | Extended thinking on by default. |
| `preferredNotifChannel` | Notification mechanism (`terminal_bell`, `iterm2`, …). |
| `companyAnnouncements` | Startup announcement strings. |

### Skills

| Key | Description |
| --- | --- |
| `maxSkillDescriptionChars` | Per-skill desc+when_to_use cap (default 1536). |
| `skillListingBudgetFraction` | Context fraction for skill listing (default 0.01). |
| `skillOverrides` | Per-skill visibility: `on`/`name-only`/`user-invocable-only`/`off`. |
| `disableSkillShellExecution` | Disable inline `` !`...` `` shell in skills/commands. |

### Lifecycle / maintenance

| Key | Description |
| --- | --- |
| `cleanupPeriodDays` | Delete session files older than N days (default 30, min 1). |
| `env` | Env vars applied to every session + spawned subprocesses. |
| `plansDirectory` | Plan file location (default `~/.claude/plans`). |
| `autoUpdatesChannel` | `stable` or `latest`. |
| `minimumVersion` | Floor preventing downgrade. |
| `respectGitignore` | `@` picker respects `.gitignore` (default true). |
| `fileSuggestion` | Custom `@`-autocomplete script. |
| `defaultShell` | `bash` or `powershell` for `!` commands. |

### Worktrees / agents / teams

| Key | Description |
| --- | --- |
| `worktree.baseRef` | `fresh` (origin default) or `head`. |
| `worktree.symlinkDirectories` / `worktree.sparsePaths` / `worktree.bgIsolation` | Worktree behavior. |
| `agent` | Run main thread as a named subagent. |
| `disableAgentView` | Turn off background agents / agent view. |
| `teammateMode` / `teammateDefaultModel` | Agent-team teammate behavior. |

### Plugins & marketplaces (mostly managed)

`extraKnownMarketplaces`, `enabledPlugins`, `strictKnownMarketplaces` (M),
`blockedMarketplaces` (M), `pluginTrustMessage` (M),
`strictPluginOnlyCustomization` (M).

### Enterprise / policy (managed-only)

`forceRemoteSettingsRefresh`, `parentSettingsBehavior`, `policyHelper`,
`allowManagedPermissionRulesOnly`, `channelsEnabled`, `allowedChannelPlugins`,
`wslInheritsWindowsSettings`, `bwrapPath`, `socatPath`.

## Example

```json
{
  "$schema": "https://json.schemastore.org/claude-code-settings.json",
  "model": "opusplan",
  "outputStyle": "Explanatory",
  "includeCoAuthoredBy": false,
  "cleanupPeriodDays": 30,
  "env": { "FOO": "bar" },
  "permissions": {
    "defaultMode": "acceptEdits",
    "allow": ["Bash(git diff *)"],
    "deny": ["Read(./.env)", "Read(./.env.*)"]
  },
  "statusLine": { "type": "command", "command": "~/.claude/statusline.sh", "padding": 2 },
  "hooks": { "PreToolUse": [ { "matcher": "Bash", "hooks": [ { "type": "command", "command": "./check.sh" } ] } ] }
}
```

## Gaps vs llmenv

llmenv's `generate_settings_json` (`src/adapter/claude_code.rs:183`) emits a fixed
stub:

```json
{ "hooks": [], "permissions": [], "mcp": [] }
```

Three problems, in order of severity:

1. **`hooks` and `permissions` are emitted as empty arrays.** The real schema is
   `hooks: { <Event>: [ {matcher, hooks:[...]} ] }` (an object keyed by event) and
   `permissions: { allow:[], ask:[], deny:[], defaultMode, additionalDirectories, sandbox }`
   (an object). The stub's shapes are **wrong**, not just empty — a real merge
   pass must produce objects, not arrays.
2. **`mcp` is not a settings key.** Emitting it is harmless but meaningless;
   remove it. MCP enable/deny would be `enabledMcpjsonServers` /
   `disabledMcpjsonServers` / `enableAllProjectMcpServers`.
3. **~115 other keys are entirely unmodeled.** Nothing in the YAML schema
   (`src/config/schema.rs`) lets a user express `model`, `env`, `outputStyle`,
   `statusLine`, `cleanupPeriodDays`, `permissions.*`, `includeCoAuthoredBy`,
   `language`, etc. The `Settings` struct in the schema is about llmenv's *own*
   cache/sync behavior, not Claude Code settings — there is no passthrough.

Design implications for a real `settings.json` generator:

- Decide the **selection model**: do settings attach to bundles (tag-selected,
  merged) like everything else, or is there a dedicated `settings:` block? Given
  array-merge semantics in Claude Code, bundle-contributed `permissions.allow`
  lists would compose naturally.
- **Hooks** need their own pipeline: today hook *files* are copied into `hooks/`
  and `{{ICM_MCP}}`-substituted, but nothing wires them into the `hooks` key of
  `settings.json`. The files are inert without the settings entries that
  reference them. This is the core of issue #34.
- Consider emitting `$schema` for editor validation.
- `CLAUDE_CONFIG_DIR` is set by `env_vars`; verify managed-settings precedence
  does not silently override generated keys in the user's environment.
