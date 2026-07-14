<!-- markdownlint-disable MD013 -->
# Permissions reference

Sources: <https://code.claude.com/docs/en/permissions>,
<https://code.claude.com/docs/en/permission-modes> (fetched 2026-05-27).

Permissions live under the `permissions` key of `settings.json` and control what
tools Claude can run without asking. Sandbox config lives under `sandbox`.

## `permissions` keys

| Key | Description |
| --- | --- |
| `allow` | Array of rules to auto-allow. `["Bash(git diff *)"]` |
| `ask` | Array of rules that always prompt. |
| `deny` | Array of rules to block (use for secrets). `["Read(./.env)"]` |
| `defaultMode` | `default`, `acceptEdits`, `plan`, `auto`, `dontAsk`, `manual`, `bypassPermissions`. |
| `additionalDirectories` | Extra working dirs for file access (most `.claude/` config is *not* discovered from these). |
| `disableBypassPermissionsMode` | (M) `"disable"` to block bypass mode + `--dangerously-skip-permissions`. |
| `skipDangerousModePermissionPrompt` | Skip the bypass-mode confirmation. |
| `allowManagedPermissionRulesOnly` | (M) Only managed allow/ask/deny apply. |

Rules **merge (concatenate + dedupe) across scopes**, unlike scalar settings.

## Rule syntax

`Tool` or `Tool(pattern)`:

| Example | Matches |
| --- | --- |
| `Bash` | all Bash commands |
| `Bash(npm run *)` | commands starting `npm run` |
| `Read(./.env)` | reading `.env` |
| `Read(./.env.*)` | reading `.env.*` |
| `Edit(...)` | edits to paths (also feeds sandbox `filesystem` allow/deny) |
| `WebFetch(domain:example.com)` | fetches to that domain |

Tool names are the exact strings from the tools reference; the same names are
hook matchers.

## Permission modes

Cycle with Shift+Tab (CLI) or the mode selector (IDE/Desktop/web).

| Mode | Behavior |
| --- | --- |
| `default` | Ask before edits/commands per rules. |
| `acceptEdits` | Auto-accept file edits; still ask for other tools. |
| `plan` | Read-only planning; no edits/commands. |
| `auto` | Auto-mode classifier decides (block/soft-deny/allow rules; see `autoMode`). |
| `dontAsk` | Suppress prompts (within rule bounds). |
| `bypassPermissions` | Skip all prompts (the `--dangerously-skip-permissions` mode). |
| `manual` | Equivalent to `default`; set via the mode selector in the CLI/IDE. |

## Sandbox (`sandbox` key)

The sandboxed Bash tool (macOS/Linux/WSL2) provides filesystem + network
isolation. Selected keys:

| Key | Description |
| --- | --- |
| `enabled` | Enable bash sandboxing (default false). |
| `failIfUnavailable` | Exit if sandbox can't start (else warn + run unsandboxed). |
| `autoAllowBashIfSandboxed` | Auto-approve bash when sandboxed (default true). |
| `excludedCommands` | Commands to run outside the sandbox. |
| `allowUnsandboxedCommands` | Allow `dangerouslyDisableSandbox` escape hatch (default true; set false to force). |
| `filesystem.allowWrite` / `denyWrite` / `allowRead` / `denyRead` | Path lists (merge across scopes; also merged with `Edit`/`Read` rules). |
| `network.allowedDomains` / `deniedDomains` | Outbound domain allow/deny (wildcards; deny wins). |
| `network.allowLocalBinding`, `allowUnixSockets`, `allowAllUnixSockets`, `allowMachLookup`, `httpProxyPort`, `socksProxyPort` | Network specifics. |
| `enableWeakerNestedSandbox`, `enableWeakerNetworkIsolation` | Reduced-security escape hatches. |
| `bwrapPath`, `socatPath` | (M) Linux/WSL2 binary paths. |

## Excluding sensitive files

Canonical pattern:

```json
{ "permissions": { "deny": ["Read(./.env)", "Read(./.env.*)", "Read(./secrets/**)"] } }
```

## Gaps vs llmenv

- llmenv emits `"permissions": []` — wrong shape (should be an **object** with
  `allow`/`ask`/`deny`/`defaultMode`/…), and empty.
- No YAML vocabulary exists for permission rules, modes, `additionalDirectories`,
  or any `sandbox.*` config.
- This is a natural bundle-level feature: a `rust-defaults` bundle could
  contribute `Bash(cargo *)` allows; a `base` bundle could contribute the `.env`
  denylist. Claude Code's array-merge semantics mean bundle contributions compose
  cleanly — design the YAML so each bundle's permission lists concatenate.
- Security note: the user's global CLAUDE.md already encodes a secrets-handling
  posture; generating the `deny` list for `.env`/credentials from a base bundle
  would make that posture declarative and consistent across hosts.
- `defaultMode` is a scalar (last-writer-wins across scopes), so it needs a single
  owning scope or an explicit precedence rule in the llmenv merge.
