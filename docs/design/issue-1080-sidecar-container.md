# Issue #1080 — Sidecar container for sandboxed agentic development

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/1080
- **Milestone:** v4.0.0
- **Type:** Design only (per the issue's own scope) — implementation is follow-up issues, not this doc.
- **Depends on:** #1056 (`llmenv launch <engine>`), shipped.

## Goal

Give `llmenv launch <engine>` a container boundary, so an agent's failure
modes — a bad delete, a force-push, an exfiltrated token — land in a
throwaway container instead of the host. Today `launch` execs the engine
directly on the host with the user's full privileges; this design adds an
opt-in sandboxed exec path with the same resolve-then-exec contract.

## Recommendation summary

| Question | Recommendation |
|---|---|
| Runtime | Docker and Podman both supported, auto-detected; colima (Docker-compatible socket) needs no special-casing beyond the Docker path. |
| Credentials | `icebreaker` (`windowlickers/icebreaker`) run as a host-side subprocess, sealed-token proxy — not vendored as a library. |
| Config surface | `features.sandbox` block (project default) plus `--container`/`--no-container` on `llmenv launch` (per-invocation override). |
| Base image | llmenv ships one minimal default image; `features.sandbox.image` overrides it. |
| Exec model | Ephemeral — `docker/podman run --rm` per `launch` invocation, wrapped by the existing `RelaunchCap` supervision. |
| Git push | Forward `SSH_AUTH_SOCK` read-only; copy `user.name`/`user.email` from the host's `~/.gitconfig`. |

## Architecture

`launch`'s existing shape is: resolve the environment (`crate::cli::resolve_env`),
then spawn the engine binary as a supervised child (`src/launch/mod.rs`). This
design changes only the exec target when the sandbox is active — the
resolve step, the notice-socket channel (`src/launch/socket.rs`), and the
`RelaunchCap` crash-restart loop are unchanged; the child becomes
`docker run --rm <flags> <image> <engine> <args>` instead of `<engine>
<args>` directly.

Runtime selection: `runtime: auto` probes `PATH` for `podman` first, then
`docker` (colima's Docker CLI shim satisfies the `docker` probe — no
separate branch needed). `runtime: docker` / `runtime: podman` force one.

## What crosses the boundary

1. **Project working tree** — read-write bind mount at a fixed in-container
   path (e.g. `/workspace`), matching the devcontainer convention.
2. **Resolved config/env** — copied in at container start the same way
   `launch` already assembles it for the host process today. Not a live
   mount of `~/.config/llmenv` — the container gets the materialized
   result, not the source config tree.
3. **ICM** — the container talks to a host-side `icm serve` over the
   already-supported remote MCP endpoint (the resolver in
   `crates/llmenv-mcp`). ICM's sqlite store never crosses the boundary;
   this also means ICM keeps working if the container has no persistent
   volume at all.
4. **`SSH_AUTH_SOCK`** — read-only mount, forwarding the host's running
   SSH agent so the container can push without ever holding a private key.
5. **Nothing else by default.** No host filesystem beyond the project tree,
   no host network beyond what `docker run` grants, no credentials beyond
   the icebreaker-sealed token and the forwarded agent socket.

## Credentials: icebreaker

[`windowlickers/icebreaker`](https://github.com/windowlickers/icebreaker)
(Apache-2.0, pinned for this review at commit `ec6bd50`) is a Rust
sealed-token proxy: a client sends an encrypted `X-Tokenizer-Token` header,
the proxy decrypts it, injects the real `Authorization` header, forwards to
the upstream API, and scans the response for leaked credentials before
returning it. Verified during this design pass, not assumed:

- **Crypto is sound.** `crypto_box` (X25519 + XChaCha20-Poly1305) for the
  sealed token, `subtle` for constant-time comparison — the same
  constant-time-compare discipline llmenv's own `launch_proxy` peer-auth
  token was hardened to use (#1632/#1640). `zeroize`/`secrecy` guard the
  decrypted secret in memory.
- **It does what it claims.** `crates/icebreaker-proxy/src/middleware/token_injection.rs`
  performs the injection; `middleware/response_scan.rs` (1,332 lines, with
  its own `tests/response_scan_integration.rs`) does the leak-scan on the
  way back. Real integration tests cover mTLS and AWS SigV4 signing too.
- **One disclosed issue rules out one subsystem.** icebreaker's own
  `crates/RUST_AUDIT.md` self-audit lists a P0: the SSO module's error
  responses leak configured hostnames, redirect URIs, and raw upstream
  OAuth bodies. This design does not use `icebreaker-sso` — llmenv already
  owns credential resolution and only needs the core proxy/injection path,
  so that module and its bug are out of the picture entirely.
- **Maturity is genuinely thin.** Zero stars, zero forks, marked as a
  mirror, single contributor visible in the history available. The
  *engineering* (crate split, real crypto, real tests, a self-published
  audit doc) is solid; the *track record* is not. Treat it as young
  infrastructure to pin and re-evaluate, not as a battle-tested dependency.

**Integration shape:** run `icebreaker serve` as a host-side subprocess
llmenv starts and tears down around the `launch` session, the same
lifecycle pattern `launch_proxy` already uses. Do not add
`icebreaker-*` crates as Rust dependencies of llmenv's own binary — that
would pull the proxy's attack surface and release cadence into llmenv's
core; running it as a separate process keeps the two independent and
matches the "loopback proxy on the host" pattern this codebase already
trusts.

## Config schema

```yaml
features:
  sandbox:
    enabled: false      # opt-in, mirrors features.launch_proxy's default
    runtime: auto        # auto | docker | podman
    image: null          # null = llmenv's published minimal default image
```

`llmenv launch <engine> --container` / `--no-container` override
`features.sandbox.enabled` for one invocation without touching config.

## Base image

llmenv publishes one minimal default image: just enough libc/CA
certificates to exec an engine binary that's copied in at container start
(the binary itself is not baked into the image — copying it in at start
keeps the image from going stale every time an engine ships a release).
`features.sandbox.image` overrides it with any user-supplied image; llmenv
performs the same copy-in-and-exec step regardless of whose image it is.

Shipping even a minimal image is a real, ongoing commitment (build,
sign, patch) — accepted here per the user's explicit choice, scoped as
small as possible (no engine binaries baked in, no per-engine images).

## Error handling

- **Runtime not found** (`docker`/`podman` on `runtime: auto`, or the
  forced one, missing from `PATH`): fail before spawn with a clear error
  naming the missing binary, same pattern as `launch`'s existing "engine
  not found on PATH" check.
- **icebreaker proxy fails to bind:** non-fatal to the sandbox itself is
  not an option here (unlike `launch_proxy`'s notice channel) — if the
  proxy that holds the only path to real credentials doesn't start, the
  container would either get no credentials or an unsealed one. Fail the
  launch before the container starts.
- **Container exits/crashes:** wrapped by the existing `RelaunchCap`
  exactly as a bare engine crash is today.

## Follow-up issues to build this

Filed as separate implementation issues (this design doc is not an
implementation plan):

1. `features.sandbox` config schema in `llmenv-config`, `runtime: auto`
   detection (`podman` then `docker` on `PATH`).
2. `docker run`/`podman run --rm` exec path in `src/launch/mod.rs`,
   wrapped by the existing `RelaunchCap`; `--container`/`--no-container`
   CLI flags.
3. Bind mounts: project tree, `SSH_AUTH_SOCK`, resolved-env copy-in.
4. icebreaker integration: spawn/teardown lifecycle (mirroring
   `src/launch/proxy.rs`), sealed-token issuance for the container's
   outbound API credential.
5. ICM-over-remote-MCP path for a container with no local `icm serve`.
6. Minimal default base image: build, publish, version pin, an engine
   binary copy-in step at container start.
7. `llmenv doctor` checks: runtime present, icebreaker binary present when
   `features.sandbox.enabled`, image pullable.
8. Docs: a `docs/` page for the sandbox mode, config reference entry
   tagged with the version it ships in.
