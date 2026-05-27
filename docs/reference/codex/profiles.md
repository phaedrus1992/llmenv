# Profiles

Profiles are named, overlayable config variants — a Codex feature with **no
Claude Code analog**. A profile is a `[profiles.<name>]` table (or a separate
`$CODEX_HOME/<name>.config.toml` file) selected with `--profile <name>`.

## Defining and selecting

```toml
# default profile (used when --profile is omitted)
profile = "work"

[profiles.work]
model = "gpt-5.4"
approval_policy = "on-request"

[profiles.experiment]
model = "gpt-5.4-thinking"
approval_policy = "untrusted"
```

In the precedence chain, profile files sit **above user config but below
project config and CLI flags** (see [config-toml](./config-toml.md)). The
guidance is: put shared defaults in the base `config.toml`, keep profiles focused
on the values that *differ*.

## Gaps vs llmenv

llmenv resolves a **single merged manifest** per invocation (tag intersection
across active scopes). There is no notion of selecting among named variants at
runtime — the *environment* (network/host/user/project) determines the config,
not a `--profile` flag.

So profiles are an **impedance mismatch**, and there are two plausible designs:

1. **Ignore profiles.** Generate a single `config.toml` per resolved
   environment. Simplest; matches llmenv's philosophy (the environment picks the
   config, not the user). **Recommended starting point.**
2. **Map scopes → profiles.** Emit a `[profiles.<scope>]` table per scope and let
   the user (or a wrapper) pass `--profile`. This re-exposes a runtime switch
   llmenv deliberately abstracts away, so it probably only makes sense if a user
   explicitly wants Codex's profile UX.

Either way, no schema change is strictly required — profiles are an *output*
shape decision for the adapter, not new input vocabulary.
