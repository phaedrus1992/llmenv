# Multi-Host Memory Topology

Run llmenv's memory backend (ICM) differently on different hosts: broadcast on
one machine so trusted peers can share it, keep it loopback-only on another
that shouldn't expose even an unauthenticated LAN service.

## Config

```yaml
# ~/.config/llmenv/config.yaml

scope:
  host:
    - id: laptop
      match: { hostname: "laptop.local" }
      tags: [host-laptop, personal, work]
    - id: build-server
      match: { hostname: "build-server.internal" }
      tags: [host-build-server, server]

host:
  laptop:
    addr: "laptop.local"
  build-server:
    addr: "build-server.internal"

features:
  memory:
    # Laptop: on a trusted home/office LAN, broadcasts so other trusted
    # devices on that network can share the same memory context.
    - server_host: laptop
      listen_host: 0.0.0.0
      port: 9092
      when: [host-laptop, personal, work]

    # Build server: not on a fully trusted LAN. Loopback-only means the
    # backend is usable by agent sessions running on that host, but nothing
    # else on its network can reach the plaintext, unauthenticated port.
    - server_host: build-server
      listen_host: 127.0.0.1
      port: 9092
      when: [host-build-server]
```

## How it works

- Each host scope emits its own tag (`host-laptop`, `host-build-server`), so
  only one `features.memory` entry is ever active on a given machine — the
  resolver would error if both matched the same scope simultaneously, but
  they can't, since the host scopes are mutually exclusive by hostname.
- `listen_host` controls which interfaces the proxy binds, independent of
  `server_host`/`addr` (which is what *other* hosts use to reach it). `0.0.0.0`
  means any device that can route to that host can connect; `127.0.0.1` means
  only processes on that same machine can.
- Both entries can point at the same `port` — they never run at the same
  time, since they're gated by disjoint host scopes.

See [MCP & Memory → Security considerations](../mcp.md#security-considerations)
before using `0.0.0.0` on anything but a network you actually trust — the
memory backend has no auth and no TLS; reachability is the entire access
model.

## Verify

```bash
# On each host:
llmenv context
# shows which features.memory entry is active and its resolved listen_host
```
