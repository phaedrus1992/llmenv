# Bash — Token-Efficiency Standards

**Applicable when:** `lang-bash` or `domain-devops` tag is active

## Bash Anti-Patterns (Token Waste)

| Pattern | Why it wastes tokens | Alternative |
|---------|---------------------|-------------|
| `cat file.log \| grep ...` | Pipes return full output; process in code sandbox instead | `mcp__plugin_context-mode_context-mode__ctx_execute_file(path, language: "shell", code: "grep ... file.log")` |
| `for i in $(cat list.txt); do cmd $i; done` | Outputs every iteration; batch with ctx_batch_execute | `ctx_batch_execute(commands: [...])` with parallel concurrency |
| `find . -name "*.go" \| wc -l` | Finds all matches in output; count in code | `mcp__plugin_context-mode_context-mode__ctx_execute(language: "shell", code: "find . -name '*.go' \| wc -l")` |
| `git log --oneline \| head -20 \| grep ...` | Full log returned to context; filter in sandbox | `ctx_execute(language: "shell", code: "git log ... \| grep ...")` |

## Bash Skill-Gates

| Skill | Gate | Trigger |
|-------|------|---------|
| `/run` | `BASH_BAN` contains `cat,find,grep` | Prevents bare file inspection; user must use appropriate tool |
| `/ctx-stats` | Requires `mcp__plugin_context-mode_context-mode__ctx_stats()` invocation first | Ensures user understands current context consumption |

## When to Use Bash

✅ **DO use Bash for:**
- Version control (`git` commands)
- Directory/file mutations (`mkdir`, `rm`, `mv`)
- Networking (`curl`, `ssh`)
- Process management (`kill`, `wait`)

❌ **DON'T use Bash for:**
- Reading and analyzing files (use `mcp__plugin_context-mode_context-mode__ctx_execute_file`)
- Searching code (use `Bash` + `rg` for exact string, but pipe output through `mcp__plugin_context-mode_context-mode__ctx_execute` for processing)
- Counting/aggregating (use `mcp__plugin_context-mode_context-mode__ctx_execute` with language-native processing)
- Looping over results (use `mcp__plugin_context-mode_context-mode__ctx_batch_execute` for parallel work)

See [skill-gates.md](skill-gates.md) for the full context-mode tool reference.

