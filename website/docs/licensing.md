# Licensing & attribution

## llmenv's own license

llmenv is dual-licensed under either of:

- [Apache License, Version 2.0](https://github.com/phaedrus1992/llmenv/blob/main/LICENSE-APACHE)
- [MIT license](https://github.com/phaedrus1992/llmenv/blob/main/LICENSE-MIT)

at your option — the SPDX expression is `MIT OR Apache-2.0`. Unless you state
otherwise, any contribution you intentionally submit for inclusion shall be
dual-licensed as above, with no additional terms.

## Third-party attribution

Release binaries statically link a tree of dependency crates. Most of their
licenses (MIT, BSD-3-Clause, ISC, Apache-2.0, …) require their copyright and
permission notices to travel with the binary. Those notices are collected on the
[Third-party licenses](./third-party-licenses.md) page (and in the
`THIRD-PARTY-LICENSES.md` file shipped with each release).

Both are **generated artifacts** — never hand-edited. They are produced from the
locked dependency graph by [`cargo-about`](https://github.com/EmbarkStudios/cargo-about):

```bash
cargo install cargo-about --locked --features cli   # once
scripts/gen-attribution.sh                          # regenerates both copies
```

- `about.toml` — the `accepted` license allowlist (kept in sync with
  `deny.toml`'s `[licenses].allow`).
- `about.hbs` / `about-web.hbs` — the Markdown / docs-site output templates.

### When to regenerate

Regenerate and commit both attribution files in the **same change** that alters
dependencies — any add, removal, or version bump that touches `Cargo.lock`. A
diff that changes dependencies but leaves the notices stale is incomplete.

### Adding a new license

If a dependency introduces a license id not yet allowed:

1. Confirm it is compatible with the existing set — permissive or weak
   (file-scoped) copyleft only; **no strong copyleft** (GPL/AGPL/LGPL).
2. Add the id to **both** `deny.toml` (`[licenses].allow`) and `about.toml`
   (`accepted`).
3. Run `scripts/gen-attribution.sh`.

## Enforcement

`cargo deny check` validates the license policy (plus advisories, bans, and
sources). It runs in CI (the `deny` job) and on `git push` via the prek
`cargo-deny` hook. A dependency under a license outside the allowlist fails the
build instead of silently shipping.
