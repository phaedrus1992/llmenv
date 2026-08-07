# Getting Started with llmenv

llmenv is a universal, scope-aware environment for AI coding agents. It detects
your current context (network, host, user, project), selects the matching
configuration, materializes it into an agent-native config directory, and points
the agent at it — automatically, from a shell hook.

This page takes you from zero to a working setup. For the conceptual model, read
[Concepts](concepts.md) afterward.

## 1. Install

**Homebrew (macOS / Linux):**

```bash
brew tap phaedrus1992/tap
brew install llmenv
llmenv --version        # verify
brew upgrade llmenv     # upgrade later
```

> The [phaedrus1992/tap](https://github.com/phaedrus1992/homebrew-tap) is a Homebrew
> repository maintained alongside llmenv. If you're on Linux, `brew` is
> [Linuxbrew](https://docs.brew.sh/Homebrew-on-Linux). <!-- markdownlint-disable-line MD013 -->

**Cargo:**

```bash
cargo install llmenv
llmenv --version        # verify
```

**From source:**

```bash
git clone https://github.com/phaedrus1992/llmenv.git
cd llmenv
cargo build --release
./target/release/llmenv --version
```

## 2. Initialize configuration

```bash
llmenv init
```

This writes a template `config.yaml` into your config directory
(`~/.config/llmenv/config.yaml`, or `$LLMENV_CONFIG_DIR` if set). It won't
overwrite an existing config. To start from an existing config repository
instead:

```bash
llmenv init --repo https://github.com/you/llmenv-config.git
```

## 3. Install the shell hook

The hook runs `llmenv export` on every prompt, keeping the environment in sync as
you move between directories and networks.

**zsh** — add to `~/.zshrc`:

```bash
eval "$(llmenv hook zsh)"
```

**bash** — add to `~/.bashrc`:

```bash
eval "$(llmenv hook bash)"
```

Reload your shell (`exec zsh` / `exec bash`) or open a new terminal. To preview
what the hook installs without committing to it, just run `llmenv hook zsh` and
read the output.

## 4. Verify the setup

```bash
llmenv doctor
```

`doctor` checks:

- configuration parsing,
- cache directory writability,
- git remote connectivity,
- orphans — scopes/tags/bundles/MCP/plugins that can never activate, a memory
  `server_host` missing from `host:`, and unknown fields in project markers.

Then inspect what resolves for your current directory:

```bash
llmenv status        # active scopes + tags, parse status
llmenv context       # the fuller resolved view
llmenv export        # the actual export lines the hook runs
```

## 5. Add a project

Per-project configuration lives in a `.llmenv.yaml` marker at the project root —
not in `config.yaml`. Drop one in and llmenv discovers it by walking the current
directory upward to `$HOME`:

```yaml
# ~/code/myapp/.llmenv.yaml
id: myapp
name: MyApp
description: "Customer-facing API"
tags: [myapp, rust]
enable_bundles: [base]      # optional: force-enable bundles regardless of tags
```

`cd` into the project and run `llmenv context` — you should see the project scope
active and its tags joined to the set.

## Minimal config example

```yaml
cache:
  cache_dir: "~/.cache/llmenv"
  sync_interval_minutes: 60

scope:
  network:
    - id: office
      match: { gateway_mac: "aa:bb:cc:dd:ee:ff" }
      tags: [office]
  user:
    - id: me
      match: { user: "alice" }
      tags: [me]

bundle:
  - name: base
    when: [me]
    vars:
      EDITOR: "code"
```

See [Configuration](configuration.md) for the complete schema.

## Commands reference

Everyday commands:

| Command | Purpose |
| ------- | ------- |
| `llmenv init` | Write a template config |
| `llmenv hook <zsh\|bash>` | Print shell integration code |
| `llmenv export [--compress]` | Resolve + export the environment |
| `llmenv regenerate` | Re-materialize without exporting env vars |
| `llmenv status [section]` | Show active scopes, tags, and config status |
| `llmenv doctor [--gc]` | Validate wiring |
| `llmenv prune [--all]` | Clean stale cache folders |

Full per-command reference: [commands.md](commands.md).

## Common first errors

- **"Config already exists"** from `init` — expected; `init` never overwrites.
  Edit `~/.config/llmenv/config.yaml` directly.
- **Nothing activates** — your scopes' tags don't match any contributor's tags,
  or no scope matches your environment. Run `llmenv status scopes` and
  `llmenv status tags` (active items are marked) and check
  [Troubleshooting](troubleshooting.md).
- **YAML parse error** — usually an unquoted value containing a colon. Quote
  addresses, MACs, SSIDs, and URLs. See
  [Configuration → YAML gotchas](configuration.md#yaml-gotchas).
- **Network scope never matches** — only `gateway_mac` is evaluated today;
  `ssid`/`cidr` are ignored. `llmenv doctor --all` flags a network scope whose
  `match` has no `gateway_mac` as an orphan that can never activate (added in
  v3.8.0). Use a host scope as a reliable fallback.

## Recommended external tools

These aren't required to run llmenv, but several `features:` and `memory:`
integrations expect them to already be installed and on `PATH`:

- **[icm](https://github.com/rtk-ai/icm)** — the memory backend behind
  `features.memory` (ICM, Infinite Context Memory). Install via Homebrew:

  ```bash
  brew tap rtk-ai/tap
  brew install icm
  ```

  Or build from source with `cargo install --path crates/icm-cli` (see the
  repo's README for feature flags). See [MCP & Memory](mcp.md) for how llmenv
  wires it in.

- **[codebase-memory-mcp](https://github.com/DeusData/codebase-memory-mcp)** —
  the indexer behind `features.codebase_memory`. Must be on `PATH` (llmenv
  looks for it at `~/.local/bin`):

  ```bash
  curl -fsSL https://raw.githubusercontent.com/DeusData/codebase-memory-mcp/main/install.sh | bash
  ```

  See [MCP & Memory → Codebase memory](mcp.md#codebase-memory-codebase_memory)
  for the llmenv-side config.

Run `llmenv doctor` after installing either one — it flags config that
references a feature whose backend isn't reachable.

## Next steps

- [Concepts](concepts.md) — how resolution actually works.
- [Configuration](configuration.md) — the full schema.
- [MCP & Memory](mcp.md) — wiring MCP servers and the shared memory backend.
- [Examples](examples/index.md) — worked scenarios, and a link to a complete
  [reference config](https://github.com/phaedrus1992/llmenv/tree/main/examples/config-llmenv-dir)
  in the repo.

## Community

[Join the Discord](https://discord.gg/HvQrGAaGAS) — ask questions, share configs, report bugs, or just hang out.
