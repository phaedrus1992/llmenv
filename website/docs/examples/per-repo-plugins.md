# Per-Repo Plugin Sets

Activate a plugin collection only when you're inside a specific repository.

## Config

```yaml
# ~/.config/llmenv/config.yaml

marketplace:
  - name: dev-commons
    url: https://github.com/phaedrus1992/llmenv-plugins.git

plugin-collection:
  - name: rust-tools
    tags: [rust]
    plugins:
      - dev-commons:rust-analyzer-hints
      - dev-commons:cargo-explain

  - name: web-tools
    tags: [web]
    plugins:
      - dev-commons:css-intellisense
      - dev-commons:ts-strict-helpers
```

```yaml
# /path/to/my-rust-project/.llmenv.yaml

tags: [rust]
```

```yaml
# /path/to/my-web-app/.llmenv.yaml

tags: [web]
```

## How it works

When you `cd` into `my-rust-project/`, the shell hook fires, walks up to find
`.llmenv.yaml`, and merges its `tags: [rust]` into the active set. The
`rust-tools` plugin collection fires, and those plugins are emitted into
`settings.json`. Walk into `my-web-app/` and the web plugins load instead.

## Force-enable without tags

If you want a bundle loaded unconditionally for a specific project (regardless
of any scope), use `enable_bundles` in the marker:

```yaml
# /path/to/special-project/.llmenv.yaml

enable_bundles: [my-special-bundle]
```

This bypasses tag matching — the bundle fires whenever you're in that project,
regardless of network, host, or user scope.

## Verify

```bash
cd /path/to/my-rust-project
llmenv doctor
# should show: active tags: [rust], plugin-collection: rust-tools loaded
```
