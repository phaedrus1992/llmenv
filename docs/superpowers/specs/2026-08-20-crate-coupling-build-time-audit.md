<!-- markdownlint-disable MD013 -->
# Crate coupling + build-time audit

Target milestone: **v4.0.0**. Tracked in #1458 (composite), #1339, #1444.

## Problem

The main `llmenv` binary crate holds ~67,900 lines, undifferentiated by crate
boundary — everything outside the four already-extracted crates
(`llmenv-config`, `llmenv-paths`, `llmenv-git`, `llmenv-util`). #1339 wants to
reduce cross-module coupling by splitting oversized modules into their own
workspace crates; #1444 wants to know whether that split (or something else)
would meaningfully improve build times. Both issues explicitly call for an
audit before any code moves — this doc is that audit.

**Scope note:** this is an investigation deliverable, not an implementation
one. No crate extraction happens in this PR — the findings below are used to
file concrete, individually-scoped follow-up issues instead. Attempting the
extraction itself in the same PR as the audit would mean cutting corners on
both.

## Method

- **Coupling**: two independent passes, cross-checked against each other.
  1. `cargo-modules dependencies` (AST-based, via `rust-analyzer`) at
     `--max-depth 3`, filtered to module-only "uses" edges
     (`--no-fns --no-types --no-traits --no-externs`).
  2. A `crate::<module>` grep across every top-level `src/` module, rolled up
     to top-level-module granularity.
  3. Where the two disagreed, resolved by direct `grep -rn "crate::X" src/Y/`
     in both directions (see [Discrepancy](#discrepancy-cli--task) below).
- **Build time**: `cargo build --timings`, plus a controlled touch-and-rebuild
  comparison (dev profile, warm dependency cache, incremental) across a leaf
  module vs. three different hub modules, to test whether *which* file
  changes affects rebuild cost under today's single-crate structure.
- Real CI job durations are cited from this repo's own recent Actions runs
  (PR #1457) rather than re-measured, since a genuinely cold, full-dependency
  clean build costs many GB of rebuild and tens of minutes — disproportionate
  to what this audit needs to answer its questions.

## Findings: coupling

`config`, `paths`, `git`, and `util` inside `src/` are thin re-export shims
over the already-extracted `llmenv-config`/`llmenv-paths`/`llmenv-git`/
`llmenv-util` crates (4–76 lines each, all `pub use`). Depending on these is
**not** internal coupling risk — it's already resolved at a crate boundary,
just reached through a compatibility import path. The tables below exclude
them as targets.

### Real internal coupling (`crate::X` used by module Y, excluding shims)

| Module | Depends on |
| --- | --- |
| `adapter` | `cli`, `hook_run`, `materialize`, `mcp`, `merge`, `plugins` |
| `cli` | `adapter`, `auth`, `hook_run`, `icm`, `materialize`, `mcp`, `memory`, `merge`, `plugins`, `scope`, `session_log`, `sync`, `task`, `test_fixtures`, `throttle` |
| `consolidation` | `hook_run`, `mcp` |
| `hook_run` | `adapter`, `cache_trace`, `cli`, `consolidation`, `icm`, `materialize`, `mcp`, `merge`, `scope`, `session_log`, `task`, `test_fixtures`, `test_log_capture` |
| `materialize` | `adapter`, `cache_trace`, `cli`, `mcp`, `memory`, `merge`, `plugins`, `scope`, `session_log`, `task`, `throttle` |
| `merge` | `materialize`, `mcp`, `plugins` |
| `memory` | `hook_run`, `scope` |
| `session_log` | `hook_run`, `mcp`, `scope` |
| `task` | `scope` |
| `throttle` | `adapter`, `hook_run` |
| `auth` | `materialize` |
| `icm` | `scope` |
| `plugins` | `cache_trace`, `scope` |

### Confirmed circular pairs

| Pair | Evidence |
| --- | --- |
| `adapter` ↔ `cli` | both directions confirmed by grep |
| `adapter` ↔ `hook_run` | both directions confirmed by grep |
| `adapter` ↔ `materialize` | both directions confirmed by grep |
| `cli` ↔ `hook_run` | both directions confirmed by grep |
| `cli` ↔ `materialize` | `materialize/status_data.rs` imports `crate::cli::statusline::data`; `cli/doctor.rs` (among others) imports `crate::materialize` |
| `materialize` ↔ `merge` | both directions confirmed by grep |
| `consolidation` ↔ `hook_run` | both directions confirmed by grep |
| `hook_run` ↔ `session_log` | both directions confirmed by grep |

`adapter`, `cli`, `hook_run`, `materialize`, `merge` form one tightly
interconnected cluster — **none of these five can be extracted into its own
crate independently today**; Rust doesn't allow circular crate dependencies,
so each cycle has to be broken (an inverted-dependency trait, an event/callback
boundary, or moving the specific coupled items to a lower layer) before that
pair can split. `consolidation`↔`hook_run` and `hook_run`↔`session_log` are
smaller, more tractable versions of the same problem.

### Discrepancy: `cli` ↔ `task`

`cargo-modules` reported `cli` ↔ `task` as circular. Direct verification
(`grep -rn "crate::cli" src/task/`) found **zero** matches — `task`'s only
internal dependency is `scope`. This is very likely an artifact of
`cargo-modules`' `--max-depth 3` truncation or its `pub use`-re-export
resolution, not a real cycle. Treat `cargo-modules` output as a lead to
verify, not a final answer — both audits here were cross-checked for exactly
this reason.

### Zero-fan-out modules — safe extraction candidates, no cycle-breaking needed

| Module | Lines (approx, per #1339) | Used by (fan-in) | Internal deps (fan-out) |
| --- | --- | --- | --- |
| `scope` | ~1,700 | 8 (`cli`, `hook_run`, `icm`, `materialize`, `memory`, `plugins`, `session_log`, `task`) | none (only the `config` shim) |
| `mcp` | ~3,000 | 7 (`adapter`, `cli`, `consolidation`, `hook_run`, `materialize`, `merge`, `session_log`) | none (only `config`/`paths` shims) |
| `cache_trace` | small | 3 (`hook_run`, `materialize`, `plugins`) | none |
| `sync` | small | 1 (`cli`) | none |
| `test_fixtures` / `test_log_capture` | test-only | 2 / 1 | none |

`scope` and `mcp` are the headline finding: high fan-in, **zero** real
internal fan-out, both independently spot-checked by direct grep. Extracting
either converts real internal coupling into a normal crate dependency for
7–8 downstream modules, with no cycle to break first. `task` (~4,600 lines)
is a near-miss: its only real dependency is `scope`, so it becomes an equally
clean candidate the moment `scope` is extracted (or can be split in the same
PR as `scope`, `task` depending on the new `llmenv-scope` crate).

## Findings: build time

- `cargo build --timings` clean build of `llmenv` alone (dependencies warm):
  **9.67s**.
- Controlled touch-and-rebuild (dev profile, incremental, warm cache) —
  does *which* file changes matter today?

  | File touched | Rebuild time |
  | --- | --- |
  | `src/scope/matcher.rs` (leaf, zero fan-out) | 1.85s |
  | `src/adapter/mod.rs` (hub, 6-way fan-out, 3 cycles) | 1.88s |
  | `src/cli/mod.rs` (hub, 15-way fan-out, 2 cycles) | 1.91s |
  | `src/materialize/mod.rs` (hub, 11-way fan-out, 2 cycles) | 1.85s |

  **No meaningful difference.** Touching the least-coupled leaf module costs
  the same as touching the most-coupled hub module, because there is
  currently only one compilation unit — every edit anywhere in the
  ~67,900 lines forces `rustc` to reprocess the whole `llmenv` crate. This is
  the direct, measured cost of the missing crate boundaries: today, "narrow
  the public API surface between modules" (#1444's suggestion) can't reduce
  recompilation at all, because there's no crate edge for a narrower surface
  to matter at. Splitting `scope`/`mcp`/`task` out would let a change confined
  to one of those become a small, independently-cached crate rebuild instead
  of a whole-`llmenv` one.
- Real CI job durations (PR #1457, `test`/`build`/`coverage`/`lint`/`deny`/
  `hawk` on `ubuntu-latest`, warm `sccache`-backed dependency cache): `lint`
  12s, `deny` 42–58s, `build` 1m9s, `test` 2m6s–4m47s, `coverage` 1m42s–2m46s,
  `hawk` 2m41s–3m25s. These are cited rather than re-measured here — see
  [Method](#method) for why a from-scratch clean build wasn't run locally.
- `mold`/`lld` are not installed locally; `sccache` is. A faster linker is an
  easy, low-risk win worth its own follow-up issue (see below) rather than
  bundling it into the crate-split work.

## Sequencing (#1339 × #1444, per @phaedrus1992's comment on #1339)

1. Extract `scope`, `mcp` first — zero fan-out, no cycles, highest combined
   fan-in (15 dependency edges resolved). This is the safe, high-value first
   move for *both* issues: less internal coupling (#1339) and the first real
   crate boundary for `llmenv`'s own recompilation to key off (#1444).
2. Extract `task` in the same wave (depends only on the newly-created
   `llmenv-scope`).
3. Re-measure the touch-and-rebuild comparison from this doc once (1)+(2)
   land, to confirm the split actually reduces the recompilation cost of
   touching `scope`/`mcp`/`task` — #1444's own acceptance criteria explicitly
   ask for this "does the payoff materialize" check, not just the split.
4. The `adapter`/`cli`/`hook_run`/`materialize`/`merge` cluster needs its
   cycles broken before any of those five can move — this is real design
   work (which side owns the shared type/trait, or whether an event/callback
   inversion is warranted), scoped as its own follow-up rather than attempted
   here.
5. Linker/cache tooling (`mold`/`sccache` adoption) is independent of the
   crate split and can proceed in parallel — filed as its own issue.

## Follow-up issues

Filed as separate, independently-scoped issues per #1444's acceptance
criteria ("prioritized list... each filed as its own issue"):

- #1459 — Extract `scope` into `llmenv-scope`
- #1460 — Extract `mcp` into `llmenv-mcp`
- #1461 — Extract `task` into `llmenv-task` (after `scope`)
- #1462 — Break the `adapter`/`cli`/`hook_run`/`materialize`/`merge` cycle
  cluster (design-first — needs a brainstorming/design pass, not a
  shovel-ready fix)
- #1463 — Adopt `mold`/`lld` + `sccache` in CI and document local setup
