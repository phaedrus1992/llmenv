<!-- markdownlint-disable MD003 MD013 MD022 MD041 -->
---

name: source-code
description: This skill should be used when starting development on a codebase, investigating or exploring source code, understanding how something is implemented, tracing code paths, or preparing to make code changes. ALSO use whenever the user refers to "the reference", "reference code", "reference folder", "reference repo", or asks to look in/check/investigate code that lives in a DIFFERENT project or piece of software than the current working directory — e.g. "look in the reference folder", "check how X works in `<other project>`", "what does the changes plugin do in the reference code", investigating a dependency, upstream project, or sibling repo. Enforces the required codebase-memory-mcp indexing protocol before any source code work begins, and the ~/git/reference/ lookup protocol for cross-repo investigation
---

# Source Code Investigation Protocol

Before reading or writing any source code, index and query the codebase through `codebase-memory-mcp`. This applies to every session — even if the codebase was indexed previously.

## Phase 1: Index Before Any Code Work

At the start of any session involving source code, before reading or writing any files:

1. Check what projects are already indexed:

```text
   mcp__codebase-memory-mcp__list_projects
   ```

1. Index the current working directory (or the relevant project root):

```text
   mcp__codebase-memory-mcp__index_repository { "path": "<project root>" }
   ```

1. Verify the index completed:

```text
   mcp__codebase-memory-mcp__index_status { "path": "<project root>" }
   ```

Only proceed to file reads or edits after indexing confirms success.

## Phase 2: Investigate Via codebase-memory-mcp First

When investigating source code — looking up a function, tracing a data flow, understanding an abstraction — query `codebase-memory-mcp` before opening files:

- **Search for symbols, functions, or patterns:**

```text
  mcp__codebase-memory-mcp__search_code { "query": "<symbol or concept>" }
  ```

- **Get the overall architecture:**

```text
  mcp__codebase-memory-mcp__get_architecture { "path": "<project root>" }
  ```

- **Trace how code flows between components:**

```text
  mcp__codebase-memory-mcp__trace_path { "from": "<entry>", "to": "<destination>" }
  ```

- **Query relationships in the knowledge graph:**

```text
  mcp__codebase-memory-mcp__search_graph { "query": "<concept>" }
  mcp__codebase-memory-mcp__query_graph { "query": "<Cypher or natural language>" }
  ```

- **Pull a specific snippet once you know where it lives:**

```text
  mcp__codebase-memory-mcp__get_code_snippet { "file": "<path>", "symbol": "<name>" }
  ```

Use `Read` or `grep` only to fill gaps that `codebase-memory-mcp` cannot answer.

## Phase 3: Exploring Related Code (Cross-Repo)

When the investigation points to a related codebase (a dependency, upstream project, or sibling repo):

### Step 1 — Check `~/git/reference/` first

If the directory exists, look for the repo there before anything else:

```bash
ls ~/git/reference/<repo-name> 2>/dev/null
```

**If found**: pull latest and check out the version that matches the dependency pin
(e.g., if the main project depends on `bar@1.3.x`, check out tag `v1.3` or the closest match):

```bash
git -C ~/git/reference/<repo-name> fetch --tags
git -C ~/git/reference/<repo-name> checkout v<version>   # or the appropriate tag
```

Then re-index (incremental, cheap):

```text
mcp__codebase-memory-mcp__index_repository { "path": "~/git/reference/<repo-name>" }
```

### Step 2 — Check ICM memory and codebase-memory-mcp

If `~/git/reference/<repo-name>` does not exist, check memory before cloning anything:

```text
mcp__icm__icm_memory_recall { "topic": "<project or concept>" }
mcp__icm__icm_memoir_search { "query": "<project name or symbol>" }
mcp__codebase-memory-mcp__list_projects
```

If the repo already appears in `codebase-memory-mcp`, query it directly without new checkout.

### Step 3 — Clone only as a last resort

If the repo is not in `~/git/reference/` and not in `codebase-memory-mcp`, and
`~/git/reference/` exists, clone there:

```bash
git clone <repo-url> ~/git/reference/<repo-name>
git -C ~/git/reference/<repo-name> checkout v<version>
```

Then index:

```text
mcp__codebase-memory-mcp__index_repository { "path": "~/git/reference/<repo-name>" }
mcp__codebase-memory-mcp__index_status { "path": "~/git/reference/<repo-name>" }
```

### Step 4 — Update ICM memory

After any cross-repo exploration, store the relevant state in ICM so future sessions
skip the discovery work:

```text
mcp__icm__icm_memory_store {
  "topic": "<repo-name>",
  "content": "Location: ~/git/reference/<repo-name>. Version checked out: v<version>. Indexed in codebase-memory-mcp. Used for: <why it was needed>."
}
```

Record: local path, checked-out version, index status, and the dependency context that required it.

Also update ICM after Phase 1 indexing if a project's location or version is new or changed.

## Detect Stale Indexes

If the codebase has changed since last index (e.g., after a pull or branch switch):

```text
mcp__codebase-memory-mcp__detect_changes { "path": "<project root>" }
```

Re-index if changes are detected before continuing investigation.
