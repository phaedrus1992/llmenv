# Rust Conventions

<!--
  This file lives in bundles/rust-dev/rules/ and is injected alongside
  AGENTS.md whenever the `lang-rust` tag is active (i.e., in Rust projects).
  It supplements — not replaces — the Rust section in AGENTS.md. Use this
  file for Rust-specific detail that would clutter the global AGENTS.md.
-->

## Workspace Lint Policy

All crates share a workspace-level `[lints]` section in Cargo.toml:

```toml
[workspace.lints.clippy]
pedantic = "warn"

[workspace.lints.rust]
unsafe_code      = "deny"
```

Individual crates inherit via `[lints] workspace = true`. Never suppress a
lint inline without a comment explaining why it can't be fixed.

## Error Handling Strategy

- Libraries: `thiserror` — typed, structured errors that callers can match on.
- Applications: `anyhow` — context-rich error chains for human-readable output.
- Never use `unwrap()` or `expect()` in production paths. `let-else` for early
  returns on `Option`/`Result`.

```rust
// Library: typed error
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("invalid header: {0}")]
    InvalidHeader(String),
}

// Application: anyhow context chain
fn load_config(path: &Path) -> anyhow::Result<Config> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("reading config at {}", path.display()))?;
    toml::from_str(&text).context("parsing config")
}
```

## Async Boundaries

- Use `tokio` for async runtimes. Do not mix runtimes.
- Spawn CPU-bound work with `tokio::task::spawn_blocking` — never block the
  async executor with synchronous I/O or computation.
- Prefer `Arc<T>` over `Rc<T>` for shared state across async tasks (`Rc` is
  not `Send`).

## Newtypes

Prefer newtypes over primitive aliases for domain concepts:

```rust
// Prevents mixing up user IDs and account IDs at the type level.
pub struct UserId(u64);
pub struct AccountId(u64);
```

Implement `Deref` only when the newtype is a transparent wrapper and callers
need direct access to the inner type. Otherwise, provide explicit methods.

## Test Conventions

- Unit tests: `#[cfg(test)]` module in the same file as the code under test.
- Integration tests: `tests/` directory at the crate root.
- Use `cargo nextest` for faster parallel test execution.
- Property-based tests: `proptest` crate. Put them in the same `#[cfg(test)]`
  module, separate from example-based tests.

## Pre-Commit Hygiene

Before every commit:

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo deny check
```

If `prek` is installed, these run automatically on `git commit`.
