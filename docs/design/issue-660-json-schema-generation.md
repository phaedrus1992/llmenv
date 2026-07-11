# Issue #660 — JSON Schema generation for materialized configs

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/660
- **Milestone:** Small Projects
- **Type:** Feature
- **Difficulty:** Moderate. The schema-emission part is small; the honest
  part is a typed-struct refactor of adapter output assembly.

## Problem

llmenv materializes engine configs (`opencode.json`, crush config, etc.)
but emits no formal JSON Schema describing what it produces. A generated
schema enables IDE validation/autocomplete of materialized files and
catches config errors at write time.

## Two facts that constrain the design (verified in code)

1. **`opencode.json` already carries a `$schema` key** pointing at the
   upstream schema `https://opencode.ai/config.json`
   (`src/adapter/opencode.rs:521`). Do **not** replace it — the upstream
   schema describes the full config surface including user passthrough
   keys llmenv doesn't own. The llmenv-generated schema is a **sidecar
   file only** (`opencode.schema.json` next to `opencode.json`); the
   in-file `$schema` pointer stays upstream.
2. **Adapters build output via untyped `serde_json::json!` inserts**, not
   typed structs (see `src/adapter/opencode.rs:429–525`). The issue's
   design constraint — "generated from the same data structures that drive
   materialization, one source of truth" — therefore requires introducing
   typed output structs first. A hand-written parallel schema is
   explicitly what the issue rejects.

## Design

### Step 1: dependency

Add `schemars` (latest stable — look it up, don't assume) to the workspace.
Per `AGENTS.md`: run `cargo deny check`, add the license id to `deny.toml`
and `about.toml` if new, and regenerate both attribution files with
`scripts/gen-attribution.sh` **in the same change** as the `Cargo.lock`
update.

### Step 2: typed output structs for the opencode adapter

In `src/adapter/opencode.rs`, replace the untyped map-inserts for the
sections llmenv itself authors with structs deriving
`Serialize + schemars::JsonSchema`:

- MCP entries (local: `type`/`command`/`environment`; remote:
  `type`/`url`/`headers`/`timeout` — mirror the exact keys currently
  inserted at `opencode.rs:439–460`),
- LSP entries (`command`/`extensions`/`env`, `opencode.rs:494–502`),
- top-level llmenv-authored keys (`plugin`, `instructions`, …).

The `materialize()` output must stay **byte-identical** after the refactor
(field order: check whether output is serialized via `BTreeMap`/sorted
keys today and preserve that). Snapshot tests under `tests/snapshots/` and
`tests/claude_code_adapter.rs`-style adapter tests are the guard — run the
full suite before and after; zero snapshot diffs allowed for this step.

### Step 3: schema assembly + adapter probe

- New module `src/materialize/schema_gen.rs`: assembles a root schema
  (draft 2020-12, `schemars`'s default) from the typed structs — top-level
  object with the llmenv-authored properties, and
  `"additionalProperties": true` at the root so user passthrough keys
  (see `tests/native_passthrough.rs`) never fail validation.
- New trait method `AgentAdapter::config_schema(&self) -> Option<serde_json::Value>`
  in `src/adapter/mod.rs`, default `None`. `OpencodeAdapter` returns
  `Some(...)`. Other adapters stay `None` for now (extensible later —
  don't touch them).

### Step 4: sidecar emission

In the materialize pipeline (`src/materialize/`), after writing an
adapter's config file: if `config_schema()` is `Some`, write
`<config-stem>.schema.json` next to it. The sidecar participates in the
same staleness/regeneration lifecycle as the config itself (find where
materialize tracks written files — e.g. whatever `tests/check_stale.rs`
exercises — and register the sidecar there so `llmenv regenerate` and
cleanup handle it).

## Tests

1. Refactor step: full existing suite green, zero snapshot changes.
2. `schema_gen` unit test: generated schema is valid JSON Schema (parse it;
   assert `$schema`/`type`/`properties` shape) and root allows additional
   properties.
3. Integration test: materialize with the opencode adapter in a tempdir →
   `opencode.schema.json` exists; a sample materialized `opencode.json`
   validates against it (use the `jsonschema` crate as a dev-dependency
   **only if** validation-in-test is cheap to add; otherwise assert on
   schema structure and skip live validation — note the choice in the
   test).
4. Adapters with `config_schema() == None` emit no sidecar.

## Acceptance criteria

- [ ] `opencode.schema.json` sidecar emitted alongside `opencode.json`;
      in-file `$schema` still points upstream.
- [ ] Schema is generated from the same structs that produce the config
      (no hand-maintained parallel schema).
- [ ] Root schema tolerates passthrough keys (`additionalProperties: true`).
- [ ] Attribution files regenerated for the new dependency; `cargo deny
      check` passes.
- [ ] CHANGELOG `[Unreleased]` entry added (keepachangelog skill), and the
      forward-merge reconciliation check from `AGENTS.md` performed.
- [ ] Clippy/fmt clean; full workspace suite green.

## Out of scope (per issue)

- Schema federation/composition across adapters.
- Version-dependent schema generation.
- llmenv validating configs against the schema itself.
- Crush/claude-code sidecar wiring (extensible via the probe; follow-up).
