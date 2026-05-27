# Shell environment policy

`[shell_environment_policy]` controls which environment variables Codex passes to
subprocesses it launches (e.g. model-proposed tool commands). It's a security
surface — the point is to avoid leaking secrets into spawned commands. Claude
Code has nothing directly equivalent.

```toml
[shell_environment_policy]
inherit = "none"                       # none | core | all
set = { PATH = "/usr/bin", MY_FLAG = "1" }
ignore_default_excludes = false        # false keeps the KEY/SECRET/TOKEN filter
exclude = ["AWS_*", "AZURE_*"]
include_only = ["PATH", "HOME"]
experimental_use_profile = false       # use the user shell profile when spawning
```

- `inherit`: `none` (clean start), `core` (trimmed set), or inherit all.
- `set`: explicit overrides injected into every subprocess.
- `ignore_default_excludes = false`: keeps Codex's automatic
  KEY/SECRET/TOKEN-name filter *before* your includes/excludes run.
- `exclude` / `include_only`: case-insensitive globs (`*`, `?`, `[A-Z]`).

Evaluation order: start from `inherit`, apply the default secret filter (unless
disabled), then `exclude`, then `include_only`, then `set`.

## Gaps vs llmenv

llmenv has no environment-filtering vocabulary. This is a **lower-priority gap** —
it's a hardening knob, not core config materialization. A `CodexAdapter` could:

- Ignore it initially (rely on Codex defaults, which already filter common secret
  patterns).
- Later expose a schema block if users want deterministic, policy-driven subprocess
  environments (e.g. corp environments that must scrub cloud creds).

Note this interacts with llmenv's MCP `env`/`env_vars` story: MCP server env is
configured per-server, while `shell_environment_policy` governs the *agent's*
tool subprocesses. They're separate surfaces.
