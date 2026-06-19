---
name: build-check
description: >
  Run the full local quality pipeline for a Rust project: format, clippy,
  tests, and supply-chain checks. Discovers the actual CI commands from the
  project's workflow files first; falls back to standard cargo commands if
  CI config is absent. Invoke before opening a PR or after a significant
  change.
---

<!--
  This skill lives in bundles/rust-dev/skills/build-check/. It only loads
  when the `lang-rust` tag is active (via project .llmenv.yaml), keeping it
  out of sessions where Rust tooling isn't relevant.

  HOW IT CONNECTS:
    - Activated by: `lang-rust` tag → rust-dev bundle → this skill
    - Uses: Bash (cargo, clippy, just) — covered by universal allow list
    - The `rust-lsp` plugin-collection (rust-analyzer-lsp) provides live
      diagnostics; this skill is for the batch/pre-commit check.
    - Output is passed through rtk on the laptop host (host-laptop bundle),
      which filters verbose cargo output before it reaches Claude's context.
-->

# Build Check

Run the full quality pipeline. Stop at the first failure and report it.

## Step 1 — Discover CI commands (source of truth)

Check `.github/workflows/` for the project's actual CI commands. CI is what
matters; local shortcuts that differ from CI are false confidence.

```bash
ls .github/workflows/
```

Look for `cargo test`, `cargo clippy`, `cargo fmt --check`, `cargo deny`,
`just ci`, or equivalent. Use those exact commands.

## Step 2 — Run the pipeline

### If the project has a `just ci` recipe:

```bash
just ci
```

This is the preferred path — `just ci` encodes the project's full quality
gate in one command.

### Otherwise, run the standard sequence:

```bash
# 1. Format check — fails if any file would be reformatted.
cargo fmt --all -- --check

# 2. Clippy — treat all warnings as errors.
cargo clippy --all-targets --all-features -- -D warnings

# 3. Tests — all features, all targets.
cargo test --all-features

# 4. Supply chain — license and vulnerability check.
cargo deny check
```

Run these sequentially. Stop on first failure and report:
- Which step failed
- The first 20 lines of error output
- The fix (if obvious from the error)

## Step 3 — Report

If all steps pass:
> Build check passed: fmt, clippy, tests, deny all clean.

If any step fails:
> Build check failed at [step]. Error: [first meaningful error line].
> Fix: [suggested fix if clear, otherwise "investigate"].
