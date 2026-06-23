# Skill-Gate Pattern — Conditional Skill Activation

**Issue #222:** Per-stack rule files with skill-gate pattern

## Overview

A "skill-gate" is a **prerequisite check** that guards skill availability. Gates prevent skills from running when their preconditions aren't met, reducing wasted prompts and context cost.

## Gate Types

### 1. Tag Gate — Skill Only Loads on Specific Tags

**When:** Skill is language-specific or domain-specific.

**Example:** `/build-check` only loads when `lang-rust` tag is active.

```yaml
# in config.yaml or bundle.yaml
skills:
  - path: ./skills/build-check/
    when: [lang-rust]   # Gate: only loads for Rust projects
```

**Cost:** Skill descriptor is not sent to the agent if the gate doesn't match. Saves ~500 tokens per prompt on irrelevant projects.

### 2. Prerequisite Gate — Skill Requires Upstream Action

**When:** Skill assumes something is already done (build passed, tests run, code formatted).

**Example:** A "review" skill that assumes code is already formatted.

In the skill descriptor (SKILL.md):

```markdown
# /code-review

Reviews code for bugs and style issues.

**Prerequisite:** Requires `cargo fmt --check` to pass locally first.
If the build or formatter is broken, this skill will not help.

**Gate Check:**
\`\`\`bash
cargo fmt --check && cargo build --all-targets 2>/dev/null
\`\`\`

**Cost Saved:** ~2000 tokens per run (no need to explain build errors unrelated to the review).
```

The agent reads the prerequisite and checks it before invoking the skill. If the gate fails, the agent is told "prerequisite failed; fix this first" rather than running the skill against broken code.

### 3. Context Gate — Skill Requires Pre-Indexed Content

**When:** Skill expects documentation or reference material to be already indexed.

**Example:** A "kubernetes-audit" skill that requires CRD docs to be indexed first.

In the skill descriptor:

```markdown
# /k8s-audit

Audits Kubernetes manifests for RBAC, network policy, and container security violations.

**Prerequisite:** Requires `ctx_index(path: "website/docs/crds/", source: "k8s-crds")` to run first.

**Cost:** ~1000 tokens saved when CRD reference is indexed vs. re-fetching docs.

**Gate Activation:**
When the user says "audit my manifests" for the first time in a session:
1. Check if `ctx_search(source: "k8s-crds")` returns results
2. If not, run `ctx_index(...)` first
3. Then run the audit skill
```

## Implementing Skill-Gates in llmenv

### 1. Tag Gates (Already Supported)

In `bundle.yaml` or `.llmenv.yaml`:

```yaml
skills:
  - path: ./skills/my-skill/
    when: [lang-rust, domain-systems]
```

### 2. Prerequisite Gates (Scaffold)

Add a `gate-check.sh` script to your skill directory:

```bash
# skills/my-skill/gate-check.sh
#!/bin/bash
set -euo pipefail

# Check: build passed?
if ! cargo build --quiet 2>/dev/null; then
  echo "❌ Prerequisite failed: cargo build" >&2
  exit 1
fi

# Check: tests pass?
if ! cargo test --quiet 2>/dev/null; then
  echo "❌ Prerequisite failed: cargo test" >&2
  exit 1
fi

echo "✓ All prerequisites met"
exit 0
```

The adapter can invoke `gate-check.sh` before offering the skill.

### 3. Context Gates (Scaffold)

In the skill descriptor (SKILL.md), document what content must be indexed:

```markdown
# /k8s-audit

Requires: \`ctx_index(path: "crds/", source: "k8s-api-reference")\`

[Rest of skill description...]
```

The agent reads this and checks: "Has anyone indexed k8s-api-reference?" If not, it runs the index first.

## Cost Savings

| Gate Type | Tokens Saved | When |
|-----------|--------------|------|
| Tag gate | ~500/prompt | Skill not loaded for irrelevant projects |
| Prerequisite gate | ~2000/run | Skill not run against broken code |
| Context gate | ~1000/run | Reference material indexed once, reused multiple times |

## Example: Complete Skill-Gated Bundle

```yaml
# bundles/token-efficiency/bundle.yaml

skills:
  # Language gates
  - path: ./skills/rust-check/
    when: [lang-rust]
  - path: ./skills/ts-build/
    when: [lang-typescript]
  
  # Domain gates
  - path: ./skills/k8s-audit/
    when: [domain-kubernetes]
  - path: ./skills/sql-schema/
    when: [domain-database]

rules:
  - rules/skill-gates.md      # This file
  - rules/rust.md             # Rust-specific standards + skill gates
  - rules/typescript.md       # TS-specific standards + skill gates
```

When the user opens a Rust project, they get:
- `rust-check` skill available
- `rust.md` rules loaded
- TS skills hidden (tag gate blocks them)

When they open a TypeScript project, they get:
- `ts-build` skill available
- `typescript.md` rules loaded
- Rust skills hidden

## Future Enhancements

1. **Hook-based gates:** Check prerequisites via hooks (SessionStart hook runs gate-check.sh, disables skill if it fails)
2. **Capability gates:** Block a capability (e.g., `Bash` for certain patterns) until a prerequisite passes
3. **Dynamic gates:** Gate activation based on code analysis (e.g., "only offer refactoring skill if complexity > 8")

