# Issue #2357 — doctor reports the size of always-loaded instruction files

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/2357
- **Milestone:** `v3.12.0`
- **Base branch:** `release/3.x` (forward-merges to `release/4.x`)
- **Type:** feature (doctor section) plus a small parser for rule frontmatter

This is a spec, not a plan.

## Problem

llmenv renders `CLAUDE.md` plus every bundle's rules into `CLAUDE_CONFIG_DIR`.
A normal setup is already about 104 KB across 23 files.
Claude Code 2.1.281 changed its large-CLAUDE.md startup notice to count instruction files together, including `@`-imports.
Nothing in llmenv says how close a setup is to either limit, and Claude Code's notice points at files under `~/.cache/llmenv/`, which users must not edit.

## Claude Code facts

| Fact | Source |
| --- | --- |
| 2.1.281: the large-CLAUDE.md startup notice counts instruction files together, so many mid-sized files and `@`-imports are caught | changelog |
| 2.1.283: `/doctor prompt-audit` audits CLAUDE.md files, skills, agents and commands; it reads the rendered copies | changelog |
| Per-file threshold in the 2.1.287 binary: about 5% of the model's context window in characters, floor about 40,000 characters | issue text, from a binary read |
| Combined threshold: not documented | issue text |

The combined threshold must be pinned before the warning is written.
Read it from the installed binary (`rg -a` around the per-file constant and the notice text; the binary is at `~/.local/share/claude/versions/<version>`) and record the value, the version, and the search used in this table.
If no constant is found, see decision 4.

## Verified facts (release/3.x)

| Fact | Location |
| --- | --- |
| `RuleFile { bundle, rel, frontmatter: Option<String>, body, raw }`; `collect_from_bundle` fills `MergedManifest.rules`; `split_frontmatter` returns the frontmatter as raw text; nothing parses `paths:` | `src/merge/rules.rs`, `src/merge/mod.rs` |
| `CLAUDE.md` is `manifest.agents_md` plus the slippage `COMPACT_SURVIVAL_FRAGMENT`, skipped when empty (#1262); each rule is written verbatim to `<out>/rules/<rel>` | Claude Code adapter `materialize` in `src/adapter/claude_code/mod.rs` |
| `agents_md` has `<!-- # from bundle: <name> -->` separators from `agents_md::concat`, so bundle provenance is recoverable | `src/merge/agents_md.rs` |
| Doctor already holds the merged manifest (`build_manifest` → `doctor_manifest`), so sizes and origins come from memory, not from the rendered files | `src/cli/doctor.rs` |
| `CacheManifest` in `.llmenv-manifest.json` lists every rendered path in `owned` | `src/materialize/manifest.rs` |
| No `@`-imports are rendered or used | adapter |
| No doctor check reads output sizes today; `rendered_claude_dir` and `read_rendered_json` are the only rendered-file readers | `src/cli/doctor.rs` |
| Doctor section pattern: `run_doctor_<name>` prints a header, pure functions return `(CheckLevel, String)`, `print_check` prints | `src/cli/doctor.rs` |
| Docs: doctor is documented in `website/docs/commands.md` `## doctor`; troubleshooting has a doctor section | docs |

## Decisions

1. **Measure from the merged manifest, not from disk.**
   Doctor already has it, and it is what the next `regenerate` will write.
   Count characters (`chars().count()`), because Claude Code's threshold is in characters.
2. **A rule is always loaded unless its frontmatter has a non-empty `paths:` list.**
   Parse only that key, with `serde_yaml` into a small struct that ignores unknown fields.
   A frontmatter that fails to parse counts as always loaded (the safe direction) and is reported once at `{info}`.
3. **Report per scope, which for Claude Code is one `CLAUDE_CONFIG_DIR`.**
   The section prints the combined total, the `CLAUDE.md` size, and the five largest always-loaded contributors grouped by bundle (`CLAUDE.md` chunks by their `from bundle` separator, rules by `RuleFile.bundle`).
4. **Thresholds are named constants with the Claude Code version they came from.**
   `CLAUDE_INSTRUCTION_FILE_LIMIT_CHARS = 40_000` (the floor; doctor does not know the model's window).
   `CLAUDE_INSTRUCTION_TOTAL_LIMIT_CHARS` = the pinned value from the binary.
   If the binary read finds no combined constant, set it to `2 × 40_000` and make the warning say `(estimated; Claude Code does not document the combined limit)`.
   Record which case applied in the code comment.
5. **Levels.** `{warn}` when any single always-loaded file is over the per-file limit or the total is over the combined limit; `{pass}` otherwise, still printing the total so users see the trend.
6. **The fix text points at the source, not the rendered copy.**
   Every warning ends with: `fix it in the source bundle, then run llmenv regenerate; Claude Code's /doctor prompt-audit reads the generated copies.`
7. **Path-filtered rules are listed by count only** (`<n> rules load on matching paths and are not counted`), so users know they are not forgotten.

## Design

### Frontmatter parse (`src/merge/rules.rs`)

```rust
#[derive(Debug, Default, Deserialize)]
struct RuleFrontmatter {
    #[serde(default)]
    paths: Vec<String>,
}

impl RuleFile {
    /// True when Claude Code loads this rule into every session.
    pub(crate) fn always_loaded(&self) -> bool
}
```

`always_loaded` returns true when there is no frontmatter, when `paths` is empty, or when the YAML does not parse.

### Measurement (`src/cli/doctor_instruction_size.rs`, a new module so `doctor.rs` does not grow)

```rust
pub(crate) struct InstructionSizeReport {
    pub total_chars: usize,
    pub claude_md_chars: usize,
    pub always_loaded: Vec<Contributor>,   // sorted by chars desc
    pub path_filtered_rules: usize,
    pub unparsed_frontmatter: usize,
}
pub(crate) struct Contributor { pub bundle: String, pub file: String, pub chars: usize }

pub(crate) fn measure(manifest: &MergedManifest, claude_md: &str) -> InstructionSizeReport;
pub(crate) fn checks(report: &InstructionSizeReport) -> Vec<(CheckLevel, String)>;
```

`claude_md` is the same text the adapter writes (agents_md plus the slippage fragment); expose the adapter's composer as a function if it is inline today, so doctor and materialize agree.

### Doctor (`src/cli/doctor.rs`)

`run_doctor_instruction_size(use_color, manifest)` prints the header `Instruction size (Claude Code):` and the checks, then the top-five list as `{info}` lines: `<bundle>  <file>  <n> chars`.
Call it after the lifecycle-hooks section, since both describe what Claude Code loads.

### Docs

- `website/docs/commands.md` `## doctor`: the new section, the two limits, and the fix path, tagged `(added in v3.12.0)`.
- `website/docs/troubleshooting.md`: "Claude Code says CLAUDE.md is large" → run `llmenv doctor`, trim the named bundle, `llmenv regenerate`.
- Changelog `Added`: doctor reports the size of always-loaded instruction text and the largest bundles.

## Tests

1. `always_loaded`: no frontmatter → true; `paths: []` → true; `paths: ["src/**"]` → false; invalid YAML → true; frontmatter with other keys only → true.
2. `measure` on a synthetic manifest: totals add up; path-filtered rules excluded from the total and counted; contributors sorted by size; bundle attribution of `CLAUDE.md` chunks follows the separators.
3. `checks` table: under both limits → one `{pass}`; one file over the per-file limit → `{warn}` naming the file; total over the combined limit → `{warn}` with the total; both → two warnings.
4. Character counting: a 40,000-character multi-byte string is not over; 40,001 is.
5. Property test: `measure(manifest).total_chars` equals the sum of its always-loaded contributors plus `claude_md_chars`.

## Acceptance criteria

1. On ranger's setup, `llmenv doctor` prints the total and names the largest bundles.
2. A test bundle with a 50,000-character rule makes doctor warn and name the bundle.
3. The combined threshold constant carries a comment with the Claude Code version and how it was found (or that it is estimated).
4. Changelog entry; commands and troubleshooting docs updated with the version tag.

## Out of scope

- Trimming or splitting any bundle's text.
- Counting skills, agents, or commands (Claude Code's `prompt-audit` covers those).
- Crush and opencode (their instruction loading has no documented limit).
