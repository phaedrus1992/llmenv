<!-- markdownlint-disable MD013 -- entries are one dense bullet per change, not wrapped prose -->

# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

<!-- 4.0 next-header -->

## [Unreleased] - ReleaseDate

4.0 is a small major so far. There is one breaking change — `session_log`'s boolean shape (`file: true`, `verbose: true`) is no longer translated to the per-sink mapping behind the scenes, and is now a parse error that names its replacement (#744). Alongside it, one restriction is loosened: `native.claude_code.permissions` is accepted rather than rejected, layering additively so the catch-all can carry Claude-Code-only permission keys llmenv doesn't model — without giving a `native:` block a way to weaken what the renderer produced (#750).

Everything shipping on the 3.x line is inherited; those entries live in `CHANGELOG-3.md` (Version 3.x on the docs site).

### Removed

- **Breaking:** the boolean `session_log` shape is gone. `session_log: { file: true, transcript: false, verbose: true }` parsed until now by being translated to the per-sink form behind the scenes; it is rejected outright in 4.0. Each sink is a mapping — `file: { enabled, level }`, `transcript: { enabled, level }` — and `verbose: true` becomes `level: debug` on whichever sink should capture prompts and tool calls, which means the two sinks can now differ. The parse error names the replacement; it does not rewrite your config. See [Configuration](https://phaedrus1992.github.io/llmenv/docs/configuration#session_log) (#744)

### Added

- `llmenv launch <engine>` resolves the environment the way `export` does and then runs the engine (`claude`, `crush`, or `opencode`, by binary name or engine id) as a supervised child process — inherited stdio, the resolved environment layered on top, and an exit code mirroring the engine's (`128 + signum` when it dies by signal). SIGINT/SIGTERM/SIGHUP are ignored by `llmenv` itself so it can't exit ahead of the engine and orphan it mid-shutdown. No shell integration required, so it behaves the same from an interactive shell, a script, CI, or an IDE task; `export` and `hook` stay available for callers that only want the variables. Unix only. See [Commands](https://phaedrus1992.github.io/llmenv/docs/commands#launch) (#1056)
- `llmenv launch` accepts `--scope`, `--tag`, and `--compress`, which mean exactly what they do for `export` — including the warning when a requested scope isn't active. `launch` always resolved the scopes the working directory and environment made active, so anyone using `--scope` to pick between environments could do it with `export` but not with the command that supersedes it. The flags may appear either side of the engine name; everything after `--` is still the engine's, so an engine with its own `--scope` is reachable there. See [Commands](https://phaedrus1992.github.io/llmenv/docs/commands#launch) (#1384)
- `native.claude_code.permissions` is accepted instead of rejected, making the catch-all the escape hatch for Claude-Code-only permission keys llmenv doesn't model (`additionalDirectories`, `disableBypassPermissionsMode`, and whatever ships next) without waiting on a neutral-schema field. It's safe to accept because the merge is additive: `allow`/`ask`/`deny` append to what was rendered rather than replacing it, `deny > ask > allow` authority is re-applied afterwards, rule strings get the same `Write` → `Edit` normalization as `native_permissions`, and every other key overwrites. `defaultMode` is rejected here — it's modeled as `capabilities.permissions.default_mode`, and allowing it would let anything that can author a `native:` block set `bypassPermissions` and switch the permission system off. A fragment can tighten permissions or add unmodeled keys; it cannot loosen what the renderer produced — an omitted or `null` `deny` leaves the rendered one intact. `native.claude_code.hooks` still hard-errors, since an array of matcher groups has no unambiguous additive merge. See [Engines](https://phaedrus1992.github.io/llmenv/docs/engines#nativeclaude_codepermissions) (#750)

### Fixed

- `llmenv login`, `llmenv setup`'s engine handoff, and `llmenv edit` supervise their child the way `launch` does, instead of waiting under the default signal disposition. A signal aimed at `llmenv` alone — a supervisor, a script, `kill <pid>` — used to kill it and leave the child running: `claude auth login` writing a credential into a temp directory nothing would read, or a full-screen editor with the shell drawing a prompt over it. llmenv now keeps waiting and reports the child's own status. A terminal Ctrl-C still reaches the child directly, since the terminal delivers it to the whole foreground process group. See [Commands](https://phaedrus1992.github.io/llmenv/docs/commands#edit) (#1385)
- `llmenv hook-run --engine <unknown>` fails, naming the valid engine ids, instead of running the hook against whatever adapter the environment looked like. The fallback announced itself only through a `warn!`, which llmenv's ERROR-only default log filter discards, so a typo'd or stale `--engine` silently read a different engine's config. Omitting the flag is unchanged. See [Commands](https://phaedrus1992.github.io/llmenv/docs/commands#hook-run) (#1386)
- `llmenv launch` forwards SIGTERM and SIGHUP to the supervised engine instead of swallowing them. Ignoring every signal was right for a terminal Ctrl-C — the whole foreground process group already gets it — but wrong whenever a supervisor targets `llmenv`'s pid alone (`docker stop` signalling PID 1, systemd `KillMode=mixed`, a CI runner or IDE task doing `kill <pid>`): the engine never learned to shut down and nothing exited until that caller's SIGKILL deadline. SIGINT is still deliberately not forwarded, since the engine already has its own copy and a second one reads as a double Ctrl-C, which many agents treat as "force quit". `launch` still never exits on its own account, so the status you get is always the engine's. See [Commands](https://phaedrus1992.github.io/llmenv/docs/commands#launch) (#1383)
- Engine detection resolves `PATH` directly instead of shelling out to `which`, so an installed engine no longer looks missing on an image that ships without `which` — routine for distroless and minimal containers. Previously both "not installed" and "couldn't run `which`" produced the same answer, which `llmenv launch` reported as a flat "not found on PATH — install it" for an engine that was present and runnable (#1382)

### Security

- The `mcp-proxy`/`uvx` lookup no longer honours an empty `PATH` entry as "the current directory", so an executable named `mcp-proxy` or `uvx` sitting in whatever directory `llmenv` was run from can't be spawned in place of the real one. A leading, trailing, or doubled `:` in `PATH` produces such an entry, and this lookup was a second copy of llmenv's `PATH` resolver that never picked up the guard the engine-detection copy got — so `llmenv doctor` and the proxy spawn could also reach opposite conclusions about the same binary. There is one resolver now (#1390)

<!-- next-url -->
[Unreleased]: https://github.com/phaedrus1992/llmenv/compare/v3.11.0...HEAD
