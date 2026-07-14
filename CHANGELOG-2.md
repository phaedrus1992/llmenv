<!-- markdownlint-disable MD024 -->

# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

<!-- 2.0 next-header -->

## [2.4.1] - 2026-07-10

- CI updates to support trusted publishing to crates.io

## [2.4.0] - 2026-07-10

### Added

- Add per-hash temp directory isolation for Claude Code subprocesses: `CLAUDE_CODE_TMPDIR`,
  `TMPDIR`, `TMP`, and `TEMP` env vars now point to `<cache_dir>/<hash>/tmp/`, scoping
  temporary files to the current content hash (#630)
- Add durable plugin cache directory: `CLAUDE_CODE_PLUGIN_CACHE_DIR` now points to
  `<state_dir>/plugins/` so plugins are not re-downloaded on every scope change (#632)

### Fixed

- Fix hook context emission including `additionalContext` content in store-only events
  (SessionStart, SessionEnd), which Claude Code's hook schema rejects — store-only events now
  emit empty output instead of triggering a validation error at the end of every session (#558)
- Fix `CONTEXT_MODE_DATA_DIR` and other state-directory env vars (from
  `materialize::state::state_env_vars`) emitting platform-native path separators
  (`\` on Windows) instead of forward slashes, breaking cross-platform compatibility
  for consumers that parse paths in these env vars. Normalization consolidated into
  the existing `normalize_rel` helper. (#497)

## [2.3.0] - 2026-06-30

### Added

- Add `features.throttle`: keep an LLM backend within its rate limits by polling usage and
  inserting a capped, adaptive delay as the request budget runs low, instead of hitting a hard
  429. Tag-scoped like `features.memory`; currently supports the `umans` backend (#487)

## [2.2.1] - 2026-06-24

### Fixed

- Fix `llmenv export` aborting with "variable value contains forbidden control character" for
  `LLMENV_ICM_CONTEXT` and other legitimately multiline values; value validation now rejects only
  NUL, since every emission path single-quotes the value and newlines are inert there (#469)

## [2.2.0] - 2026-06-23

### Added

- Add built-in `token-efficiency` example bundle with env vars (`LLMENV_BASH_BAN`, `CBM_WARN_THRESHOLD`,
  `CBM_AUTOINDEX`), SessionEnd auto-handoff hook, SessionStart context-mode reminder hook,
  PostToolUse reject-scanner scaffold, and minimal `native_permissions` limiting Bash to
  state-mutation operations (git, mkdir, curl, trash). Include per-stack rule files (`bash.md`,
  `rust.md`, `typescript.md`, `skill-gates.md`) documenting the skill-gate pattern for conditional
  skill activation by language tag, prerequisite, or indexed content (#218, #219, #220, #222, #223)
- Add `--compress` flag to `llmenv export`: strips trailing whitespace and collapses excessive blank
  lines for token-efficient AGENTS.md output (#226)
- Wire `LLMENV_BASH_BAN` env var into the Claude Code adapter permission layer: when set, denies
  Bash tool invocations whose commands match any comma-separated prefix pattern before execution
  (#464)

### Fixed

- Fix `token-efficiency` example bundle declaring `BASH_BAN` instead of `LLMENV_BASH_BAN`; the
  Bash deny feature silently failed for any user of the example config (#466)
- Fix `token-efficiency` example bundle placing env vars under `features.env` instead of the
  top-level `env` key and using snake_case hook event names instead of PascalCase (e.g.
  `session_end` → `SessionEnd`); env vars were not exported and hooks never fired
- Fix `LLMENV_BASH_BAN` accepting patterns containing `)`, `(`, and newlines that produced
  malformed deny rules; invalid pattern characters are now rejected at startup (#465)
- Fix `LLMENV_BASH_BAN` treating a non-unicode env var value the same as the variable being
  unset; non-unicode values now return an error instead of silently disabling enforcement (#465)
- Fix `llmenv export --compress` not preserving the final newline, producing non-POSIX output
  (#465)

<!-- 2.1 next-header -->

## [2.1.0] - 2026-06-23

### Added

- Add `session_log` config field: opt-in JSONL tracing of all llmenv log events to
  a file for diagnosing hooks and materialization without reading stderr (#382)
- Add SSH auth negotiation timeout (`ssh -o ConnectTimeout`) and HTTP pack-transfer
  stall detection (`http.lowSpeedTime`/`http.lowSpeedLimit`) to all git subprocesses,
  preventing indefinite hangs on slow or unresponsive servers (#453)
- Add annotated `examples/my-llmenv/` reference config: a fully commented example
  covering `config.yaml`, five bundles, hooks, skills, rules, and scripts
- Detect volta, fnm, Linux pnpm (`~/.local/share/pnpm/`), and macOS pnpm
  (`~/Library/pnpm/`) install paths when seeding `installMethod` in Claude Code
  settings; previously these were classified as `native`

### Fixed

- Fix `GIT_SSH_COMMAND` being overwritten by llmenv's SSH timeout injection;
  user SSH identity files, `ProxyJump`, and other existing SSH customizations
  are now preserved
- Fix `seed_install_method` overwriting a user-customized `installMethod` value
  in Claude Code `settings.json`; the field is now only written when absent
- Fix `seed_install_method` silently swallowing I/O errors (e.g. permission
  denied) when reading `settings.json`; non-NotFound errors now propagate
- Fix long interactive session pause when GitHub remote is unreachable: all git
  subprocesses now apply a TCP connection timeout (`GIT_CONNECT_TIMEOUT` — 10 s
  for background fetch/pull, 30 s for explicit plugin clone/fetch)
- Fix malformed `marketplace.json` entries (missing or invalid `source` field)
  being silently dropped; these now emit a `warn` log with the entry details (#361)

### Security

- Reject NUL, newline, and carriage-return characters in env var values at config
  load time; these were previously accepted silently and could interfere with shell
  export (#356)
- Reject `file://` transport in external plugin source URLs; only `https://` and
  SSH remotes are permitted (#360)
- Remove `StrictHostKeyChecking=accept-new` from llmenv's SSH options for git
  operations; this option weakened host-key verification (MITM/DNS-hijacking
  exposure) and was unrelated to the timeout feature it was grouped with

<!-- 2.0 next-header -->

## [2.0.5] - 2026-06-18

### Added

- Fold six `*-ls` listing commands into `status` subcommands: `status bundles`,
  `status tags`, `status scopes`, `status mcps`, `status marketplaces`,
  `status plugins`. The top-level `*-ls` forms are retained as hidden deprecated
  shims and will be removed in 2.1.
- Add `context --bundle <name>` to narrow the context view to a single bundle,
  showing its env vars, hooks, MCPs, plugins, and skills
- Add `context --why` to show activation tracing — which scope triggered each
  active tag and which tags fired each bundle
- Add `export --explain` to annotate each exported variable with its source
  (adapter or llmenv introspection)
- Add `sync --dry-run` to preview pending config changes without committing
- Add `check-stale --auto-fix` to automatically re-materialize config on drift
  rather than only printing a warning
- Add `validate` command to check config for structural issues (duplicate bundle
  names, bundles with no activation tags)
- Add `edit [bundle-name]` command to open `config.yaml` or a named bundle file
  directly in `$EDITOR`
- Add `completions <shell>` command to generate shell completion scripts for
  bash, zsh, and fish
- Document `regenerate`, `login`, `config-context`, and `config-guard` commands
  in `commands.md`; add `regenerate` and `login` to the `getting-started.md`
  quick-reference table
- Expand `doctor` entry in `commands.md` to list the token-efficiency settings
  it checks

### Fixed

- Fix `edit` command allowing paths outside the config root via `..` traversal;
  the target path is now canonicalized and validated before opening
- Fix `edit` command ignoring arguments in `$EDITOR` (e.g. `code --wait`);
  the editor value is now split on whitespace before invoking
- Fix `validate` not checking `enable_bundles` references in project-scoped
  config; unknown bundle names now report an error regardless of scope type
- Fix `plugin-sync` silently succeeding when a configured plugin is absent from
  the marketplace manifest after sync; it now prints a user-visible error and
  exits non-zero
- Fix `status` listing commands and `doctor --all` incorrectly classifying MCPs,
  bundles, and plugins as orphaned when their `when:` tags are emitted only by
  project scopes; the emitted-tag set now includes project-scope active tags

## [2.0.4] - 2026-06-16

### Added

- Provide prebuilt `linux/aarch64` (ARM64) release binaries

### Fixed

- Fix `hookEventName` being emitted at the top level of hook JSON instead of
  inside `hookSpecificOutput`; it is now nested per the Claude Code hook schema,
  so hooks that read the event name from context find it in the right place (#419)
- Fix `llmenv plugin-sync` silently dropping all externally-sourced plugins
  (e.g. `slack`, `superpowers`) whose `marketplace.json` entry uses the
  `{"source": "git", "url": "..."}` object form; only bare-string sources were
  parsed, so every object-form entry was lost. Malformed object-form entries now
  emit a warning, and the related messages correctly direct users to
  `llmenv plugin-sync` instead of `llmenv sync`
- Fix hooks crashing with a broken-pipe error when the agent truncates their
  stdout early; hooks are fail-soft and now exit 0 on `SIGPIPE` (#422)
- Fix bundle and tag memory recall errors being silently discarded; all MCP
  action failures (recall, tag recall, bundle recall, store) now emit a
  `tracing::warn!` with structured context so misconfigured or unreachable
  recall is diagnosable without source-level debugging (#421)

## [2.0.3] - 2026-06-15

### Fixed

- Fix `SessionStart` (and other hook) output missing the required `hookEventName` field,
  causing Claude Code to reject hook JSON with "hookSpecificOutput is missing required
  field 'hookEventName'" on startup

## [2.0.2] - 2026-06-14

### Fixed

- Fix `cargo release --workspace` not bumping sub-crates: add explicit
  `shared-version = true` to each sub-crate `release.toml` so cargo-release
  treats them as part of the workspace version group
- Fix CI publish step silently timing out when sub-crate versions don't match
  the release tag: add upfront version validation that fails fast with a clear
  error message
- Fix `pre-release-hook = []` panic in cargo-release 1.1.2: remove empty hook
  arrays from sub-crate configs and update workspace hook to use `${WORKSPACE_ROOT}`
  so it resolves correctly from any sub-crate working directory

## [2.0.1] - 2026-06-14

### Fixed

- Fix multi-crate crates.io publishing: enable sub-crates (`llmenv-util`, `llmenv-paths`,
  `llmenv-git`, `llmenv-config`) for publishing with required metadata, bump all to 2.0.0
  to match root, and publish in dependency order with crates.io indexing polls in CI

## [2.0.0] - 2026-06-14

### Added

- Add token-efficiency checks to `llmenv doctor`: warns when
  `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE`, `BASH_MAX_OUTPUT_LENGTH`, `MAX_MCP_OUTPUT_TOKENS`,
  or `ENABLE_PROMPT_CACHING_1H` are not set (or misconfigured); informs when
  `CLAUDE_CODE_SUBAGENT_MODEL` is unset; warns when no `context-mode` MCP server is
  configured
- Add `config::template::generate_template()` function; `llmenv init` now derives the
  config template from a single source rather than a hardcoded string, making it easier
  to keep the template in sync as the schema evolves
- Add `llmenv config-context` subcommand, auto-registered as a `SessionStart` hook
  by the Claude Code adapter; emits source config file and bundles directory paths
  as `hookSpecificOutput.additionalContext` so the agent always knows where to edit
  llmenv config rather than touching managed cache files
- Add `llmenv config-guard` subcommand, auto-registered as a `PreToolUse` hook
  (matcher: `Write`, `Edit`, `MultiEdit`) by the Claude Code adapter; warns when the
  agent writes to a path inside the managed cache directory and redirects to the
  source config; always exits 0 (fail-soft, never blocks the write)
- Add stable authentication cache: `oauthAccount` credentials are now stored in
  `state/auth/<uuid>.json` outside the content-hashed config dir and automatically
  re-injected on every new materialization; Claude Code no longer requires
  re-authentication after a version bump, project switch, or directory change (#172)
- Add `llmenv login [--global]` subcommand: captures credentials via `claude auth login`,
  saves them to the stable auth cache, and optionally persists them globally (#172)
- Add `init.seeded_settings` to `config.yaml`: user-selected keys from
  `~/.claude/settings.json` are seeded into `settings.json` on first materialization of
  a new config folder, carrying over preferences without overwriting managed settings;
  `llmenv init` now prompts to log in, import from `~/.claude`, or skip (#172)
- Add per-bundle `features.memory` overrides: bundles can declare a `features:` block in
  `bundle.yaml` to use a different memory daemon `server_host` per scope, enabling
  different daemons on different machines or networks without a global config change (#335)

### Changed

- Replace ASCII pipeline and precedence diagrams in the concepts and philosophy
  documentation pages with Mermaid flowcharts; the diagrams now render as proper
  graphs on the Docusaurus docs site

### Removed

- **Breaking:** Remove `env` (and its deprecated alias `vars`) from the top-level
  `bundle:` config field. Bundle-level environment variables must now be declared
  in `bundle.yaml` under `capabilities.env`. (#352)

### Fixed

- Fix `config-guard` path-prefix check accepting `..`-based traversal paths (e.g.
  `~/.cache/llmenv/../../../etc/shadow` matched as inside the cache); paths are now
  normalized lexically before the prefix check
- Fix `config-guard` silently swallowing JSON parse failures when the hook payload was
  malformed; non-empty non-JSON stdin now logs a warning to stderr
- Fix `config-guard` not logging when `CLAUDE_CONFIG_DIR` is set but has no recognizable
  `claude-code` ancestor directory; the fallback is now visible to operators
- Fix `config-context` silently substituting a wrong default path when config path
  resolution fails; it now emits a warning to stderr and returns a degraded-state context
  message rather than feeding the agent incorrect file paths
- Fix missing bundle directories being silently ignored; `llmenv` now logs a warning
  when a configured bundle name has no corresponding directory, making typos and
  deleted directories detectable
- Fix `mcp[].env` keys not being validated for the `LLMENV_` prefix or reserved state
  vars (`CLAUDE_CONFIG_DIR`, `LLMENV_STATE_DIR`); these were accepted silently where
  `capabilities.env` already rejected them, creating an inconsistent validation gap
- Fix git fetch spawn errors logged at `debug` level in the background sync path;
  a spawn error (git binary missing or misconfigured) is unexpected and is now logged
  at `warn` so operators can see it
- Fix git reset errors during explicit plugin sync silently logged at `debug` level;
  errors are now logged at `warn` so sync failures surface in production logs (#376)
- Fix clock skew silently bypassing the pull throttle check; when the stored sync
  timestamp is in the future, `llmenv` now logs a `warn` with the skew magnitude
  (`skew_secs`) and proceeds with the pull rather than silently skipping it (#377)
- Fix missing `plugin.json` after a plugin sync being silently ignored; `llmenv` now
  logs a `warn` when the plugin manifest is absent after materializing the plugin,
  making broken plugin installs diagnosable (#379)

<!-- 1.0 next-header -->

<!-- next-url -->
[2.4.1]: https://github.com/phaedrus1992/llmenv/compare/v2.4.0...v2.4.1
[2.4.0]: https://github.com/phaedrus1992/llmenv/compare/v2.3.0...v2.4.0
[2.3.0]: https://github.com/phaedrus1992/llmenv/compare/v2.2.1...v2.3.0
[2.2.1]: https://github.com/phaedrus1992/llmenv/compare/v2.2.0...v2.2.1
[2.2.0]: https://github.com/phaedrus1992/llmenv/compare/v2.1.0...v2.2.0
[2.1.0]: https://github.com/phaedrus1992/llmenv/compare/v2.0.5...v2.1.0
[2.0.5]: https://github.com/phaedrus1992/llmenv/compare/v2.0.4...v2.0.5
[2.0.4]: https://github.com/phaedrus1992/llmenv/compare/v2.0.3...v2.0.4
[2.0.3]: https://github.com/phaedrus1992/llmenv/compare/v2.0.2...v2.0.3
[2.0.2]: https://github.com/phaedrus1992/llmenv/compare/v2.0.1...v2.0.2
[2.0.1]: https://github.com/phaedrus1992/llmenv/compare/v2.0.0...v2.0.1
[2.0.0]: https://github.com/phaedrus1992/llmenv/compare/v1.0.10...v2.0.0
