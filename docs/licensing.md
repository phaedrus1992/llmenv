# Licensing & attribution

## llmenv's own license

llmenv is dual-licensed under either of:

- Apache License, Version 2.0 ([`LICENSE-APACHE`](../LICENSE-APACHE))
- MIT license ([`LICENSE-MIT`](../LICENSE-MIT))

at your option. The SPDX expression in `Cargo.toml` is `MIT OR Apache-2.0`.
Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you shall be dual-licensed as above, without any
additional terms or conditions.

## Third-party attribution

Release binaries statically link a tree of dependency crates. Most of their
licenses (MIT, BSD-3-Clause, ISC, Apache-2.0, …) require that the original
copyright and permission notices travel with the binary. Those notices are
collected in [`THIRD-PARTY-LICENSES.md`](../THIRD-PARTY-LICENSES.md) at the repo
root.

`THIRD-PARTY-LICENSES.md` is a **generated artifact** — do not edit it by hand.
The same notices are also published on the docs site
(`website/docs/third-party-licenses.md`) so they are browseable without cloning.
Both are produced from the locked dependency graph by
[`cargo-about`](https://github.com/EmbarkStudios/cargo-about):

```bash
cargo install cargo-about --locked --features cli   # once
scripts/gen-attribution.sh                          # regenerates both copies
```

- [`about.toml`](../about.toml) — the `accepted` license allowlist (kept in sync
  with `deny.toml`'s `[licenses].allow`).
- [`about.hbs`](../about.hbs) / [`about-web.hbs`](../about-web.hbs) — the output
  templates for the distribution file and the docs-site page.
- [`scripts/gen-attribution.sh`](../scripts/gen-attribution.sh) — runs
  `cargo about` for both outputs from one command.

### When to regenerate

Run `scripts/gen-attribution.sh` and commit both generated files in the **same
change** that alters dependencies — any add, removal, or version bump that
touches `Cargo.lock`. A diff that changes dependencies but leaves the notices
stale is incomplete (see the hard rule in [`AGENTS.md`](../AGENTS.md)).

### Adding a new license

If a dependency introduces a license id not yet allowed:

1. Confirm it is compatible with the existing set — permissive or weak
   (file-scoped) copyleft only; **no strong copyleft** (GPL/AGPL/LGPL).
2. Add the id to **both** `deny.toml` (`[licenses].allow`) and `about.toml`
   (`accepted`).
3. Run `scripts/gen-attribution.sh`.

## Enforcement

`cargo deny check` validates the license policy (plus advisories, bans, and
sources). It runs in CI (`.github/workflows/ci.yml`, the `deny` job) and on
`git push` via the prek `cargo-deny` hook (`.pre-commit-config.yaml`). A
dependency under a license outside the allowlist fails the build instead of
silently shipping.
