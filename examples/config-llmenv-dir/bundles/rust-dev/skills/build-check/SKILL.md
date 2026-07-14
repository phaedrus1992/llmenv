---
name: build-check
description: Run cargo build + clippy + test before claiming done in the llmenv repo.
---

# Build check

Run, in order:

1. `cargo build`
2. `cargo clippy --all-targets -- -D warnings`
3. `cargo test`

All three must pass.
