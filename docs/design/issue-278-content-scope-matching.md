# Issue #278 — file-glob (`content`) scope matching

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/278
- **Milestone:** Large Projects
- **Type:** Feature
- **Difficulty:** Moderate. The issue body carries a full, already-decided
  **Design Decision + Specification + Implementation Plan** — read the
  entire issue before starting; this doc only anchors it to code.

## Summary

New scope kind `scope.content`: auto-activate tags when files matching a
glob exist under the cwd, for repos without a `.llmenv.yaml` marker
(forks, third-party clones, mixed monorepos).

```yaml
scope:
  content:
    - id: rust-project
      match: { glob: "Cargo.toml", depth: 2 }
      tags: [lang-rust]
```

Semantics (decided in the issue — don't relitigate): existence check only
(no content inspection), recursive walk with optional `depth` cap,
early-exit on first match, evaluated point-in-time at export alongside the
other scope kinds, each matching content scope contributes tags
independently.

## Code anchors (verified)

- Scope resolution loop: `src/scope/mod.rs` (~line 73 onward) iterates
  `cfg.scope.network` / `cfg.scope.host` / etc., calling
  `matcher::matches_*` and pushing `ActiveScope { kind: "...", ... }`.
  Content scopes slot into this same loop with `kind: "content"` — copy
  the existing per-kind block shape exactly.
- Matchers live in `src/scope/matcher.rs` (existing per-kind `matches_*`
  functions + their inline tests are the pattern).
- Schema `Scope` struct: `crates/llmenv-config/src/schema.rs`.
- Doctor warnings: `src/cli/doctor.rs` — reuse its existing warning
  infrastructure.

## Implementation order

Follow the issue's own 8-step Implementation Plan. Condensed:

1. Schema: `ContentScope { id, match: ContentMatch, tags }`,
   `ContentMatch { glob, depth: Option<usize> }`, and
   `Scope.content: Vec<ContentScope>` — check whether the existing scope
   kind fields on `Scope` use `Vec` (default empty) or `Option<Vec>`, and
   **match the existing convention** rather than the issue's literal
   `Option<Vec<...>>`. `#[serde(default)]` either way for back-compat.
   Validate at load: invalid glob = hard error naming pattern + scope id;
   empty `id`/`tags` rejected consistent with other scope kinds.
2. Matcher: `evaluate_content_scopes(&[ContentScope], cwd) -> ...` using
   `globset` + `walkdir` (**both already dependencies** — verify in
   `Cargo.toml`; add nothing new). Depth cap via `WalkDir::max_depth`;
   early exit on first match; unreadable dirs skipped (debug log), never
   fatal.
3. Wire into `src/scope/mod.rs` resolution; fired scopes contribute tags
   through the existing path and appear in `LLMENV_ACTIVE_SCOPES` as
   `content:<id>` (find where the env var is assembled and confirm the
   `kind:<id>` format other kinds use — mirror it).
4. Doctor: warn on any content scope with no `depth` ("may be slow in
   large repositories; set `depth`"). Always warn — no repo-size
   heuristic (issue chose the simple option).
5. Tests per the issue's Step 5 list: fires on match, silent on non-match,
   depth cap respected (nested `a/b/c/file.txt` with `depth: 1` doesn't
   fire; unlimited does), invalid glob rejected at parse, multiple scopes
   aggregate. Use tempdirs; put unit tests inline in `matcher.rs` like the
   existing matcher tests, plus one integration test in `tests/` if an
   export-level scope test file already exists to extend.
6. Back-compat check: existing fixture configs (no `content` key) parse
   and resolve unchanged — run the full suite.

## Gotchas

- Glob matching must be against paths **relative to cwd** (the walk root),
  not absolute paths — `**/*.py` won't match `/Users/x/repo/a.py` if you
  feed globset the absolute path. Strip the prefix before matching.
- `depth` semantics: define depth 1 = entries directly in cwd, and encode
  that in the depth-cap test (issue's test says `a/b/c/file.txt` is depth
  3). `WalkDir::max_depth(n)` matches this definition.
- Doctor changes: `llmenv doctor` may run in repos with no content scopes
  — the warning loop must handle absent config gracefully.

## Acceptance criteria

The issue's checklist, plus:

- [ ] CHANGELOG `[Unreleased]` entry (keepachangelog skill) +
      forward-merge reconciliation per `AGENTS.md`.
- [ ] User-facing docs: document `scope.content` wherever the other four
      scope kinds are documented, same format.
- [ ] No new dependencies; clippy/fmt clean; full suite green.

## Out of scope (per issue non-goals)

- File *content* inspection; watching/inotify.
