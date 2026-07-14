<!-- markdownlint-disable MD013 -->
# Plugin Augmentation for ship-issue

Pick which trailofbits plugins run alongside `pre-pr-review` standard agents during PR review.

## When to Run

**fp-check** → PR has untrusted input, parsing, validation, or serialization. Skip pure docs/config. **Run after other agents** (needs their context for data-flow analysis).

**insecure-defaults** → PR modifies config, security settings, auth, or crypto.

**mutation-testing** → PR modifies tests or core logic paths (needs high branch coverage).

**slop-scan** → Always run. Invoke via the pinned wrapper script (`bundles/base/scripts/slop-scan.sh scan . --lint`) — never call `npx slop-scan` directly.

## Skip Rules

- Pure docs/config/deps: skip plugins (unless affects security/API)
- Frontend/styling: skip entry-point-analyzer
- No external input: skip fp-check
- Non-Rust: skip dimensional-analysis

## Scheduling

Standard analyzers (code-reviewer, semgrep, code-simplifier, silent-failure-hunter, security-audit, property-test-gap-finder, variant-bug-hunter, slop-scan via the pinned wrapper) run parallel.

fp-check → after standard agents finish.

## Message to ship-issue

Include plugin determination. Example:

```text
Standard agents parallel + fp-check (after, for data-flow context) + mutation-testing.
```
