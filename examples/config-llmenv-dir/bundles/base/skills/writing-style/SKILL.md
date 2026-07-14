---
name: writing-style
description: >
  Apply consistent writing style to commits, PR descriptions, issue bodies,
  and inline code comments. Invoke before writing any user-facing text to
  ensure it matches the project's established voice.
---

<!--
  This skill lives in bundles/base/skills/writing-style/ so it's available
  in every session (base bundle fires for user-alice always).

  SKILL.md files are injected as agent context when the skill is invoked.
  The frontmatter `name` and `description` fields control how llmenv
  materializes the skill as a plugin slash-command.

  This particular skill encodes a personal writing voice so that Claude
  produces commits and PR descriptions that sound like the author wrote them,
  not like a language model summarizing a diff. Adapt the sections below to
  your own style.

  HOW IT CONNECTS TO THE REST OF THE SYSTEM:
    - Invoked explicitly: user types `/writing-style` or Claude invokes it
      when writing commit messages or PR descriptions.
    - The skill is loaded from this SKILL.md into the agent's context.
    - It doesn't require any special hooks or MCP servers — it's pure
      context injection.
-->

# Writing Style Guide

Use this guide whenever writing commits, PR titles/bodies, issue descriptions,
or inline comments. The goal is prose that reads as if a senior engineer wrote
it in a Slack message: direct, specific, no ceremony.

## Core Principles

- **Direct**: lead with the fact, not a preamble.
- **Specific**: name the thing, not "the functionality" or "the component".
- **Brief**: one sentence is better than three when one works.
- **Technical but not jargon-heavy**: write for a peer who knows the system,
  not for a documentation audience.

## Commit messages

Format: imperative mood, ≤72 characters, present tense.

```text
Add rate limiting to the ingest endpoint
Fix off-by-one in page cursor calculation
Drop unused `legacy_auth` middleware
```

What NOT to do:

```text
# Too long, passive voice, vague
Added some changes to fix the issue with the rate limiting that was
causing problems in the endpoint

# Marketing speak
Implement robust, enterprise-grade rate limiting solution
```

The subject line is a command to the codebase: "Do X." If the subject needs
more than ~72 chars, the change is probably too big for one commit.

## PR descriptions

Lead with what the code does *now*, not what you changed or why you changed it.

**Good:**
> The ingest endpoint now rejects requests above 100 req/s per API key with
> a 429. Burst headroom is 20 extra requests; the window is a sliding 60s.

**Bad:**
> This PR adds rate limiting. I decided to use a sliding window because token
> bucket was too complex. I also considered leaky bucket but rejected it.

Skip: "In this PR", "This change", "As part of this work". Start with the
system behavior.

## Issue descriptions

State the problem, not the investigation. Assume the reader is a peer who
will act on it.

**Good:**
> `cargo test` on a fresh checkout fails with `error[E0432]: unresolved import`
> on `crate::auth::Token`. Introduced in #314. Bisect points to the rename in
> `src/auth/mod.rs:47`.

**Bad:**
> I was trying to run the tests and encountered an error. After some
> investigation I believe there might be an issue with the imports.

## Inline code comments

Only write a comment when the WHY is non-obvious. Never explain WHAT.

```rust
// ponytail: global lock; per-account locks if throughput matters
let _guard = GLOBAL_LOCK.lock().await;
```

Not:

```rust
// Lock the global lock to ensure thread safety
let _guard = GLOBAL_LOCK.lock().await;
```

## Tone markers

- OK: "This breaks X", "Avoids the O(n²) scan", "Skipped Y — only matters if Z"
- Not OK: "This enhancement improves", "Leverages", "Robust", "Seamless"
- Hedging: only when genuine uncertainty exists. "Probably" and "might" are fine
  when you mean them; drop them when you're certain.
