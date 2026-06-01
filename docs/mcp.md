# MCP Servers and the Memory Backend

llmenv treats MCP (Model Context Protocol) servers as a first-class config
concept. Servers are declared once under `mcp:`, attached to scopes via tags
(the same selection model as bundles), and rendered by each adapter into its
agent-native config (for Claude Code: `mcp.json`).

llmenv's own memory backend is configured separately under `memory:`. It is a
single networked service, not a generic MCP entry — its implementation (ICM,
Infinite Context Memory) is deliberately hidden behind the `memory:` vocabulary.

For the config-field reference, see
[Configuration → `mcp:`](configuration.md#mcp) and
[`memory:`](configuration.md#memory). This page covers the runtime model: the
selection mechanism, the memory topology, the security model, and the
tag-scoped-memory env var contract.

## Selection model

Every `mcp` entry carries `tags`. A server is included in the materialized
output when **any** of its tags is present in the active tag set for the current
environment — identical to how `bundle` entries fire. Scopes (network/host/
user/project) emit the tags; the intersection decides what is active.

```yaml
mcp:
  - name: playwright
    tags: [base]            # active whenever the `base` tag is
    command: npx
    args: ["-y", "@playwright/mcp@latest"]
```

## Server kinds

A static server is either **stdio** (a local launch command) or **remote**
(an HTTP/SSE URL):

```yaml
mcp:
  - name: playwright
    tags: [base]
    type: stdio             # default
    command: npx
    args: ["-y", "@playwright/mcp@latest"]
    env:
      DISPLAY: ":0"

  - name: weather
    tags: [base]
    type: http              # http | sse
    url: "https://weather.example.com/mcp"
```

## Memory backend (`memory:`)

The memory backend is a single service that one host runs locally while every
host — including the one running it — reaches over the network. The daemon
(`icm serve`) is stdio-only, so on the server host llmenv wraps it in
`mcp-proxy` to expose it on a TCP port; agents everywhere connect to that port.

- On the **designated server host**, llmenv launches a local `mcp-proxy` bound
  to `0.0.0.0:<port>` that bridges the stdio daemon onto the network.
- **Every agent**, on every host, is configured with a **remote** client
  pointed at the server host's address: `http://<addr>:<port>`.

The server host needs [`mcp-proxy`](https://github.com/sparfenyuk/mcp-proxy)
available — it's the stdio↔network bridge that exposes the `icm serve` daemon on
a TCP port. llmenv resolves it one of two ways:

- if `mcp-proxy` is on `PATH`, it's run directly (e.g. `uv tool install
  mcp-proxy`, `pipx install mcp-proxy`, or any install that lands it on `PATH`);
- otherwise llmenv runs it on demand via [`uvx`](https://docs.astral.sh/uv/)
  (`uvx mcp-proxy`), which fetches and caches it without a persistent install.

So the server host needs **either** `mcp-proxy` **or** `uvx` installed. If
neither is present, `llmenv export` fails with an error telling you to install
one or remove the `memory:` block. Client hosts need neither — they only open an
HTTP connection to the server.

The server host's address comes from the top-level `host:` table:

```yaml
host:
  fixed:
    addr: "fixed.local"      # IP or resolvable hostname

memory:
  server_host: fixed         # key into the `host:` table
  port: 7878
  tags: [base]               # activates the backend (same model as bundles)
  default_topics: ["context-{project}", preferences]
```

### How the topology is resolved

1. Scopes are evaluated against the current environment; the active host-scope
   ids and the active tag set are computed.
2. If any of `memory.tags` is active, the backend is selected: every agent gets
   a remote client at `http://<addr>:<port>` built from the host-table address.
3. If this host matches `server_host` (its id is among the matched host scopes),
   the CLI also launches the local `mcp-proxy` bound to `0.0.0.0:<port>`.

### Placing a host on a network manually

Network auto-detection (gateway MAC, SSID, CIDR) doesn't always work — a VPN, a
captive network, or an unrecognized gateway can all leave the network scope
unmatched, so the memory tag never activates and clients can't find the server.

Because the memory backend activates on **any** active tag, you can attach its
tag to a **host scope** instead of relying on the network scope. A host scope
matches by hostname (always reliable) and can emit the same tag the network
scope would have:

```yaml
scope:
  network:
    - id: home
      match: { gateway_mac: "aa:bb:cc:dd:ee:ff" }
      tags: [home]            # fires when the gateway is detected
  host:
    - id: laptop
      match: { hostname: laptop }
      tags: [home]            # always fires on this host — manual fallback

memory:
  server_host: fixed
  port: 7878
  tags: [home]               # active via either route
```

With this, `laptop` always emits `home`, so its agents always get the memory
client URL — even when the network can't be auto-detected. The host that
matches `server_host` additionally launches the local proxy.

## Security considerations

The memory backend has no transport security and no access control:

- The proxy binds to `0.0.0.0:<port>` (all interfaces), and every client
  connects over plaintext **`http://`** — there is no TLS, so anything stored
  in memory crosses the wire in the clear.
- There is no authentication. Any host that can reach `<addr>:<port>` can read
  and write the memory backend. Access is gated **only** by network reachability
  — that is the trust model.

Deploy it only on a network you trust (home LAN, a private VPN, a firewalled
subnet). Do not expose the port to the public internet, and do not point the
`host:` `addr` at a publicly routable address. If you need to bridge hosts
across an untrusted network, tunnel the port over SSH or a VPN rather than
opening it directly.

## Diagnostics

List the MCP servers that resolve for the current environment:

```bash
llmenv mcp-ls        # alias: llmenv mcps
```

`llmenv doctor` flags orphaned MCP config:

- a server (or the memory backend) whose tags are never emitted by any scope
  (it can never activate),
- a memory `server_host` with no entry in the `host:` table.

```bash
llmenv doctor
```

## Troubleshooting

### Wrong role on a host

Which host runs the memory server keys off whether the current host matches a
host-scope whose id equals `server_host`. Verify the active scopes and tags:

```bash
llmenv scope-ls
llmenv tag-ls
```

### Client can't reach the server

Confirm the `host:` entry resolves and the port is open on the server host:

```bash
nc -vz fixed.local 7878
```

### Server not activating

The server only renders when one of its tags is active. Check that a scope in
the current environment emits a matching tag (`llmenv tag-ls`).

## Tag-scoped memory and the env var contract

llmenv bridges the active scope into memory so that context can be stored once
and recalled in *any* environment sharing the same tags — even across different
projects. Two mechanisms carry this:

### `LLMENV_ICM_CONTEXT`

On every `llmenv export`, llmenv emits `LLMENV_ICM_CONTEXT`: a markdown chunk
encoding the active tags, the firing bundles, and (when a project marker is
active) the project name and description. Its shape:

```text
## llmenv context
Active tags: `office`, `rust`
Bundles: `base`, `office-tools`

Store scope-specific memory under keyword `llmenv-tag:<tag>` (per tag)
or `llmenv-bundle:<bundle>` (per bundle) so it is retrievable across
projects. On each turn, llmenv auto-recalls memory under these tags'
`llmenv-tag:<tag>` and bundles' `llmenv-bundle:<bundle>` keywords
across all projects.

**Project:** MyApp — Customer-facing API
```

Agents read this to learn which tags are live and how to key memory so it
follows the tag rather than the project.

### Keyword convention

- `llmenv-tag:<tag>` — memory keyed to a tag. Stored once, retrieved in any
  environment where that tag is active. The TurnStart hook recalls this keyword
  automatically across all projects (see [Lifecycle hooks](#lifecycle-hooks)).
- `llmenv-bundle:<bundle>` — memory keyed to a bundle, retrieved whenever that
  bundle fires. The TurnStart hook recalls this keyword automatically across all
  projects (parallel to `llmenv-tag:<tag>`). See [Lifecycle hooks](#lifecycle-hooks).

### Lifecycle hooks

llmenv provides engine-neutral lifecycle hooks (`hook-run` command) that
automatically activate when a memory backend is configured and active. These hooks
run in response to three neutral events:

- **SessionStart** — `hook-run session_start` injects the session wake-up pack
  (`icm_wake_up`) containing your critical memories (by importance and recency)
- **TurnStart** — `hook-run turn_start` injects recalled context at the start of
  each agent turn (`icm_memory_recall`). It issues a project-scoped recall for
  the active tags, then one **project-unfiltered** recall per active tag keyed on
  `llmenv-tag:<tag>`, and one **project-unfiltered** recall per active bundle
  keyed on `llmenv-bundle:<bundle>` — so memory stored under a tag or bundle in
  one project surfaces when the same tag or bundle activates in another
- **SessionEnd** — `hook-run session_end` stores the active scope context
  (`icm_memory_store`) when the session closes

Each hook talks to the memory backend over MCP. Failures degrade gracefully: a
missing or unreachable backend logs a warning and exits cleanly (exit code 0) so
hooks never block the agent. See [`docs/commands.md`](commands.md#hook-run) for
details.

### SessionStart injection

The Claude Code adapter registers a `SessionStart` hook. Alongside
`check-stale` (drift detection), llmenv records the active tag/bundle set to a
`0600` state file (`icm.json` in the state dir) so the hook can surface the
keyword convention to the agent at startup. The `hook-run session_start` command
is also invoked at session start to inject ICM memory.

### Related introspection vars

`LLMENV_ICM_CONTEXT` is one of several vars `export` emits. The full set —
`LLMENV_ACTIVE_SCOPES`, `LLMENV_ACTIVE_TAGS`, `LLMENV_ACTIVE_BUNDLES`,
`LLMENV_ACTIVE_PROJECT`, `LLMENV_PROJECT_ROOT`, `LLMENV_ICM_CONTEXT` — is
documented in the [README](../README.md#introspection-environment-variables)
and [Concepts](concepts.md#introspection).
