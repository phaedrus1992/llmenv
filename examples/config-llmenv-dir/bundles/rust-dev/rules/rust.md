<!-- markdownlint-disable MD003 MD013 MD022 MD041 -->
---
paths:

- "**/*.rs"
- "**/Cargo.toml"

---

# Rust Conventions

## Toolchain Configuration

When a project uses a pinned `rust-toolchain.toml`, explicitly declare required components in the `components` array. This ensures rustup installs them without requiring a separate `rustup component add` step.

**Critical:** `rust-analyzer` must be in the `components` list for the LSP to work out of the box. Without it, rust-analyzer fails to initialize even when installed globally, because `rustup` prioritizes the pinned toolchain and does not auto-sync components.

```toml
[toolchain]
channel = "stable"
components = ["rust-analyzer", "rustfmt", "clippy"]
```

## Workspace Lint Policy

Enforce strict linting in the root `Cargo.toml`:

```toml
[workspace.lints.clippy]
pedantic = { level = "warn", priority = -1 }
unwrap_used = "deny"
expect_used = "warn"
panic = "deny"
panic_in_result_fn = "deny"
unimplemented = "deny"
allow_attributes = "deny"
dbg_macro = "deny"
todo = "deny"
await_holding_lock = "deny"
exit = "deny"
mem_forget = "deny"
module_name_repetitions = "allow"
similar_names = "allow"

[workspace.lints.rust]
unsafe_code = "deny"
```

Use `?` and the appropriate error type for each crate. Use `#[expect(lint, reason = "...")]`
over `#[allow(lint)]` — it warns when the suppressed lint stops firing, preventing stale suppressions.
`allow_attributes` is denied to enforce this.

**unsafe:** Prefer `deny` over `forbid` for `unsafe_code` so a narrowly-scoped
`#[expect(unsafe_code, reason = "...")]` can override where genuinely required (FFI, platform
wrappers). Keep unsafe minimal and localized to a single crate. Every unsafe block needs a
`// SAFETY:` comment.

## Error Handling Strategy

| Layer | Strategy |
| ----- | -------- |
| Libraries | `thiserror` enums with `#[from]`, typed variants. Add `# Errors` docs on public `Result` fns. |
| Applications / binaries | `anyhow` or `eyre` + `color-eyre` with `.wrap_err()` and `.suggestion()` |

## Async Boundaries

Keep async confined to binary/application crates that need a runtime (tokio reconcile loops,
IPC, network). Library crates should stay pure synchronous unless their domain is inherently
async — sync I/O that blocks by design (e.g. HID, blocking syscalls) stays sync.

## Pre-Commit Hygiene

Run `cargo fmt` after every Rust file edit, before staging. Same for `shfmt` on shell scripts.
A formatting hook failure that bounces a commit wastes a full rebuild cycle.

## Test Conventions

- **Use `Arc::make_mut` to mutate `Arc`-wrapped test data** — not the clone+rewrap pattern
  (`(*data.field).clone()` / `data.field = Arc::new(…)`). `Arc::make_mut` is shorter, idiomatic,
  and avoids the paired clone/rewrap that drifts out of sync.

## Newtypes

Use newtypes to make invalid states unrepresentable. **"parse, don't validate"** — validate in the
constructor; existence then guarantees validity, and downstream never re-validates.

**When:** domain values with constraints (ranges, indices, IDs), semantic disambiguation
(`(u8, u8)` silently accepts wrong order; `(ReportId, AxisValue)` cannot), units where mixing is a
silent bug.

**Don't:** wrap unconstrained types with no confusion risk — that's just noise.

**Structure — keep the inner field private:**

```rust
pub struct AxisValue(i16);

impl AxisValue {
    pub fn new(raw: i16) -> Result<Self, AxisError> {
        if (AXIS_MIN..=AXIS_MAX).contains(&raw) {
            Ok(Self(raw))
        } else {
            Err(AxisError::OutOfRange(raw))
        }
    }
}
```

**Derive traits generously** — downstream can't add them later due to the orphan rule:

- Always: `Debug, Clone, PartialEq`
- When the inner type allows: `Eq, PartialOrd, Ord, Hash, Copy`
- Skip `Default` unless zero/empty is meaningful

**`TryFrom` delegates to `new()` — never duplicate validation:**

```rust
impl TryFrom<i16> for AxisValue {
    type Error = AxisError;
    fn try_from(raw: i16) -> Result<Self, Self::Error> { Self::new(raw) }
}
```

**Access:** prefer `AsRef` over `Deref` for constrained types. Use explicit accessors:

```rust
impl AxisValue {
    pub fn into_inner(self) -> i16 { self.0 }
    pub fn as_inner(&self) -> i16 { self.0 }  // Copy types: return by value
}
```

**Skip `Borrow<T>`** unless the newtype hashes and compares identically to the inner type —
otherwise it breaks `HashMap`/`HashSet` lookups.

## General Conventions

- All public types derive or implement `Debug`
- No glob re-exports (`pub use foo::*`) — export items individually
- Avoid vague names (`Manager`, `Handler`, `Processor`, `Service`) when a domain name exists
- Enums for state machines and effect/variant types, not bools or magic ints
- Lib crates: `tracing` (`error!`/`warn!`/`info!`/`debug!`) only; never `println!`/`eprintln!`
- Bin crates with a CLI: `println!` for user-facing output is fine; use `tracing` for diagnostics
- Hardcoded magic values (timeouts, retries, buffer sizes, ranges) need a comment explaining
  *why that value*. Timing constants and retry counts are a code smell — prefer making them
  configurable. If a value must be fixed, prefix it `DEFAULT_` and explain why it can't be tuned.

For broader API design (naming cost model, trait derives, `Send`/`Sync`, builder/sealed patterns):
[`rust-api.md`](rust-api.md). For runtime security footguns the borrow checker won't catch:
[`rust-hardening.md`](rust-hardening.md).
