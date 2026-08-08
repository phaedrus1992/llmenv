<!-- markdownlint-disable MD013 -- entries are one dense bullet per change, not wrapped prose -->

# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

<!-- 3.0 next-header -->

## [Unreleased] - ReleaseDate

### Added

- Inherit the Claude Code OAuth token across cache folders, so a config edit or version bump no longer produces a login prompt. Previously only the account identity (`oauthAccount`) was inherited — the folder knew who you were but not that you were logged in. Covers both stores: `.credentials.json` on Linux/WSL and the macOS keychain item, whose service name embeds a hash of the config-dir path and so is no more stable across folders than a file. A live cached token is never overwritten by a stale folder's, and a folder's own token is never replaced. `llmenv login` captures the token too; `llmenv doctor` reports whether one is cached and whether it expired; `llmenv doctor --gc` drops the keychain item belonging to each folder it deletes. See [Configuration](https://phaedrus1992.github.io/llmenv/docs/configuration#oauth-credential-inheritance) (#1057)
- Keep third-party MCP server logins (Slack, Notion, Linear, …) across cache folders. Claude Code stores those tokens under `mcpOAuth` in the same store as the login token, so they ride along with it — but a lapsed Claude login no longer discards them, since the two authenticate different things and expire independently. `mcp-needs-auth-cache.json` is inherited too, so Claude Code doesn't re-probe every OAuth server after a hash change, and `llmenv doctor` reports how many MCP tokens are cached. See [Configuration](https://phaedrus1992.github.io/llmenv/docs/configuration#third-party-mcp-server-logins) (#1058)
- Warn about `native_<feature>.<engine>` keys no engine will ever read, instead of dropping them silently. A typo (`native_mcp.opencde`), or a key naming a real engine whose adapter doesn't read that map (`native_model_providers.claude_code`, `native_hooks.opencode`), used to parse, merge, and hash cleanly and then vanish. `llmenv export`, `llmenv regenerate`, and `llmenv doctor` now report both cases across every per-engine map, reading the merged config so keys contributed by a `bundle.yaml` are covered; `llmenv validate` fails outright on an unknown engine id. See [Engines](https://phaedrus1992.github.io/llmenv/docs/engines#engine-keys-are-validated) (#1032)
- Flag `capabilities.permissions` patterns that use Claude Code's colon-prefix syntax (a trailing `:*` command prefix, or a `domain:`/`url:` filter) when opencode is also installed and enabled. opencode matches a pattern as a plain glob, so the rule never applies there — and a dead `deny` fails open, which the warning calls out specifically. Reported by `llmenv doctor`, `llmenv export`, and `llmenv regenerate`, for bundle-contributed rules as well as top-level ones. See [`doctor`](https://phaedrus1992.github.io/llmenv/docs/commands#doctor) (#838)
- Add `llmenv task ls --current-project` to narrow a task listing to the current project's tasks (any session ever tagged to it, open or closed), and `llmenv task show --current`/`--next` to jump straight to the task in progress (or the next actionable one after it) without hunting through `task ls` first. See [`task`](https://phaedrus1992.github.io/llmenv/docs/commands#task) (#927, #928)
- Add `llmenv completions --install` to write a shell completion script straight to its standard directory ($BASH_COMPLETION_USER_DIR/`~/.local/share/bash-completion/completions` for bash, $ZSH_CUSTOM/`~/.zsh/completions` for zsh, `~/.config/fish/completions` for fish) instead of only ever printing to stdout — most users never discovered `completions` existed because wiring it up meant knowing the right path yourself. Auto-detects the shell from `$SHELL` when omitted, `--dir` overrides the target, `--force` allows overwriting an existing file. See [`completions`](https://phaedrus1992.github.io/llmenv/docs/commands#completions) (#756)
- Add `features.cd_guard`, a warn-only `PreToolUse` advisory on Bash commands that `cd`, on by default. "Shell cwd was reset to `<path>`" was the single most common non-empty Bash stderr signature across ~18k archived sessions (77 occurrences) — Claude Code resets the working directory after every Bash call that `cd`s, standalone or as the leading step of a compound command, silently breaking any following command that assumed the new directory. Prose guidance alone wasn't stopping it; this mechanizes the reminder instead, without ever blocking the call. See [Configuration](https://phaedrus1992.github.io/llmenv/docs/configuration#featurescd_guard) (#976)
- Add `capabilities.permissions.preset: safe-readonly`, a core-shipped bundle of `allow` rules (with `deny` companions closing the one dangerous flag each tool has) for the read-only CLI tools this project's own bundled rules already tell the agent to prefer — `rg`, `ast-grep`, `shellcheck`, `shfmt`, plus read-only `git status`/`diff`/`log`/`show`/`blame` and `ls`. `fd` is excluded: its own dangerous flag can hide behind a short-flag cluster in a way a `deny` glob can't catch. 272 of the "Claude needs your permission" prompts across ~18k archived sessions were for exactly these tools, because core shipped no default `allow` rules for them and every config had to reinvent its own. See [Configuration](https://phaedrus1992.github.io/llmenv/docs/configuration#capabilities) (#975)
- `llmenv doctor` now flags a config that allows a legacy tool (`grep`, `find`) without also allowing the replacement this project's own rules recommend for it (`rg`, `fd`) — a cheap nudge toward the new `safe-readonly` preset for configs that haven't adopted it. See [Configuration](https://phaedrus1992.github.io/llmenv/docs/configuration#capabilities) (#975)

### Changed

- `llmenv task start <id>` now refuses to start a task with an unmet `blocked_on` reference instead of only warning and starting it anyway — `blocked_on` is an explicit dependency the user configured on purpose, so an unresolved one is a real ordering violation, not just untidy. Pass `--force` to override. A `blocked_on` reference is satisfied only once the target task *and every one of its descendants* are done, so blocking on a parent task alone covers its whole child set (e.g. several parallel sibling tasks) without a `block` edge per sibling. An undone `--parent` relationship is unaffected — it's organizational grouping, not an ordering guarantee, and now gets an explicit soft-block warning (starts anyway) where previously nothing checked it at all. See [`task`](https://phaedrus1992.github.io/llmenv/docs/commands#task) (#1164)
- `llmenv task ls` now requires `--session <id>` or `--all` — it previously defaulted to listing every session's tasks across the whole store with no flag at all, easy to reach for by accident when only the current session's tasks were wanted. Pass `--all` to deliberately see everything. See [`task`](https://phaedrus1992.github.io/llmenv/docs/commands#task) (#1124)
- `hook-run` reuses the bundle-merge result from the last `regenerate`/`export` instead of redoing it on every invocation. The prior in-process merge cache (#813) never actually hit in real usage — each `hook-run` is a fresh subprocess — so the disk I/O and YAML parsing behind memory-backend resolution ran on every `SessionStart`/`TurnStart`/`SessionEnd`. It's now persisted to a small cache file keyed on bundle/config content, with a live merge as the fallback whenever that key doesn't match. See [Materialize](https://phaedrus1992.github.io/llmenv/docs/concepts#materialize) (#920)
- `LLMENV_TRACE_TIMING`'s per-phase marker now fires on every `hook-run` event, not just the ones that reach the full memory-dispatch stage (previously 4 of 11). Each field is present only for phases the event actually reached, so an early return still reports whatever `config_load`/`scope_eval` cost it incurred instead of nothing. See [Troubleshooting](https://phaedrus1992.github.io/llmenv/docs/troubleshooting#profiling-hook-run-latency) (#1128)

### Fixed

- A bundle a project turns off via `disable_bundles` no longer contributes its `features.memory`/`host` entries to the ICM memory endpoint that lifecycle hooks resolve. `hook-run` computed its own firing-bundle set that honored tag matches and `enable_bundles` but skipped `disable_bundles`, so hooks could resolve memory against a bundle the materialized manifest had already excluded — and, for a project that set `disable_bundles`, the two disagreeing sets also meant the bundle-merge cache never hit. A disabled bundle is likewise no longer named in memory recall queries or in the context chunk stored in the backend. See [Configuration](https://phaedrus1992.github.io/llmenv/docs/configuration#project-markers) (#1125)
- `no memory backend active for this scope` now says which of the four causes applies instead of one message for all of them: no bundles fired, nothing declares `features.memory`, a firing bundle has no content directory (so its `bundle.yaml` was never read), or the only bundle supplying memory is turned off via `disable_bundles`. `llmenv doctor --all` warns about that last case too — previously memory worked in `~/`, stopped the moment you `cd`'d into the project, and `doctor` stayed green. A top-level `features.memory` entry whose `server_host` lives in a disabled bundle's `host:` table now names the bundle in its error as well. See [Configuration](https://phaedrus1992.github.io/llmenv/docs/configuration#project-markers) (#1131). Found during review of #1125.
- A `bundle.yaml` llmenv can't parse or read no longer masquerades as "no memory backend configured". The bundle merge behind memory-endpoint resolution swallowed its error and defaulted to no bundle contributions, so a broken bundle file sent you off to read your scope config — the one place the problem wasn't. It now reports the parse failure. A failed merge-cache signature is logged rather than silently degrading the #920 optimization to unexplained hook latency. (#1132). Found during review of #1125.
- Detached memory children can no longer fail into `/dev/null`. Web-fetch memory stores, post-session consolidation, and detached transcript records were spawned with their stderr discarded, and the errors meant to compensate logged at a level the default filter drops — so any of them could fail with no trace anywhere. Their stderr now goes to `$XDG_STATE_HOME/llmenv/detached-hook.log` (owner-only, rotated at 512 KiB), and their failures log at error level, mirroring the fix #1086/#1091 shipped for the mcp-proxy and indexer logs. See [Troubleshooting](https://phaedrus1992.github.io/llmenv/docs/troubleshooting#memory-backend-issues) (#1133). Found during review of #1125.
- `llmenv doctor` no longer reports `native_permissions.opencode` and `native_permissions.crush` as orphaned keys. The orphan check hardcoded `claude_code` as the only engine name, so the two newer adapters' own permission overrides were flagged even though both adapters read them. It also no longer treats an MCP server name as a valid `native_permissions` key — that map is keyed by engine, so such a key was itself dead config. (#1032)
- `llmenv doctor`'s engine binary check no longer skips engines added after it was written; it now walks the adapter registry instead of a hardcoded `crush`/`opencode` list. (#1032)
- `llmenv setup` no longer skips engines added after it was written. Both `probe_engines` (which checks `PATH` for installed engines) and `compute_disabled_engines` (which computes the resulting `disabled_engines` config) hardcoded the same three-engine list rather than reading the adapter registry, so a new adapter wouldn't be offered by the wizard and could end up explicitly disabled even with its binary installed. Same bug class as #1032. (#1074)

- `llmenv statusline` no longer vanishes when `config.yaml` won't parse. It rendered nothing at all — the command exited non-zero with empty stdout and the parse error went only to a stderr the engine discards, so a YAML typo silently blanked the status line in every open terminal with no signal anywhere. It now exits 0 and renders `⚠️ llmenv: config error — run 'llmenv doctor'` instead. See [`statusline`](https://phaedrus1992.github.io/llmenv/docs/commands#broken-config-renders-an-error-row) (#1052)

- An IPv6 `memory.listen_host` no longer starts a proxy llmenv can never see. The bind address was assembled as `{host}:{port}`, giving `::1:9092` — which the liveness probe can't parse, since IPv6 needs bracketing. So the proxy started, went undetected, and every following export waited out the bind window, reported "did not bind", and started another one. The address is now built (and parsed) through `SocketAddr`, so the two can't disagree. Found during review of #1084–#1086. (#1087)
- A `^C` (or a dropped SSH session) while `mcp-proxy` was starting no longer disables the memory backend permanently. `llmenv export` runs in the shell's foreground process group, so it died holding its spawn lockfile — and with no staleness check, every later export failed against a file most users had never heard of. The lock now records its holder and is reclaimed when that process is gone. A concurrent export during a cold start also waits for the first one's proxy instead of immediately reporting a lockfile error. Found during review of #1084–#1086. (#1087)
- `mcp-proxy` startup failures are diagnosable again. The proxy's stderr went to `/dev/null`, so a proxy that wouldn't start produced only "did not bind … check that the port is free and mcp-proxy is correctly installed" — advice that named two causes that were both wrong in practice, while the real one (an `ImportError` from `mcp-proxy`'s open-ended `mcp` requirement) was only visible by re-running the command by hand. Its stderr now goes to `$XDG_STATE_HOME/llmenv/mcp-proxy.log` (owner-only, rotated at 1 MiB), the failure warning quotes the tail of that log, and the speculative hints are gone. See [MCP Servers and the Memory Backend](https://phaedrus1992.github.io/llmenv/docs/mcp#proxy-lifecycle-on-the-server-host) (#1086)
- `llmenv export` no longer warns that `mcp-proxy` failed to start when it started fine. The post-spawn check slept a fixed 300 ms and probed once, but a real proxy takes ~0.55 s to bind (~2 s via `uvx`, which pays uv's resolve cost) — so every cold start printed a bind-failure warning and deleted the pidfile, while the proxy it had just launched came up moments later and kept running, orphaned. llmenv now polls for the bind every 50 ms for up to 5 s, and reports a proxy that exits before binding immediately rather than waiting the budget out. See [MCP Servers and the Memory Backend](https://phaedrus1992.github.io/llmenv/docs/mcp#proxy-lifecycle-on-the-server-host) (#1084)
- Stop recording a dead pid as the running `mcp-proxy`, and stop launching a second proxy when the first is already serving. Liveness required *both* a pidfile and a listening port, so a live proxy whose pidfile went missing read as dead: llmenv spawned a replacement that died instantly on the taken port, wrote that dead child's pid to the pidfile, and then saw the *original* proxy answer its probe — reporting success. The pidfile was left permanently wrong but non-empty, which the old check read as proof of life forever, and the "listen_host is '0.0.0.0'" warning fired on a run that started nothing. The bind address is now the sole liveness signal; the pid is written only after the bind is confirmed and the child is confirmed alive, and a pidfile naming a process that isn't running is cleared. See [MCP Servers and the Memory Backend](https://phaedrus1992.github.io/llmenv/docs/mcp#proxy-lifecycle-on-the-server-host) (#1085)

- Stop losing `/resume` history on every cache-folder change. Claude Code keeps its transcripts in `projects/` inside `CLAUDE_CONFIG_DIR`, so a config edit or version bump left the session list empty. `projects/` now lives once in the durable state dir with each folder symlinked to it, and `history.jsonl` is copied in when a folder has none. Transcripts stranded by the old behavior are folded into the shared store on first run, newest copy of a session winning. The previous `migrate_ephemeral` mechanism only ran in `strict` hashing mode and scanned the wrong directory level, so on the default mode it never migrated anything. See [Configuration](https://phaedrus1992.github.io/llmenv/docs/configuration#inherited-claude-code-state) (#1059)

- A cached ICM transcript session id is no longer trusted forever. Session logging correlates each Claude Code session with an ICM transcript session, recorded once and reused on every later hook event — but if ICM restarted or pruned that session in between, the stale id was replayed with no recovery short of restarting `llmenv`. It's now revalidated once per launch (at `SessionStart`) before being trusted, and a failed revalidation re-establishes a fresh session instead. Found during review of #1087. (#1090)
- A failing `codebase-memory-mcp` index run is diagnosable again instead of leaving nothing to look at. Indexing a repo can take minutes and runs detached so it never blocks `SessionStart`, but its stderr went to `/dev/null` — so a failure partway through was invisible. It's now captured to `<index_path (or its default)>/index.log`, size-bounded and owner-only, mirroring the same fix #1086 shipped for the mcp-proxy log. See [Configuration](https://phaedrus1992.github.io/llmenv/docs/configuration#featurescodebase_memory) (#1091). Found during review of #1087.
- A stale cached MCP session id is recovered from instead of replayed forever. `llmenv`'s MCP HTTP client caches the `Mcp-Session-Id` a server hands out on `initialize` and reuses it on every call — but a server restart or session expiry made every later call fail (HTTP 400/404) with no recovery short of restarting `llmenv`. It now clears the cache and re-initializes once before giving up. Found during review of #1087. (#1094)
- A locked macOS keychain no longer reads as "no credential stored". `security find-generic-password` failed with its stderr discarded, so a keychain awaiting unlock and a genuinely absent credential looked identical — both silently degraded into an unexplained re-login prompt. Any lookup failure other than the documented "item not found" exit code now surfaces as an error naming the likely cause. `security add-generic-password` failures also report the tool's own diagnostic instead of just a status code. Found during review of #1087. (#1092)
- Post-session consolidation no longer leaks a `claude` subprocess on every LLM-call timeout. The 120-second call to `claude -p` ran without `kill_on_drop`, so a timeout dropped the process handle without terminating it — each one potentially holding an open API session. Same root cause as the `mcp-proxy` orphan #1087 fixed. Found during review of #1087. (#1093)

- A set-but-empty `LLMENV_STATE_DIR`, `LLMENV_CONFIG_DIR`, or `CLAUDE_CONFIG_DIR` (e.g. from a stray `export FOO=` in a shell profile) is no longer treated as a real override. It resolved to a relative path, scattering the task tracker's `tasks/*.json`, the statusline's usage-delta cache, or `llmenv-status.json` into whatever directory the process happened to run from instead of the intended state/config/cache location — invisible to every later command run from elsewhere. All three now fall through to their documented default, same as when the variable is unset. Found during pre-pr-review of #1109. (#1111)
- `TaskList`/`TaskCreate` no longer report an unreadable task or session store as an empty one. `list_tasks`/`list_sessions` collapsed a genuine read error (permission denied, a bad mount, an `LLMENV_STATE_DIR` pointing at a file) to the same empty result as "nothing tracked yet," so `TaskList` denied with a false "(no tasks tracked yet)" and `TaskCreate` could auto-start a second session on top of the store it couldn't read — both now surface the real error and point at `llmenv task` for a manual fallback instead. Found during pre-pr-review of #1109. (#1112)
- The task/session store's directories and its lock file are now created owner-only (`0700`/`0600`) from the moment they're created, instead of at the default (often world-readable) permissions and narrowed only later. Found during pre-pr-review of #1109. (#1113)
- `write_owner_only_atomic`'s parent directory is now owner-only (`0700`) at every level, not just the immediate parent, and a directory that already existed at a looser mode is hardened too. It used to `create_dir_all` the parent (default umask, typically `0755`) and chmod only that immediate parent afterward — a TOCTOU window, and a permanent world-readable state for any intermediate ancestor the chmod never touched, or any directory created before this hardening existed. Found during pre-pr-review of #1177. (#1178)
- A set-but-empty `HOME` (e.g. from a stray `export HOME=` in a shell profile) is no longer treated as a real value. `expand_tilde` expanded `~/rest` to `/rest` — anchored at the filesystem root — instead of leaving it unchanged like an unset `HOME`; the interactive setup wizard's config/plugin scanning and project-tag/scope discovery had the same gap. Same bug class as #1111. Found during pre-pr-review of #1177. (#1179)
- Five more state/cache directories are now created owner-only (`0700`): `mcp-proxy`'s pidfile/lockfile parent and its bounded-log directory, the session-log append directory, the throttle usage-cache directory, and the durable materialization state dir (plus every configured tool's subdirectory). Same bug class as #1178. Found during pre-pr-review of #1184. (#1186)
- `llmenv doctor --all` now flags a network scope whose `match` has no `gateway_mac` as an orphan that can never activate. The matcher only evaluates `gateway_mac`; `ssid`/`cidr` are accepted by the config schema and documented as fields, but silently ignored — so a scope keyed only on `ssid`/`cidr` never fired, with no signal anywhere short of reading the docs. See [Getting Started](https://phaedrus1992.github.io/llmenv/docs/getting-started#common-first-errors) (#1051)
- Five more directories are now created owner-only (`0700`): the bundle materialization cache root, the `read_once`/`repeat_detect` hook state directories, the plugin/marketplace cache root, and `llmenv init`'s config directory. Same bug class as #1178/#1186. Found during pre-pr-review of #1186's own PR. (#1196)
- A user-configured `features.codebase_memory.index_path` is no longer forced to `0700`. Since #1186, indexing forced that permission unconditionally, which broke setups sharing the directory with a `codebase-memory-mcp` process running under a different uid (separate service account, differently-mapped container) — indexing then failed with an `EACCES` visible only via debug logging. Only llmenv's own default state-dir-rooted cache directory is still hardened; an explicit `index_path` override now keeps whatever permissions its owner already gave it. See [Configuration](https://phaedrus1992.github.io/llmenv/docs/configuration#featurescodebase_memory) (#1196)
- A timed-out post-session consolidation call no longer orphans `claude -p`'s own descendants. #1093 made a timeout kill the direct `claude -p` child instead of leaking it, but `kill_on_drop` only signals that one pid — any MCP servers or tool subprocesses `claude -p` spawned kept running. It's now spawned into its own process group, and a timeout kills the whole group. Found during pre-pr-review of #1163. (#1165)
- Four more directories are now created owner-only (`0700`): the bundle materialization cache root under the default hashing mode (#1196 only reached the less-common strict mode's cache root, leaving the default mode unprotected), the statusline widget's PR-lookup and usage-delta caches, `llmenv doctor`'s cache-directory-writable check, and the plugin-payload cache directory. Same bug class as #1178/#1186/#1196. Found during pre-pr-review of #1196's own PR. (#1198)

## [3.7.0] - 2026-07-28

Mostly config-schema hardening: `native.<engine>` fragments now reject malformed
shapes and point at the right escape hatch instead of silently dropping config,
and tags/bundle names are validated instead of failing silently deep in ICM.
Also ships opencode model-provider rendering parity with Crush, an on-by-default
repeat-loop guard (`features.repeat_detect`), `LLMENV_EXTRA_TAGS` for
tag-activation without a committed marker file, and a 1997 GeoCities-style
retro skin for the docs site.

### Added

- Give the docs site (`website/`) a 1997 GeoCities-style retro skin — dark black-and-gold theme, tiled background, marquee banner, under-construction badge, and a per-browser hit counter, all checked against WCAG AA contrast. Site-only change; no `llmenv` CLI/config behavior affected. (#1027)
- Add background MIDI music to the docs site, playing continuously while browsing. Includes a fixed mute/play toggle per WCAG 2.1's audio-control requirement, since browsers already block true autoplay until the visitor interacts with the page. (#1027)
- Add model provider configuration rendering to the opencode adapter — `capabilities.model_providers`/`default_models` now render into `opencode.json`'s `provider`/`model`/`small_model` fields, matching the existing Crush support. `api_type` maps to the AI SDK package name opencode expects (e.g. `openai` → `@ai-sdk/openai-compatible`); `default_models`'s `large`/`small` roles map to opencode's two default-model slots. See [Configuration](https://phaedrus1992.github.io/llmenv/docs/configuration) (#1004)
- Add `capabilities.native_model_providers.<engine>` — the escape hatch for provider keys opencode and Crush accept but `capabilities.model_providers` has no field for (opencode's per-model `reasoningEffort`, say). Deep-merges onto the rendered provider block, and renders on its own so a hand-written provider survives `llmenv regenerate`. See [Engines](https://phaedrus1992.github.io/llmenv/docs/engines#native_model_providersengine) (#1008)
- Add `features.repeat_detect`, an engine-neutral guard against stuck-loop behavior, **on by default**. Covers two cases: a model repeating the identical tool call `threshold` times in a row (default 3), and — the more common real-world trigger — a model ignoring the task tracker's "you still have a task in progress" reminder every turn instead of pausing it. Both surface an advisory (the tool-call case nudges trying something else; the reminder case points at `llmenv task wait <slug> "<reason>"`) rather than blocking anything, and it fires for any adapter/model since it lives in the shared lifecycle-hook layer rather than per-adapter code. See [Configuration](https://phaedrus1992.github.io/llmenv/docs/configuration#featuresrepeat_detect) (#1006)
- Add `LLMENV_EXTRA_TAGS`, a comma-separated env var that unions extra tags into the active scope tag set — works with or without a committed `.llmenv.yaml`, for cases like a client repo you can't add config files to, a throwaway clone, or a personal-only tag you don't want to share via a checked-in file. See [Configuration](https://phaedrus1992.github.io/llmenv/docs/configuration#activating-tags-without-a-committed-marker) (#1020)

### Changed

- The task-tracker redirect messages for Claude Code's built-in `TaskCreate`/`TaskUpdate` now mention `llmenv task wait|block`, not just `start|note|done`, so the agent is pointed at the full command set instead of just the original three. Also trimmed the redirect and Stop-hook wording (`stop_hook_reminder`) to cut repeated boilerplate on every turn/call, and shrank `skills/llmenv/references/task-tracker.md` from 97 to 29 lines to match its sibling reference files. See [`task`](https://phaedrus1992.github.io/llmenv/docs/commands) (#994, #995)

### Fixed

- Rejecting a modeled key in `native.<engine>` pointed you at `native_<key>.<engine>` as if that field always existed — for `provider`, `model`, `lsp`, and `instructions` it never did. The error now names the one hatch that applies, or the neutral `capabilities` field when there is none. See [Engines](https://phaedrus1992.github.io/llmenv/docs/engines) (#1008)
- A `native_*.<engine>` fragment that wasn't a mapping (usually a YAML indentation slip) silently deleted the whole block it was meant to merge into and exited 0 — taking any neutrally-declared MCP servers or hooks with it. It now errors, naming the field and the shape it got. See [Engines](https://phaedrus1992.github.io/llmenv/docs/engines) (#1008)
- The SessionStart/Stop task-tracker reminders scoped `wip` tasks to the current project but not the current session, so an agent in one terminal could be nudged with directive "keep working — don't stop mid-task" language about a task a completely different, concurrently-running session owned — risking two agents driving the same branch/PR at once. Each task in the reminder now names the session that started it, and the wording never presumes ownership: it conditions resuming or finishing a task on the agent actually recognizing it as its own earlier work. See [`task`](https://phaedrus1992.github.io/llmenv/docs/commands) (#1028)
- `Capabilities::is_empty()` never checked `features.codebase_memory`, so a config fragment whose only content was a `codebase_memory` entry was silently reported as empty — dropping it wherever `is_empty()` gates rendering/merging. It now accounts for `codebase_memory` like every other feature list. See [Configuration](https://phaedrus1992.github.io/llmenv/docs/configuration#featurescodebase_memory) (#1021)
- `merge_capabilities` hardcoded `advisor_size` to `None`, so setting `advisor_size` in any bundle or scope silently never reached the generated engine settings. It's now resolved by highest-precedence-wins like every other scalar capability field. Found during pre-pr-review of #1025.
- Document `capabilities.model_providers`/`capabilities.default_models` in the configuration reference — the schema has supported custom model-provider endpoints and role-keyed default models for several releases with no user-facing docs. See [Configuration](https://phaedrus1992.github.io/llmenv/docs/configuration) (#994)
- `llmenv materialize`'s `opencode.schema.json` sidecar — documented as shipping back in 3.3.0 (#660) but never actually wired into the crate — now really gets written alongside `opencode.json`, which now points its own `$schema` field at the sidecar instead of opencode's hosted schema. See [Engines](https://phaedrus1992.github.io/llmenv/docs/engines#what-the-opencode-adapter-emits) (#1001)
- `docs/env-vars.md` documented `LLMENV_ACTIVE_TAGS`/`LLMENV_ACTIVE_SCOPES`/`LLMENV_ACTIVE_BUNDLES` as colon-separated; the code has always joined them with commas. Corrected while adding docs for `LLMENV_EXTRA_TAGS` (#1020)
- A tag (or bundle name in `enable_bundles`/`disable_bundles`) from `.llmenv.yaml`, `config.yaml`'s scopes, or `$LLMENV_EXTRA_TAGS` containing anything outside alphanumeric/`-`/`_` used to pass through unnoticed until ICM's recall query rejected it — silently disabling memory recall/store *and* session logging for the rest of the session, with no visible error. Tags and bundle names are now validated (and length- and count-capped) where they're created; invalid or excess entries are dropped with a `tracing::warn!` (visible with `RUST_LOG=warn`) instead. See [Configuration](https://phaedrus1992.github.io/llmenv/docs/configuration#project-markers) (#1035)
- The MCP docs page linked the `memory:` config reference at a nonexistent anchor (`configuration#memory`), dropping readers at the top of the Configuration page instead of the `features.memory:` section. See [MCP Servers and the Memory Backend](https://phaedrus1992.github.io/llmenv/docs/mcp) (#1037)

## [3.6.1] - 2026-07-24

A bug-fix and small-UX patch centered on the task tracker: Claude Code's built-in task tools now feed the `llmenv task` tracker instead of bypassing it, `task ls` output is grouped and filterable, and reminders no longer leak across projects. It also fixes feature-enabled MCP permission precedence on Claude Code and trims per-session context bloat — the statusline `{pr}` and `branch` widgets self-resolve their PR under engines that don't send one, rendered hooks no longer fire twice per event, and the ICM memory injection stays silent when the store is empty. Adds the opencode adapter, stale MCP server pruning, and tiered MCP permission rules for built-in servers.

### Added

- Add Opencode engine adapter (`src/adapter/opencode.rs`) — full feature parity with the Claude Code adapter: renders `opencode.json` (MCP, LSP, permissions, env vars), `AGENTS.md` with frontmatter translation, rules, and a JS hook bridge shim that maps Opencode plugin events to llmenv hook subprocess calls with Claude-shaped stdin payloads. Plugin content (skills, commands, agents, MCP) from Claude Code bundles is translated into Opencode-native forms ([#657](https://github.com/phaedrus1992/llmenv/issues/657))
- Add model provider configuration rendering to the Crush adapter — `capabilities.model_providers` and `capabilities.default_models` are now rendered into `crush.json` ([#682](https://github.com/phaedrus1992/llmenv/issues/682))
- Add stale MCP server pruning to the Claude Code adapter — servers previously owned by llmenv but absent from the resolved set are removed from `.claude.json`, preserving user-added servers ([#739](https://github.com/phaedrus1992/llmenv/issues/739))
- Add tiered MCP permission rules for built-in servers (ICM, context-mode) — read-only tools are auto-allowed, mutation tools prompt the user, and destructive tools are denied, matching the sensitivity tier of each tool ([#694](https://github.com/phaedrus1992/llmenv/issues/694))
- `llmenv task ls` human output now groups tasks by session (current-project sessions first), indents subtasks under their parent, prefixes each row with a state glyph + label, and annotates blocked tasks with their `blocked_on` refs; new `--state <open|wip|waiting|done>` (repeatable) and `--hide-done`/`--active` filters compose with `--session` and apply to `--format json` too. See [`task`](https://phaedrus1992.github.io/llmenv/docs/commands) (#926)
- Feature-enabled MCPs (`features.context_mode`, `features.memory`) now take a `mcp_permissions` override to customize the read-only/mutation/destructive tier→action policy per feature. See [`mcp_permissions`](https://phaedrus1992.github.io/llmenv/docs/configuration#featuresmcp_permissions) (#946)

### Changed

- The bundled `llmenv` skill's task rules now guide agents to link tasks liberally with `--parent` (ordered decomposition) and `block --on` (real dependencies) and to record milestones, design rationale, and failures with `task note`. See [`task`](https://phaedrus1992.github.io/llmenv/docs/commands) (#932)

### Fixed

- Fix opencode hook shim generating misleading warning when bundle path resolution fails — diagnostic now correctly describes stale or restructured bundles ([#769](https://github.com/phaedrus1992/llmenv/issues/769))
- Fix `split_frontmatter` crash on empty/single-delimiter input in the opencode adapter ([#769](https://github.com/phaedrus1992/llmenv/issues/769))
- Fix silent `remove_file` error discard in claude_code companion file cleanup — now emits `tracing::warn!` on failure
- Add `tracing::warn!` diagnostics to `read_owned_servers` I/O and parse error paths
- The task-tracker Stop hook no longer re-injects the `waiting`-task FYI every turn; `waiting` tasks are now silent on Stop and surface only in the SessionStart reminder. See [`task`](https://phaedrus1992.github.io/llmenv/docs/commands) (#933)
- `llmenv task add` no longer warns "you have N task(s) already in progress" for `waiting` tasks — only genuinely `wip` tasks count, since starting new work alongside a task paused on external input is legitimate (#933)
- The statusline `{pr}` widget no longer renders empty under engines (like Claude Code) that don't send a `pr` field — it now self-resolves via `gh pr view` for the current branch, cached briefly so it doesn't shell out on every render. See [`statusline:`](https://phaedrus1992.github.io/llmenv/docs/configuration) (#950)
- The task-tracker Stop hook's `wip` reminder and SessionStart's `waiting` reminder no longer leak across projects sharing the same task store — a `wip`/`waiting` task from one project no longer nags a hook running in another. See [`task`](https://phaedrus1992.github.io/llmenv/docs/commands) (#949)
- Feature-enabled MCP permissions (context-mode, ICM) no longer conflict between a wildcard allow and per-tool tier rules; Claude Code's `deny > ask > allow` precedence was silently shadowing the wildcard, so mutation tools prompted on every call and destructive tools were blocked outright even with the feature enabled. Default policy now allows read-only and mutation tools without prompting, and asks before destructive ones. An explicit `native_permissions` rule on a built-in MCP tool now takes precedence over the tier default for that tool (`deny > ask > allow`), rather than emitting a competing entry. See [`mcp_permissions`](https://phaedrus1992.github.io/llmenv/docs/configuration#featuresmcp_permissions) (#946, #972)
- The statusline `branch` widget's PR hyperlink no longer stays inert under engines (like Claude Code) that don't send a `pr` field — the branch text now links to the current branch's PR via the same self-resolving `gh pr view` lookup the `{pr}` widget uses (#950), sharing its short-lived cache. See [`statusline:`](https://phaedrus1992.github.io/llmenv/docs/configuration) (#973)
- Rendered `settings.json` no longer lists each hook twice for the same event on a first or strict render — the freshly generated hooks doc is now deduped at generation time (the same strip-nulls-then-dedup pass `reconcile` already applied when a prior file existed), so each guard fires once per event instead of launching two (or, for dual-interpreter guards, four) processes per tool call (#977)
- The ICM memory injection no longer adds a `No memories found` block or a "consider saving" nag to the context on every prompt when the store is empty — advisory-line stripping is now case-insensitive to server wording, and a recall left with only advisory/blank lines injects nothing (#978)
- With the task tracker enabled, Claude Code's built-in `TaskCreate`/`TaskList`/`TaskUpdate` tools are now redirected into the `llmenv task` tracker instead of Claude's ephemeral task state — `TaskCreate` records a real task (auto-starting a session when none is open), `TaskList` returns the tracker's view, and `TaskUpdate` maps status to start/done/delete. Previously the agent's built-in task tools bypassed the tracker, so it sat mostly unused. See [`task`](https://phaedrus1992.github.io/llmenv/docs/commands) (#985)
- Features set at the root of `config.yaml` (`features:`) are no longer silently dropped from the generated engine config. `build_manifest` only fed `merge()` the `capabilities:` block, so a root-level `task_tracker`, `slippage`, or `context_mode` (incl. its `mcp_permissions` override) never reached the manifest that renderers gate on — the task-tracker hooks, slippage guardrails, built-in skill reference docs, and MCP-permission overrides could all silently go missing. Root `features:` now folds into the merged manifest (root wins over bundle-contributed values) (#987)
- A hook removed from your config no longer lingers in the generated `settings.json`. `reconcile` unions rendered hooks with what's already on disk (to preserve hooks a plugin self-registers at runtime), which meant a hook llmenv *used to* render but no longer does was kept forever. llmenv now records the hooks it renders and, on the next render, purges its own dropped hooks while still preserving genuinely-foreign ones (#991)

## [3.6.0] - 2026-07-22

3.6.0 includes three new engine-facing pieces — an in-engine task tracker, a first-class `llmenv statusline` subcommand, and a third supported engine (opencode, alongside Claude Code and Crush) — plus a `codebase-memory-mcp` integration.

A string of hook-run perf work landed too: single-walk `scope.content` matching instead of one walk per matcher, `uname(2)` instead of shelling out to `hostname`, memory-recall dedup, and cutting redundant `config.yaml` re-parses and per-invocation clones/reads/stats across hook-run, export, and regenerate.

On the fix side: opencode permission precedence and malformed-rule handling, skill-frontmatter YAML escaping for control chars and Unicode noncharacters, several `read_once`/session-log ordering bugs, and null-valued hook keys leaking into generated engine configs.

### Added

- Add an in-engine task tracker (`llmenv task add|start|done|wait|ls|show|note|block|clear`), off by default. See [`task`](https://phaedrus1992.github.io/llmenv/docs/commands) (#231)
- Add mandatory, project-tagged task sessions: every task belongs to a session, each session is tagged with the project it started in, and any number can be open at once. `task session start` surfaces an existing same-project session with a `--resume`/`--replace`/`--new` checkpoint instead of colliding; sessions carry a `--description`, and `task session ls` lists the open ones for recovery after a context compaction. See [`task`](https://phaedrus1992.github.io/llmenv/docs/commands) (#905)
- Add an `llmenv` skill materialized into every engine (Claude Code, opencode, Crush) with a reference file per enabled built-in (task tracker, memory, context-mode, codebase-memory), replacing the old Claude-Code-only task-tracker CLAUDE.md fragment. See [`task`](https://phaedrus1992.github.io/llmenv/docs/commands) (#905)
- Add a first-class `llmenv statusline` subcommand with 21 configurable widgets, replacing the old ad hoc status line. See [`statusline:`](https://phaedrus1992.github.io/llmenv/docs/configuration) (#836)
- Opt-in per-phase hook-run timing via `LLMENV_TRACE_TIMING` — emits phase durations as one `llmenv-trace {json}` stderr line, off by default
- `llmenv doctor` flags `hook.matcher` values shaped like file globs (e.g. `*.rs`) — Claude Code only matches `hook.matcher` against tool name, so these silently never fire (#837)
- Add `features.codebase_memory`, a first-class integration for [codebase-memory-mcp](https://github.com/DeusData/codebase-memory-mcp). See [MCP servers](https://phaedrus1992.github.io/llmenv/docs/mcp) (#365)
- Add the opencode adapter — `opencode` is now a third supported engine alongside `claude_code` and `crush`, at near-parity with Claude Code. See [Engines](https://phaedrus1992.github.io/llmenv/docs/engines) (#876)

### Changed

- Hook-run performance: single-walk `scope.content` matching instead of one walk per matcher (#703), `uname(2)` instead of shelling out to `hostname`, memory-recall dedup for repeated blocks, and fewer redundant `config.yaml` re-parses/clones/reads/stats across hook-run, export, and regenerate

### Fixed

- Bundle/user hooks no longer emit null-valued `tool`/`command` keys into the generated Claude Code or Crush config (#720)
- Skill frontmatter `name`/`description` containing control characters or Unicode noncharacters no longer produces invalid YAML when auto-quoted (#859, #873)
- `features.read_once` no longer silently drops Debug-level session-log capture for `PreToolUse` events (#864)
- A computed `read_once` deny/advisory result is no longer discarded if an unrelated hook-run pipeline error occurs afterward (#867)
- `SessionEnd` session-log capture is no longer skipped when the redundant-store dedup check fires (#866)
- opencode adapter: a native `allow` rule no longer silently overrides a structured `deny` rule for the same tool+pattern (#877); a malformed native permission rule string no longer falls back to wildcard-allow (#882)
- A hook whose handler `type` doesn't match its populated field now fails config load with a clear error, instead of silently loading as a no-op (#851)
- A computed `read_once` deny result is now always enforced (was only guarded by `debug_assert!`, a no-op in release builds) (#868)
- `config.yaml` now rejects a duplicate `scope.content` id, matching the existing `network`/`host`/`user` check (#843)
- Claude Code adapter: a `Write` permission rule is now rewritten to `Edit` before reaching `settings.json`, matching Claude Code's own deprecation (#888)
- opencode/crush plugin materialization no longer fails with a missing `install_location` when `cache.remote_sync: false`
- The `icm` statusline widget always rendered empty — its parser expected JSON, but the underlying tool returns plain text (#903)
- The `config_stale` statusline widget ignored a custom icon override unless a custom `format` was also set (#904)
- Sync-state, marketplace-manifest, and MCP-proxy pidfile reads now surface non-`NotFound` I/O errors (e.g. permission denied) instead of masking every stat failure as "file absent" (#893)
- `llmenv memory diff` no longer risks overwriting the snapshot baseline when a stat error masks an existing snapshot as absent, and now surfaces read errors (#911); the opencode adapter surfaces permission errors on a plugin's `commands/`/`agents/` directories instead of silently skipping them (#912)
- Directory and file reads across cache prune/gc, skill validation, bundle rules/content ingestion, opencode plugin MCP/hooks parsing, and settings import now surface permission errors instead of an `exists()` stat masking them as "absent" — closing the last of this class, including a case where an unreadable skills directory silently bypassed skill validation (#915, #916)

## [3.5.1] - 2026-07-15

### Fixed

- `remote_sync` no longer blocks manual `llmenv sync` and `llmenv plugin-sync` commands — it only gates the non-interactive throttled pull during `llmenv export` (#835)

## [3.5.0] - 2026-07-15

### Added

- Configurable session-log retention: `session_log.transcript.retention_days` — best-effort deletion of stale session-log files before each SessionStart; validated >= 1 (#812)
- Add `cache.remote_sync` config option (default `true`) to disable remote git operations — prevents shell freezes when 1Password's SSH agent is locked and an SSH askpass prompt hangs terminal-based git ops (#833)

### Changed

- Build manifest once per export/regenerate instead of once per adapter, reducing repeated work in multi-engine setups (#708)
- Hot-path optimizations for hook-run pipeline: cache Env::detect() results (30s TTL), cache bundle merge by config mtime, reuse Tokio runtime and MCP HTTP client via OnceLock (#813)

### Fixed

- Remove dead process-static CONFIG_CACHE from hook_run that never saved a parse (each hook event is a fresh process); poisoned-cache log no longer fires on cold-start misses (#706)
- Add eprintln! diagnostic when fs::canonicalize() fails in read-once, so operators can detect non-canonicalized cache keys (#728)
- Add eprintln! diagnostic when deprecated PascalCase 'filePath' key is used in read-once, surfacing format drift (#729)
- Preserve MCP server sub-keys (runtime auth tokens) across re-materialization in `merge_mcp_into_claude_json` — fixes silent auth loss on every materialize in Loose/Normal mode (#814)
- Fail fast on manifest build error with preserved error chain instead of silently falling back to stale manifest (#708)
- Gate git marketplace and external plugin sync behind `cache.remote_sync` to prevent hangs when remote sync is disabled
- Distinguish local-only commits from pushed commits — prints "Committed locally (remote sync disabled — push skipped)" instead of misleading "Synced config to GitHub" when remote_sync is off
- Add `## Version X.x` headers to the generated website changelog for correct section hierarchy across major versions

## [3.4.0] - 2026-07-14

This release tightens error diagnostic coverage across two dozen silent-fallthrough
sites, adds PermissionMode variants for granular permission control, hardens cache
GC edge cases, and normalizes JSON/YAML merge null-strip behavior.

### Added

- Add `auto`, `dontAsk`, and `manual` PermissionMode variants alongside
  existing boolean/string forms — `auto` is only honored from user-scope
  settings, `dontAsk` skips the permission prompt, and `manual` matches
  the default deny-mode behavior (#748)
- Migrate ephemeral state (`projects/`) across hash changes in Strict
  mode materialization (#746, #797)

### Fixed

- Fold `strip_json_nulls` into `normalize_json` so every merge path (not just
  `reconcile_settings`) benefits from null-tolerant merge dedup (#718)
- Add null-stripping to `normalize_yaml` and insert-path null guard to
  `merge_yaml` for YAML merge parity with JSON (#718)
- Session log transcript correlation (`session_log::state`) no longer
  silently fails when `state_dir()` is unavailable — falls back to CWD with
  a `tracing::warn!` instead of returning `None`/`Err` (#737)
- Add `tracing::warn!` diagnostics to 7 additional silent-error swallowing
  sites in file_sink, event serialization, read-once canonicalize, throttle
  error body, consolidation error body, and MCP client error body reads (#773)
- Enrich pre-subscriber diagnostics — promote event serialization failures
  to `error!`, add URL context to throttle/consolidation error messages,
  and log fallback path in `state_path()` warnings (#784)
- Surface silent error swallowing in read-once hook — `state_dir()`
  resolution failures are now logged as warnings before returning empty
  strings (#760)
- Surface silent error swallowing in doctor version skew check —
  `read_dir` failures on adapter cache directories are now logged as
  warnings instead of being silently skipped (#764)
- Surface silent error swallowing in login auth status update —
  `CacheManifest::read` failures are now logged as warnings instead of
  being silently skipped (#765)
- Surface silent error swallowing in auth, throttle, hook-run, and
  reconcile_settings — read/parse failures are now logged as warnings instead
  of being silently discarded (#749)
- Fix transcript session id parsing — ICM returns the session id as a JSON
  object, not a bare ULID, so every transcript record call was passing a JSON
  blob instead of a real id and records went nowhere (#755)
- Add diagnostics for walkdir entry errors in scope matcher — I/O errors
  during directory traversal are now logged as warnings instead of silently
  skipped (#752)
- Add diagnostics for project marker file read errors — read failures on
  `.llmenv.yaml` are now logged as warnings before returning defaults (#753)
- Add diagnostics for config-context stdin JSON parse failures — parse
  errors are now logged as warnings before falling back to SessionStart (#754)
- Surface silent error swallowing in settings.json parse — parse failures
  in `apply_seeded_settings` are now logged as warnings instead of silently
  returning defaults (#762)
- Surface silent error swallowing in version comparison — malformed version
  strings in `compare_versions` are now logged as warnings instead of silently
  returning `Equal` (#766)
- Surface silent error swallowing in session log path resolution — path
  resolution failures are now logged to stderr instead of silently falling
  back to CWD before the tracing subscriber is initialized (#763)
- Upgrade `debug_assert!` to `tracing::warn!` in scope matcher — walkdir
  entries outside the workspace root are now surfaced as warnings instead
  of only being checked in debug builds (#761)
- Remove angle brackets from bare URLs in changelog and release docs —
  `<url>` is interpreted as JSX by Docusaurus, breaking the `docs.yml`
  CI build against `website/docs/changelog.md` and `website/docs/release.md`
  (#811)
- GC in Normal mode now age-checks each shape individually instead of
  treating the entire version generation as one unit (#738, #797)
- Clock-skew handling in GC — entries with future mtimes are now
  treated as expired with a logged warning instead of silently skipped
  (#797)
- Edge-case hardening in cache lifecycle — log I/O errors in ephemeral
  migration, attempt older siblings on copy failure, clean up `.tmp`
  staging directories in GC, and log unexpected entries (#797)

## [3.3.0] - 2026-07-13

### Deprecated

- The old boolean `session_log` shape (`file: bool`, `transcript: bool`,
  `verbose: bool`) is deprecated. It still parses in 3.x but will be
  removed in 4.0. Migrate to the new per-sink mapping blocks. ([#744](https://github.com/phaedrus1992/llmenv/issues/744))

### Removed

- Remove dead `diff` field from `ReadOnce` config schema — the
  planned phase-2 delta mode was never implemented (#725)

### Changed

- `session_log.verbose` replaced with per-sink `level` (info/debug/trace).
  `session_log.file` and `session_log.transcript` are now mapping blocks with
  `enabled` + `level` fields. Old boolean shape still parses. ([#740](https://github.com/phaedrus1992/llmenv/issues/740))

### Fixed

- Early-exit hook-run before scope evaluation for events that
  produce no memory actions — saves ~3.5ms per PreToolUse
  dispatch on a loaded config (#702)
- Thread `--engine` flag through to adapter selection so
  hook-runs targeting non-default engines (e.g. opencode)
  actually use the correct adapter instead of always env-sniffing
  (#704)
- Fix WebSearch auto-store labelling "URL: unknown" instead of
  the actual search query — read `tool_input.query` for WebSearch
  and label as `Query:` (#707)
- Strip ICM advisory lines ("Consider saving", "No memories found.")
  from hook-run recall output — ~1KB/turn of noise in agent
  conversations (#692)
- Fix doctor false-flagging marketplaces pinned to annotated
  tags as broken — `git rev-parse <tag>` returns the tag
  object SHA, not the commit SHA; use `^{commit}` peeling for
  commit-vs-commit comparison (#695)
- Fix project-scoped tags from `.llmenv.yaml` leaking into
  host-level plugin collection, MCP server, and throttle
  resolution — introduce `non_project_tags()` to exclude
  project-scoped tags from host config generation (#696)
- opencode adapter not activating when `OPENCODE_CONFIG_DIR` is unset
  (now falls back to checking if `opencode` is on PATH) (#657)
- Fix read-once hook using PascalCase `filePath` when Claude Code
  sends snake_case `file_path` — production read-once was a
  complete no-op against any Read call (#724)
- Move `prune_stale_sessions` from `SessionCache::load()` (runs
  on every Read) to `save()` — eliminates redundant readdir +
  stat per Read call (#726)
- Surface silent error swallowing in config load, session-log
  correlation, and setup detection — add `inspect_err`
  diagnostics before `.ok()`/`.ok()?`/`unwrap_or_default()` that
  silently discarded errors (#731, #710, #712, #713)

### Added

- Add `llmenv upgrade` subcommand for self-upgrade from
  GitHub releases (`--check`, `--track beta|release`,
  `features.upgrade.track` config option) (#686)
- Add model provider configuration
  (`capabilities.model_providers`) with schema types,
  validation, merge rules, and CrushAdapter rendering
  (#526, #527, #528)
- Add default model selection
  (`capabilities.default_models`) for role-keyed model
  resolution across providers (#530)
- Add content-based scope matching with file glob
  patterns (`scope.content`) — auto-activates tags when
  matching files exist in the working directory, without
  requiring `.llmenv.yaml` markers (#278)
- Cache hashing now supports `version: major` granularity — set
  `hashing: { normal: { version: major } }` in config.yaml to key
  cache folders on major version only (e.g. `1/` instead of `1.2/`).
  Default remains `minor` for full backward compatibility. (#651)
- opencode engine support — new `opencode` adapter with full parity
  vs the claude-code adapter: AGENTS.md, rules, skills, MCP
  (local/remote), LSP, permissions, hook bridging via a generated JS
  shim plugin, and Claude-plugin content translation (#656, #657)
- JSON Schema generation for materialized configs — adapters that
  derive `JsonSchema` on their output structs now emit a
  `{adapter}.schema.json` sidecar alongside the native config file,
  enabling IDE validation and editor autocompletion for materialized
  opencode.json files. (#660)
- Add read-once file deduplication hook — tracks files
  read via the Read tool within a session and skips
  re-reading unchanged files within a configurable TTL
  (`features.read_once`). Includes deny-mode envelope to
  block writes to never-read files (#318)
- Add slippage control bundle — effort-level injection
  and compaction-survival rules to improve agent behavior
  consistency across long sessions
  (`features.slippage`) (#317)
- Add TTL-based memory retention pruning
  (`llmenv memory prune`, `memories.retention` config with
  per-type durations, `memories.auto_prune` flag during
  materialize) (#270)
- Add post-session LLM consolidation — after SessionEnd,
  distills recent memories into permanent semantic rules
  via direct Anthropic API call, reducing context drift
  across sessions (#595)

## [3.2.0] - 2026-07-11

### Changed

- Move WebFetch/WebSearch ICM storage and PostSession consolidation to background
  detached child processes, reducing hook latency for common events (#670)
- Cache parsed config by file mtime in hook-run to avoid redundant YAML parsing on each event (#670)

### Added

- `llmenv doctor` checks that config-dependent executables (`icm`,
  `mcp-proxy`/`uvx`, `claude`, `crush`) are available on `PATH`,
  respecting each tool's config conditions (memory entries, disabled
  engines, optional status). (#655)
- Add Discord community link to README and getting-started guide

### Fixed

- `capabilities.permissions` and `native_permissions` rules
  (top-level or bundle-contributed) whose `pattern`/`paths` have
  unbalanced parentheses — e.g. a process-substitution deny pattern like
  `bash <(curl *` — are now rejected at config-load time with a fix hint,
  instead of rendering into a `Tool(pattern)` string that Claude Code/Crush
  silently drop at settings-load time. This previously left `deny` rules
  silently non-functional with no warning from `llmenv doctor` or config
  validation. (#664)
- Validate skill-file paths with CommonMark-aware parsing (`pulldown-cmark`)
  instead of fragile heuristics. Fenced/indented code blocks and inline code
  spans containing `~/.claude` no longer falsely trigger configuration-path
  validation errors. (#659)
- Fix root-level `lsp:` and `skills:` declarations in `config.yaml` not
  being materialized into the rendered manifest. These were parsed,
  validated, and documented but silently never reached the output. (#661)
- Fix false `"marketplace.json broken"` warning from `llmenv doctor` when
  the context-mode marketplace clone is properly synced but lacks a
  standalone `marketplace.json` — the marketplace is managed internally
  and the check was a false positive
- Fix loopback address detection in the ICM MCP SSRF guard to cover the
  full `127.0.0.0/8` range, unspecified addresses (`::`, `::0`, `0.0.0.0`),
  and provide a safer fallback when `needs_proxy` cannot be determined
- Fix background PostSession consolidation child process inheriting stdin,
  which could cause hangs; add trace logging for CONFIG_CACHE poison
  detection

## [3.1.0] - 2026-07-10

### Added

- Auto-activate OS tag in scope resolution — bundles with OS-specific `when:` tags
  (e.g. `linux`, `macos`, `windows`) now activate automatically without requiring
  manual scope configuration (#638)
- Create plugin cache directory automatically on export (`CLAUDE_CODE_PLUGIN_CACHE_DIR`),
  and add `llmenv prune --plugin-cache` flag for explicit shared plugin cache cleanup (#643)

### Fixed

- Build static Linux binaries with musl (`*-linux-musl`) instead of glibc
  (`*-linux-gnu`) so the pre-built Homebrew-tap binaries work on any Linux
  distro regardless of system glibc version (#647)
- Fix typos in `llmenv prune` output text

## [3.0.0] - 2026-07-10

### Major changes since v2.4.1

This release introduces a multi-engine architecture (Crush alongside Claude
Code), a built-in persistent memory system via ICM, automatic context-mode
integration, and a new interactive setup wizard. Full granular changeset in
the rc.1 and rc.2 sections below.

- **Multi-engine support** — llmenv now drives Crush as a second agent engine
  alongside Claude Code. `export`/`hook`/`regenerate` iterate all installed
  adapters. The CrushAdapter renders hooks, MCP servers (stdio/SSE/HTTP), LSP,
  permissions, and skills against Crush's actual schema.
- **ICM Memory System** — Built-in persistent memory with session logging
  (transcript + JSONL file), CLI observability (`llmenv memory stats|list|diff|prune`),
  importance/type annotations, consolidation groundwork, and `SessionStart`/
  `SessionEnd` lifecycle hooks that actually wire memory wake-up and store.
- **Context-mode integration** — Enabling `features.context_mode` auto-wires
  the context-mode plugin: marketplace clone, MCP server, durable data dir,
  and permissions. Supersedes the removed `LLMENV_BASH_BAN`.
- **`llmenv setup` wizard** — Interactive command that scans existing tool
  configs (`~/.claude`, `~/.cursor`), prompts for preferences, and generates a
  validated `config.yaml` with starter `AGENTS.md`.
- **First-class LSP & Skills** — Declare language servers (`name`, `command`,
  `filetypes`, `init_options`, etc.) and skills directly in config or bundles,
  tag-scoped and independent of the plugin model.
- **MCP field parity** — `headers`, `disabled`, `disabled_tools`, and `timeout`
  on MCP server entries.
- **Config validation & observability** — `llmenv doctor` warns on dangling
  bundle dirs, unused marketplace entries, and orphaned `native_permissions`.
  `disabled_engines` skips rendering for named engines. Token-efficiency checks
  in `doctor`, `--compress` export flag.
- **BREAKING:** `session_log` is now a mapping (`{ file, transcript, verbose,
  path, max_content_bytes }`) instead of a path string. The old string form is
  rejected with a migration hint.
- **Removed:** `LLMENV_BASH_BAN` env var; superseded by context-mode.

### Changes since v3.0.0-rc.2

- Forward-merged from 2.4.0: per-hash `CLAUDE_CODE_TMPDIR` temp isolation and
  `CLAUDE_CODE_PLUGIN_CACHE_DIR` durable plugin cache (#630, #632)
- Forward-merged from 2.4.0: `CONTEXT_MODE_DATA_DIR` and other state-directory
  env vars now emit forward-slash paths on all platforms (#497)
- `llmenv doctor` structural validation: dangling bundle directories, unused
  marketplace entries, orphaned `native_permissions` keys (#604)
- CI: trusted publishing to crates.io via OpenID Connect

## [3.0.0-rc.2] - 2026-07-09

### Added

- `llmenv setup` interactive wizard: scans existing tool configurations
  (`~/.claude`, `~/.cursor`), prompts for GitHub repo and bundle organization,
  and generates a validated `config.yaml` with starter `AGENTS.md`. (#561, #575)
- `llmenv setup --rescan`: re-read existing tool configs and refresh the
  enumeration JSON without overwriting config.yaml, AGENTS.md, or bundle
  contents. Composes with `--no-launch` and `--path`. (#576)
- The Claude Code adapter now renders `capabilities.lsp`: entries with an
  `extension_to_language` map (new field, e.g. `{".rs": "rust"}`) render into a
  synthetic skills-directory plugin (`skills/llmenv-lsp/.claude-plugin/plugin.json`),
  which Claude Code auto-loads with no marketplace or install step — its only LSP
  surface is a plugin's `lspServers` manifest key. Entries without the map are
  skipped (with a warning) rather than rendered incorrectly, since the existing
  `filetypes` field (language ids) doesn't reliably convert to Claude's required
  extension-to-language form. (#556)
- `CrushAdapter` hardening: incompatible hook events, `mcp_tool` hooks, and
  non-skill plugin content (`agents/`, `commands/`, `hooks/`) now warn and skip
  instead of hard-erroring the entire render — one unsupported piece no longer
  blocks Crush output altogether. (#543)
- `llmenv doctor` now reports, by name, every hook event that a `PATH`-detected
  adapter can't materialize (e.g. Crush skipping a `PostToolUse` hook), and its
  token-efficiency checks now count a var as set if it's declared in
  `native.claude_code.env`, not only in the live process environment. (#543)
- Top-level `disabled_engines` config list: skip rendering for named engines
  (e.g. `claude_code`, `crush`) even when their binary is on `PATH`. An entry
  that doesn't match any registered engine prints a warning on every
  `export`/`regenerate`/`doctor` run (not just `llmenv validate`). Matching is
  case-insensitive, so `Claude_Code` or `CRUSH` disable the same engines as
  their lowercase form, and the `--engine` flag's own unknown-engine check
  now matches case-insensitively too. (#562, #564)
- Add optional `<!-- llmenv-type: episodic|semantic|procedural -->` HTML-comment marker in
  context chunks to classify stored memories by type. Types persist as ICM memory metadata and
  can be filtered in recall. Configurable default via `default_type` on memory server entries. (#267)
- Add `llmenv memory stats|list|diff|prune` CLI subcommand for ICM store observability. `stats`
  shows record counts, `list` dumps memories for the active scope, `diff` highlights changes
  since the last session snapshot. (#268)
- Add optional `<!-- llmenv-importance: low|medium|high|critical -->` marker to tag memory
  importance at write time. Configurable per-type defaults via `type_importance` map on memory
  server entries. SessionEnd writes now skip duplicate chunks when unchanged. (#269)
- Add `consolidation` config section with `enabled` and `max_rules_per_session` fields.
  Wires a diagnostic consolidation hook into the SessionEnd lifecycle; LLM integration
  deferred. (#271, #595)
- Add three structural validation checks to `llmenv doctor`: warn on dangling bundle
  directories (declared but missing on disk), unused marketplace entries (defined but
  unreferenced), and orphaned `native_permissions` keys (no matching MCP server or
  engine adapter) (#604)

### Changed

- Replace stale Claude Code env var table in `docs/env-vars.md` with a link to the
  [upstream docs](https://code.claude.com/docs/en/env-vars)

### Fixed

- Fix `export`/`regenerate` never actually materializing Crush output: the internal
  materialization step ignored which adapter was passed in and always rendered Claude
  Code's layout, so `crush.json` and `CRUSH_GLOBAL_CONFIG`/`CRUSH_GLOBAL_DATA` were never
  produced even with `crush` on `PATH`. `regenerate` also gained the same per-adapter
  `PATH`-gated loop `export` already had. (#543)
- Fix `CrushAdapter` hard-erroring the *entire* render over a single incompatible hook
  event, `mcp_tool` hook, or plugin with `agents/`/`commands/`/`hooks/` content — one
  unsupported bundle previously blocked Crush output altogether. Incompatible pieces
  are now skipped with a warning naming them; everything Crush can support still
  materializes. (#543)
- Fix `LLMENV_STATE_DIR` (and other configured tool-state relocation vars) getting
  silently overwritten with the wrong adapter's state directory once more than one
  adapter materializes in the same `export`/`regenerate` run — the durable-state
  feature is scoped to tools writing into `CLAUDE_CONFIG_DIR`, so it now only runs
  for the Claude Code adapter instead of once per adapter. (#543)
- Fix unbounded, non-timeout-bounded DNS resolution in the ICM MCP client's SSRF
  guard: `validate_url_production` resolved domain hosts via a plain blocking
  `to_socket_addrs()` call before the 2s `HOOK_TIMEOUT` was ever applied, so a slow
  or failing DNS resolver could hang `llmenv hook-run` — including the per-prompt
  `turn_start` hook — for minutes instead of seconds. Resolution is now bounded by
  the same timeout via a dedicated helper. (#547)
- Fix `CrushAdapter` exporting `CRUSH_GLOBAL_CONFIG` pointing directly at the rendered
  `crush.json` file instead of the directory containing it. Crush's own config loader
  joins `crush.json` onto `CRUSH_GLOBAL_CONFIG` itself, so the file-path value made it
  look for `crush.json/crush.json` and fail to load — `crush` couldn't start with any
  llmenv-managed config. `CRUSH_GLOBAL_CONFIG` now points at the cache directory, matching
  the original design intent. (#551)
- Fix `CrushAdapter` rendering hooks in Claude Code's nested `{matcher, hooks:
  [{type, command, tool}]}` shape instead of Crush's flat `HookConfig` (`{matcher?,
  command}`) — Crush read an empty `command` off the wrapper object and rejected the
  whole config with `hook PreToolUse[0]: command is required`, so no hook (or any
  other capability sharing the render) ever reached Crush. Also ports Claude Code's
  bundle-relative hook-script path resolution (a bare `hooks/foo.sh` in a hook
  `command` resolves against the bundle's directory) into the shared adapter helper
  so Crush benefits from it too — it previously only ran for Claude Code, leaving a
  bundle-authored relative script path broken under Crush. (#551)
- Fix `CrushAdapter` rendering MCP servers, LSP `init_options`, and permissions in
  Claude Code's shapes instead of Crush's actual schema
  ([crush.json schema](https://charm.land/crush.json)), found by auditing the adapter against it: every
  MCP server previously failed to initialize because Crush's required `type` field
  (`stdio`/`sse`/`http`) was either missing (stdio entries) or set to the
  nonexistent value `"remote"` (remote entries) — Crush's MCP client hits an
  `unsupported mcp type` error for anything else. LSP `init_options` was written
  under Claude Code's `initializationOptions` key, so Crush's plain
  `json.Unmarshal` silently dropped it. `permissions.denied_tools`/`default_mode`
  were also dropped — Crush's `PermissionsConfig` has only `allowed_tools`; not a
  security regression (Crush already denies-by-default outside the allow-list),
  but dead output. The full rendered config (all three MCP transports, hooks, LSP,
  permissions) now validates against the real schema with zero violations. (#554)
- Fix the ICM memory backend (`session_start`/`turn_start`/`session_end`) being
  completely non-functional whenever it resolved to loopback or a private-network
  address — the documented common topology (AGENTS.md: "the resolved icm MCP
  endpoint can be a remote `icm serve`"). Four bugs stacked, each masking the next:
  the SSRF guard rejected loopback/private/ULA outright (now split into
  `SsrfPolicy::PublicOnly` vs. `AllowPrivateNetwork`, the latter used by the ICM
  client); the client never sent the `Accept` header MCP's Streamable HTTP
  transport requires (406); the client never performed the MCP `initialize`
  session handshake the transport requires (400 missing session ID); and the
  `SessionEnd` store action never sent the tool's required `topic` field. All four
  fixed together; verified end-to-end against a live ICM server. (#548)
- Fix remaining hardcoded ClaudeCodeAdapter call sites: thread the actual adapter identity through
  `build_and_materialize`, `run_export`, `run_regenerate`, `run_prune`, `run_doctor`,
  `run_throttle_inner`, and `hook_run` instead of assuming Claude Code (#544)
- Fix skill materialization rejecting a `SKILL.md` whose `description` contains a colon (e.g.
  "Triggers on: ..."); `name`/`description` values are now auto-quoted before the strict YAML
  parse so a single malformed-looking skill no longer takes down the whole adapter (#568)
- Fix bundle hook paths in generated `settings.json` referencing the source directory instead
  of the materialized cache directory. Hook paths now resolve against the cache copy via
  two-pass resolution — direct join for clean relative paths, suffix-match against the
  materialized manifest for shell-variable/absolute prefixes — with longest-suffix matching
  and path-boundary checks to prevent ambiguous matches. (#162)
- Fix memory deduplication snapshot being written before the MCP store call completed.
  A transient store failure left the snapshot ahead of reality, causing the next
  `SessionEnd` to skip the store and permanently lose the memory chunk.
- Fix unknown keys under `features:` silently degrading instead of producing a clear
  error; `Features` now rejects unknown fields at parse time. (#602)
- Fix skills with the same name from different bundles colliding in materialization
  after tag filtering; skills are now deduplicated by name, keeping the first
  occurrence. (#600)
- Fix `llmenv doctor` not verifying the context-mode marketplace clone exists when
  `features.context_mode.enabled` is true; now warns if the marketplace hasn't been
  synced yet. (#601)
- Fix example bundle hook matchers using glob patterns (`*.rs`, `*.py`, `*.ps1`)
  instead of valid tool-name regexes; corrected to `^(Edit|Write|MultiEdit)$`. (#605)
- Fix example bundle commands containing unsubstituted template placeholders and
  incorrect ICM CLI usage instead of ICM MCP calls. (#606)
- Fix example `fyi` app: race-condition in `mkdir` lock in `refresh.sh`, missing
  `TypeError` in toggle handler, missing `Origin` check on POST endpoints, and
  phantom `topFocus` in `SPEC.md`. (#607)
- Fix example plugin augmentation: pinned slop-scan wrapper and cryptic dangling
  bullet in `general.md`. (#608)

## [3.0.0-rc.1] - 2026-07-01

### Added

- `features.context_mode` built-in feature: enabling `features.context_mode.enabled`
  auto-wires the context-mode plugin (marketplace, plugin, durable
  `CONTEXT_MODE_DATA_DIR`, and MCP permission) — the token-efficiency counterpart
  to the built-in ICM memory feature. Warns when the plugin is also declared manually
  in a plugin-collection. (#490)
- ICM-transcript session logging: llmenv records scope + lifecycle (and, with
  `session_log.verbose`, prompts and tool use) into ICM's transcript store via
  the ICM MCP, discoverable by `llmenv-tag:` / `llmenv-bundle:` tokens and
  project. A local JSONL `file` sink mirrors the same stream, independent of
  ICM reachability. (#382)
- The Claude Code adapter now auto-registers `SessionStart`/`SessionEnd` hooks
  running `llmenv hook-run`, fixing a gap where the ICM memory wake-up/store
  dispatcher existed but was never wired into generated `settings.json` —
  memory wake-up/store now actually fires. Continuous per-prompt recall
  (`turn_start`) is still unwired; tracked in #499. (#382)
- Multi-engine foundation for a second agent engine (Crush): `export`, `hook`,
  and `regenerate` now iterate a registry of engine adapters, materializing each
  into its own per-engine cache subtree and skipping any whose binary isn't on
  `PATH`. Claude-only users see no behavior change. Groundwork for the Crush
  adapter (#506); no Crush support ships yet. (#502)
- Add first-class `lsp:` capability: declare language servers (`name`, `when`,
  `command`, `args`, `env`, `disabled`, `filetypes`, `root_markers`,
  `init_options`, `timeout`) at the top level or inside a bundle, tag-scoped like
  `mcp`. Engines with no LSP concept (Claude Code) silently ignore them. (#503)
- Add first-class `skills:` capability, decoupled from plugins: declare a skill
  (`name`, `path`, `when`) directly in config or a bundle, tag-scoped, validated
  with the same frontmatter and path checks as plugin-bundled skills. (#504)
- Add MCP server field parity: `headers`, `disabled`, `disabled_tools`, and
  `timeout` on MCP server entries. All optional — existing configs parse
  unchanged. (#505)
- `CrushAdapter`: Crush is now a supported engine. `export`/`hook`/`regenerate`
  render `crush.json` when `crush` is on `PATH`. What maps: permissions →
  `allowed_tools`/`denied_tools` (lossy, fail-closed — `ask` rules collapse to
  `denied_tools`, never silently allowed; Crush has no ask concept); hooks →
  `PreToolUse` only (`mcp_tool`-kind hooks and unsupported hook events hard-error
  with an actionable message); MCP servers (including `headers`, `disabled_tools`,
  `timeout`); LSP servers → `lsp.<name>`; first-class skills and plugin-projected
  skills → `options.skills_paths`. Non-skill plugin content (`agents/`, `commands/`)
  hard-errors naming the offending plugin. `native.crush` / `native_permissions.crush`
  / `native_hooks.crush` / `native_mcp.crush` merge verbatim — provider/model config
  lives here until first-class provider config ships (#508). Docs in #507. (#506)

### Changed

- **Behavior change (dual-engine export):** `export`, `hook`, and `regenerate`
  now iterate all registered engine adapters. If `crush` is on `PATH`, a new
  `crush/` cache subtree is materialized and `CRUSH_GLOBAL_CONFIG` /
  `CRUSH_GLOBAL_DATA` are exported alongside the existing Claude Code env vars.
  Claude-only users (no `crush` binary on PATH) see no change. (#502, #506)
- **BREAKING:** `session_log` is now a mapping (`{ file, transcript, verbose,
  path, max_content_bytes }`), not a path string. ICM transcript logging is on
  by default. The pre-3.0 `session_log: "<path>"` form is rejected with a
  migration hint. (#382)

### Removed

- `LLMENV_BASH_BAN` env var and its deny-rule wiring. It was broken as shipped
  (read from llmenv's process env before bundle-declared values landed) and is
  superseded by the built-in context-mode feature. (#490, removes #464)

### Fixed

- Fix marketplace and plugin-payload sync returning a broken clone with unstable cache key when
  git HEAD cannot be resolved. Now detects and errors on broken clones (after clone or pull),
  cleans up the corrupted directory, and forces a fresh clone on retry (#537)

<!-- next-url -->
[Unreleased]: https://github.com/phaedrus1992/llmenv/compare/v3.7.0...HEAD
[3.7.0]: https://github.com/phaedrus1992/llmenv/compare/v3.6.1...v3.7.0
[3.6.1]: https://github.com/phaedrus1992/llmenv/compare/v3.6.0...v3.6.1
[3.6.0]: https://github.com/phaedrus1992/llmenv/compare/v3.5.1...v3.6.0
[3.5.1]: https://github.com/phaedrus1992/llmenv/compare/v3.5.0...v3.5.1
[3.5.0]: https://github.com/phaedrus1992/llmenv/compare/v3.4.0...v3.5.0
[3.4.0]: https://github.com/phaedrus1992/llmenv/compare/v3.3.0...v3.4.0
[3.3.0]: https://github.com/phaedrus1992/llmenv/compare/v3.2.0...v3.3.0
[3.2.0]: https://github.com/phaedrus1992/llmenv/compare/v3.1.0...v3.2.0
[3.1.0]: https://github.com/phaedrus1992/llmenv/compare/v3.0.0...v3.1.0
[3.0.0]: https://github.com/phaedrus1992/llmenv/compare/v3.0.0-rc.2...v3.0.0
[3.0.0-rc.2]: https://github.com/phaedrus1992/llmenv/compare/v3.0.0-rc.1...v3.0.0-rc.2
[3.0.0-rc.1]: https://github.com/phaedrus1992/llmenv/compare/v2.3.0...v3.0.0-rc.1
