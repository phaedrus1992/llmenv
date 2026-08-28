# Local API-proxy mode for `llmenv launch claude_code`

Tracked by: #1289

## Background

`llmenv launch <engine>` (#1056, shipped) resolves the environment and spawns
the target engine as a supervised child process. Claude Code sends every
request to `api.anthropic.com` with an injected system prompt the user cannot
edit. Claude Code already respects `ANTHROPIC_BASE_URL`, so llmenv can start a
local HTTP proxy for the session, point the child at it, and rewrite outbound
requests before they leave the machine — no TLS interception needed.

Prior art referenced in #1289: the `ANTHROPIC_BASE_URL` → local proxy →
rewrite → forward pattern (aihero.dev, `zen-logic/claude-proxy`,
docs.bswen.com). Rejected alternative: `Piebald-AI/tweakcc`, which patches
Claude Code's bundled `cli.js` directly — more invasive and brittle across
Claude Code releases than a network-layer proxy.

A second real-world driver surfaced during design: the `omlx` project's
"Lserm proxy" (`phaedrus1992/omlx` issue #1) already does a narrower version
of this — it detects Claude Code's auto-mode safety-classifier request by
matching a billing header **and** system-prompt text together (the header
alone also matches normal turns), and only then sets `thinking` to disabled,
but only when the request carries no explicit `thinking` field already. This
proxy's rule engine is designed to express that case directly, not just
system-prompt trimming.

## Non-goals

- Rewriting the **response** stream. Claude Code needs live token streaming;
  the issue and the classifier use case both only need outbound-request
  rewriting.
- A generic per-adapter proxy trait. Only Claude Code is wired up now; the
  module boundary keeps the door open for a second engine later, but nothing
  is built for that today (YAGNI).
- TLS interception / MITM. Not needed — `ANTHROPIC_BASE_URL` redirection is
  sufficient and matches how Claude Code already supports corporate gateways.

## Architecture

New module `src/launch/proxy.rs`, structured like the existing
`src/launch/socket.rs` (which already runs a per-session Unix socket with the
same start-before-spawn / teardown-on-exit lifecycle):

- `run_launch` starts the proxy (if `features.launch_proxy.enabled`) before
  spawning the child: bind `TcpListener` on `127.0.0.1:0` (ephemeral port),
  spawn a `tokio` task serving it via `hyper` + `hyper-util` (already
  transitive dependencies of `reqwest`, promoted to direct deps — no new
  dependency tree).
- The child's env gets `ANTHROPIC_BASE_URL` set to the local proxy's address.
  If the resolved env already had an `ANTHROPIC_BASE_URL` (e.g. a corporate
  gateway), the proxy captures that as its own upstream target instead of
  `https://api.anthropic.com` — it chains through the existing override
  rather than clobbering it.
- Teardown mirrors `SocketCleanup`'s `Drop` pattern in `socket.rs`: the
  serving task is aborted when the child exits, at the same point
  `spawn_and_supervise` already tears down the notice socket.
- The proxy forwards using `reqwest::Client` (existing dependency, rustls
  already enabled) — one HTTP request in, one out, streamed back via
  `.bytes_stream()` so Claude Code's SSE streaming isn't buffered or delayed.

## Config schema

`features.launch_proxy: Option<LaunchProxy>` in
`crates/llmenv-config/src/schema.rs`, following the `TaskTracker` precedent
(`enabled: bool`, off by default — this rewrites live API traffic and prompt
content, so opt-in is the safer default, unlike `repeat_detect`/`cd_guard`
which are on by default for lower-stakes guardrails):

```yaml
features:
  launch_proxy:
    enabled: true
    rules:
      - when:
          - { target: header, name: "x-billing-header", check: present }
          - { target: body, path: "system[0].text", check: { matches: "security monitor", regex: false } }
          - { target: body, path: "thinking", check: missing }
        target: body
        path: "thinking"
        op: { set: { type: "disabled" } }
      - target: body
        path: "system[0].text"
        op: { strip: { pattern: "verbose boilerplate.*", regex: true } }
```

### Rule shape

```
Rule {
    when: Vec<Condition>,   // empty = always fires; multiple conditions are AND-ed
    target: Header | Body,
    path: String,           // JSON-path-lite (dot + bracket index); ignored for Header target, name used instead
    name: String,           // header name; used only when target == Header
    op: Set(json value) | Remove | Strip { pattern: String, regex: bool },
}

Condition {
    target: Header | Body,
    path: Option<String>,   // Body only; omitted = whole serialized body
    name: Option<String>,   // Header only
    check: Missing | Present | Equals(value) | Matches { pattern: String, regex: bool },
}
```

### Op semantics

- **`Set`** upserts: creates the path and any missing intermediate objects.
  This is required for the classifier case above — Claude Code's classifier
  request never sends a `thinking` field at all, so `Set` must be able to add
  it, not just overwrite an existing one.
- **`Remove`** and **`Strip`** are no-op-if-the-path-is-absent — see Error
  handling below.

### Condition semantics

- `Missing` / `Present` check path (body) or header-name existence.
- `Equals` checks an exact match against a parsed JSON value (body) or header
  value string (header).
- `Matches` runs a literal-substring or regex match: on the header's raw
  value string, or on the body — either the value at `path` (stringified) or
  the whole serialized request body when `path` is omitted.
- All conditions on a rule are AND-ed; an empty `when` list means the rule
  always fires.
- Header name matching is case-insensitive, matching HTTP's own semantics and
  `hyper`'s `HeaderMap`, which already normalizes lookups this way.

## Error handling

- **Rule application misses** (a `Remove`/`Strip` target path that doesn't
  exist, a `when` condition referencing a header that isn't present) are
  non-fatal: skip the rule, `tracing::warn!` with the rule's identity, and
  forward the rest of the request as configured. This is a deliberate
  fail-open choice — a Claude Code update that changes the request shape
  should degrade to "prompt not trimmed" rather than break every launch
  session.
- **Upstream network errors** (unreachable, TLS failure) are not silently
  swallowed — the proxy returns a real HTTP 502 to Claude Code, which
  surfaces through Claude Code's own error handling like any other network
  failure.
- **Config validation** (malformed JSON-path syntax, invalid regex) happens
  at config-load time alongside the rest of `Features` deserialization, so a
  broken rule fails `llmenv doctor` / `llmenv export` up front instead of
  failing silently mid-session.

## Testing

- Integration tests use `wiremock` (already a dev-dependency) as a stand-in
  for `api.anthropic.com`, asserting the proxy forwards the rewritten request
  wiremock expects and streams wiremock's response back unmodified.
- Reuses the `tests/fixtures/fake_engine.sh` pattern from the existing
  `launch` integration tests (`tests/launch.rs`) so proxy tests don't depend
  on a real `claude` binary being installed in CI — `fake_engine.sh` dumps
  its environment (confirming `ANTHROPIC_BASE_URL` was rewritten to the
  local proxy) and can make a real HTTP call against the proxy to exercise
  the rewrite path end-to-end.

## Acceptance criteria

- `features.launch_proxy` config schema lands in `llmenv-config`, validated
  at load time (bad JSON-path/regex rejected with a clear error).
- `llmenv launch claude_code` starts the proxy when
  `features.launch_proxy.enabled` is true, sets `ANTHROPIC_BASE_URL` to it,
  and tears it down on child exit.
- An existing `ANTHROPIC_BASE_URL` in the resolved env is chained through as
  the proxy's upstream, not clobbered.
- `Set` rules upsert (create missing paths); `Remove`/`Strip` are
  no-op-if-absent with a warning logged.
- Response bodies stream through unmodified (no buffering, no rewriting).
- Rule application failures never abort the request; upstream network
  failures surface as a real error to Claude Code.
- Integration tests cover: a `Set` rule creating a missing `thinking` field
  under the AND-combined `when` conditions from the classifier use case, a
  `Strip` rule trimming system-prompt text, and the existing-`ANTHROPIC_BASE_URL`
  chaining behavior.
