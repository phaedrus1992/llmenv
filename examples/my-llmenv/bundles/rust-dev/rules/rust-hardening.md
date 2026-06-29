---
paths:
  - "**/*.rs"
  - "**/Cargo.toml"
---

# Rust Hardening Rules

Bugs the borrow checker won't catch. Sourced from the [uutils CVE audit](https://corrode.dev/blog/bugs-rust-wont-catch/)
— dozens of CVEs in safe Rust, none caught by clippy or `cargo audit`. These patterns matter
anywhere code parses untrusted input (config, network bytes, API responses, CRD specs, env vars)
or touches the filesystem.

## Panics on untrusted input = DoS

In any code path that touches attacker-shaped input — config fields, API responses, env vars, file
contents, network bytes — every `unwrap` / `expect` / `[i]` indexing / `as` cast / `from_utf8` /
unchecked arithmetic is a DoS waiting to happen. The failure mode depends on context: with the
default `panic = "unwind"` strategy a panic unwinds the current thread or task; on the main thread
(or with `panic = "abort"`) the process terminates. Inside a tokio task the runtime catches the
unwind, but a loop that panics on every iteration over the same input still produces an effective
crash loop — work never makes progress.

Use:

- `?` to propagate errors
- `.get(i)` instead of `slice[i]`
- `checked_*` / `saturating_*` / `try_from` instead of `as` casts and bare arithmetic
- `from_utf8_lossy` or `OsStr` when bytes aren't guaranteed UTF-8 (kernel paths, env values)

Push validation to the boundary — newtype constructors, deserializer impls, the edge of the request
handler — and surface real errors. See [`rust-api.md`](rust-api.md) §Type Safety (C-VALIDATE).

**Hardening lints** (noisy in trusted glue/test code, so `warn` not `deny` — but treat every
warning as a smell to fix):

```toml
[workspace.lints.clippy]
indexing_slicing = "warn"
arithmetic_side_effects = "warn"
panic_in_result_fn = "deny"
```

The bar: no raw `[i]` or unchecked `+`/`-`/`*` on data that crossed a trust boundary. If you
`#[expect(...)]` one, leave a comment explaining why the input is bounded. `panic_in_result_fn` is
a `deny` — panicking from a function whose signature already returns `Result` is never right;
return an error instead.

## Don't trust a `&Path` across two syscalls

A `&Path` is just a name the kernel re-resolves on every call. Between check and use, anyone with
write access to a parent directory can swap in a symlink (TOCTOU), and the privileged action lands
on the attacker's chosen target. This applies to any code that writes to a user-supplied path or
touches a host path mounted from outside its trust boundary.

Rules:

- **New files:** `OpenOptions::new().write(true).create_new(true).open(p)` — refuses existing files
  *and* dangling symlinks.
- **Anything else:** open the parent directory once and operate relative to that fd. Use
  [`cap-std`](https://docs.rs/cap-std) or [`rustix`](https://docs.rs/rustix) for `openat`-style
  APIs. `std::fs::*` re-resolves on every call.
- If you act on the same path twice, **assume it's a TOCTOU bug until proven otherwise.**

## String equality on paths ≠ filesystem identity

`"/"`, `"./"`, `".///"`, and a symlink to `/` all *are* the root directory but compare unequal as
strings. uutils shipped a CVE by string-matching `"."` and `".."` while accepting `"./"` — and then
deleted the current directory.

For identity comparisons:

- `fs::canonicalize` resolves `.`, `..`, and symlinks to an absolute path
- For true identity, open both paths and compare `(dev, inode)` via
  `std::os::unix::fs::MetadataExt`

String equality on user-supplied paths is almost always wrong.

## Set permissions at creation, not after

```rust
// Bad — world-accessible during the gap between calls
fs::create_dir(&path)?;
fs::set_permissions(&path, Permissions::from_mode(0o700))?;

// Good — born with the right mode
DirBuilder::new()
    .mode(0o700)
    .recursive(true)
    .create(&path)?;
```

A `chmod` after creation doesn't revoke file descriptors opened during the window. Use
`OpenOptions::mode()` for files and `DirBuilderExt::mode()` for directories. Set `umask` explicitly
if it matters — the kernel ANDs your mode with it.
