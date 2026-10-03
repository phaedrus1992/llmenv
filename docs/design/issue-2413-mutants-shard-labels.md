# Issue #2413 — mutants shard names read `0/3, 1/3, 2/3` instead of `1/3, 2/3, 3/3`

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/2413
- **Milestone:** `v3.12.0`
- **Base branch:** `release/3.x` (forward-merges to `release/4.x`)
- **Type:** CI fix (display only)

This is a spec, not a plan.

## Problem

The mutants job names count from zero (`mutants (changed lines, shard 0/3)`, `1/3`, `2/3`).
The second number is a count, so the names read as if one shard is missing.
cargo-mutants needs the zero-indexed `k/n` form for `--shard`, so only the display name should change.
The full-sweep job has the same problem, and when it is skipped its name shows the literal `${{ matrix.shard }}`.

## Verified facts (release/3.x)

| Fact | Location |
| --- | --- |
| `scripts/mutants-plan.sh <base-ref> [--per-shard N] [--max-shards N]` prints `shards=<N>` and `matrix=[0,1,…]` (zero-indexed) on stdout; it validates the base ref and the numbers and prints a diagnostic on stderr | `scripts/mutants-plan.sh` |
| The plan job exposes `steps.plan.outputs.shards` and `steps.plan.outputs.matrix`; the PR job's `strategy.matrix.shard` is `fromJSON(needs.plan.outputs.matrix)`; its `name:` is `mutants (changed lines, shard ${{ matrix.shard }}/${{ needs.plan.outputs.shards }})`; the run step passes `--shard "$SHARD/$SHARDS"` and uploads `mutants-in-diff-${{ matrix.shard }}` | `.github/workflows/mutants.yml` |
| The sweep job has a literal `shard: [0, 1, 2, 3, 4, 5, 6, 7]`, name `mutants (full sweep, shard ${{ matrix.shard }}/8)`, and runs `--shard ${{ matrix.shard }}/8` | `.github/workflows/mutants.yml` |
| A comment in the workflow already explains why the list is zero-indexed (shard `8/8` failed once) | `.github/workflows/mutants.yml` |
| Workflow tests live in `.github/workflows/__tests__/` (one shell test file for the forward-merge guards); the plan script has no test today | `.github/workflows/__tests__/` |
| `actionlint` and `zizmor` are the repo's workflow linters (CLAUDE.md) | tooling |

## Decisions

1. **The plan script emits one-based label strings.**
   `matrix=["1/3","2/3","3/3"]`.
   GitHub expressions have no arithmetic, so the one-based label must come from the script.
2. **The job name is static, and the matrix holds only the label.**
   GitHub appends the matrix values to a static name, so a running job reads `mutants (changed lines) (1/3)`.
   A skipped job shows no matrix text, where a `${{ matrix.label }}` in the name showed the literal expression.
   Both jobs take the zero-based shard for `--shard` from `strategy.job-index`.
   The sweep job lists its eight labels inline, with a comment.
3. **The artifact names use `strategy.job-index`** (zero-based, no slash), since a label such as `1/3` is not a valid artifact name.
4. **The plan script gets a test** in `.github/workflows/__tests__/mutants-plan.sh`, run the same way the existing guard test is run (check how `forward-merge-release-guards.sh` is invoked, in CI or by hand, and follow it).
5. **`shards=0` still gives `matrix=[]`**, and the PR job's `if:` on `shards != '0'` is unchanged.

## Design

### `scripts/mutants-plan.sh`

- Build the matrix with a loop producing `"k+1/N"` for `k` in `0..N-1`.
- Keep `shards=<N>` as the first output line and `matrix=…` as the second.
- Update the usage comment at the top of the file.

### `.github/workflows/mutants.yml`

- PR job: `name: mutants (changed lines)`; the matrix key is `label`, fed from `fromJSON(needs.plan.outputs.matrix)`; `SHARD: ${{ strategy.job-index }}`.
- Sweep job: a `label` list of eight strings and `name: mutants (full sweep)`.
- Update the two comments about zero indexing to mention the label.

## Tests

1. `__tests__/mutants-plan.sh`: with a fake diff of 13 mutants and `--per-shard 6`, the script prints `shards=3` and the matrix `["1/3","2/3","3/3"]`; with zero mutants, `shards=0` and `matrix=[]`; with `--max-shards 2` the labels read `1/2`, `2/2`.
   Stub `cargo mutants --list` the way the script's own usage allows (an env var or a `PATH` shim), so the test needs no real mutants run.
2. `actionlint` and `zizmor` pass on the workflow.
3. A PR run shows job names `shard 1/N … N/N`.

## Acceptance criteria

1. Job names on a PR read `1/N … N/N`; `--shard` still receives `k/N` with `k` from 0.
2. The skipped sweep job's name no longer shows a literal expression.
3. The plan script test passes.

## Out of scope

- Changing the shard count heuristic.
- Making the sweep job's shard count dynamic.
