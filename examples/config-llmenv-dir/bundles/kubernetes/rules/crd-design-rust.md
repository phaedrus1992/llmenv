---
paths:
  - "**/*.rs"
  - "**/Cargo.toml"
---

# CRD Design Rules (kube-rs)

Applies to CustomResourceDefinitions defined in Rust (`kube` / `CustomResource` derive).

## Backwards Compatibility

- New fields must use `Option<T>` with `#[serde(default)]` — required fields break existing CRs.
- Renaming or removing fields breaks existing CRs — add the new field alongside the old with a
  migration path.
- Changing field semantics silently corrupts state.
- `spec.selector.matchLabels` are immutable — never derive selector labels from mutable values.
- After Helm/operator upgrades, verify CRD instances retain all spec fields (3-way merge can drop
  newly-added fields).

## Field Merging & Config Composition

When composing a partial spec over defaults (CR over app-default, per-app over stack-default), use
field-level merge helpers — not wholesale `unwrap_or(default)`, which lets a serde-defaulted field
(`""`, `0`, `Http`) clobber a real default. The footgun is the **argument direction**, so make it
uniform and misuse-resistant:

- **One direction for every helper.** Pick `override.merge_with(&base) -> Merged`: the receiver
  (`self`) is the higher-priority value and wins per field; the argument is the fallback. Never ship
  two helpers with opposite receiver meaning (e.g. one `user.merge_with(&base)` and one
  `base.merge_with(&over)`) — a swapped call silently drops user input **and still type-checks**.
- **Name the parameter for its role** (`base` / `fallback`), never `defaults` when the receiver is
  also a "defaults" value — that naming is what invites the swap.
- **Per field:** keep `self`'s value unless it's the empty/zero/`None` sentinel, then take the
  fallback. Enums without a sentinel (e.g. `probe_type`) take `self` unconditionally — which is
  exactly why a swapped receiver corrupts them with no error.
- **Test the direction.** Every mergeable type needs a unit test asserting an explicit override
  value beats a *non-empty* default (not just that a missing field inherits). The "missing inherits"
  test alone passes even when the arguments are swapped.

## Newtype Patterns

Use newtypes for domain values with constraints: private inner field, a validating constructor as
the single source of truth. Derive `Debug, Clone, PartialEq, Eq` always; add `PartialOrd, Ord,
Hash, Copy` when the inner type supports it. Use `TryFrom` delegating to `new()`, and `AsRef` over
`Deref` for borrowed access. Use `#[serde(try_from = "String")]` for validated CRD string fields.
See [`rust.md`](rust.md) §Newtypes in the rust-dev bundle for the full pattern.

## Time-Based Configuration

Prefer a `Duration` newtype that accepts human-friendly strings (`5m`, `1h`, `1h30m`) as well as
bare seconds (`300`), stored internally as whole seconds. When that isn't possible (third-party CRD
constraints), fall back to **seconds, never milliseconds** — Kubernetes Job deadlines and database
timeouts operate in seconds, and millisecond precision adds complexity without value for
operational settings. All defaults and documentation should use seconds or larger units.

## Enum String Representations

Prefer strum derives (`Display`, `AsRefStr`, `IntoStaticStr`) over manual `as_str()` or `Display`
impls. Serde and strum are independent — keep `#[serde(rename = "...")]` and
`#[strum(serialize = "...")]` aligned when both are needed.

## CRD Generation

If the CRD YAML is generated from Rust source (e.g. `kube`'s `CustomResourceExt::crd()`), **never
hand-edit the generated manifest** — edits are overwritten on rebuild. Regenerate whenever any CRD
source file changes, and after a rebase/merge that brings in CRD-affecting commits. Verify the
generated file matches source with a `git diff` against the base branch after rebasing.
