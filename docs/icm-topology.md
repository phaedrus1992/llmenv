# MCP Servers and the Memory Backend

llmenv treats MCP (Model Context Protocol) servers as a first-class config
concept. Servers are declared once under `mcp:`, attached to scopes via tags
(the same selection model as bundles), and rendered by each adapter into its
agent-native config (for Claude Code: `mcp.json`).

llmenv's own memory backend is configured separately under `memory:`. It is a
single networked service, not a generic MCP entry — its implementation (ICM) is
deliberately hidden behind the `memory:` vocabulary.

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
