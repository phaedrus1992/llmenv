# Rust — Token-Efficiency Standards

**Applicable when:** `lang-rust` tag is active

## Cargo Command Anti-Patterns

| Pattern | Why it wastes tokens | Alternative |
|---------|---------------------|-------------|
| `cargo test --all` then reading full output | Full test suite output to context | `ctx_execute(language: "shell", code: "cargo test --all 2>&1 \| grep -E 'test result:\|FAILED'")` |
| `cargo clippy --all-targets` then manually reviewing warnings | All warnings returned to context | Run locally before committing; let CI catch new ones |
| `cargo doc --open` then reading docs via context | Docs are already published; don't re-render | Point to docs.rs or docs in repository |

## Rust Anti-Patterns (Correctness + Token Waste)

| Pattern | Why it's a smell | Fix |
|---------|-----------------|-----|
| `.unwrap()` on user input | Panics on invalid input; use `?` and return `Result` | Replace with `Result` return + proper error handling |
| `Vec` when `&[T]` works | Forces callers to allocate; less flexible | Accept `&[T]` or `impl IntoIterator<Item=T>` |
| Cloning large types unnecessarily | Wasted allocations + token cost showing diffs | Use `&T`, `Cow<T>`, or move semantics |
| Over-commenting working code | Comments explaining WHAT code does = wasted tokens explaining redundant code | Remove; let type names + variable names self-document |

## Rust Skill-Gates

| Skill | Gate | Trigger |
|-------|------|---------|
| `rust-skills:*` | Requires `lang-rust` tag | Rust skills only load in Rust projects |
| `/build-check` | Requires `cargo build` to succeed locally first | Prevents submitting broken PRs |

## When to Use Context-Mode Tools in Rust Workflows

✅ **DO:**
- `ctx_execute_file` to count lines, find patterns in test output, summarize logs
- `ctx_batch_execute` to run multiple `cargo` commands in parallel (test, clippy, fmt check)
- `ctx_execute` to parse `cargo tree` output and analyze dependencies

❌ **DON'T:**
- Pipe `cargo` output to Bash for analysis — process in `ctx_execute` instead
- Read full compiler error output to context — extract key errors with `ctx_execute_file`
- Run interactive tools (tests in watch mode, cargo-watch) — these belong in a separate terminal

## Cargo-Specific Patterns

### Testing

```rust
// DON'T: Read full `cargo test` output
$ cargo test

// DO: Use ctx_execute to extract test results
ctx_execute(language: "shell", code: "cargo test 2>&1 | grep -A 5 'test result:'")
```

### Dependency Audit

```rust
// DON'T: Full `cargo tree` output with all dependencies
$ cargo tree

// DO: Use ctx_execute_file to analyze a subset
ctx_execute_file(path: "/tmp/cargo-tree.txt", language: "shell", 
  code: "grep 'YANKED\|DEPRECATED' FILE_CONTENT")
```

