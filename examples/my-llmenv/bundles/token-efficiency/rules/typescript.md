# TypeScript — Token-Efficiency Standards

**Applicable when:** `lang-typescript` or `lang-javascript` tag is active

## Node/npm Anti-Patterns

| Pattern | Why it wastes tokens | Alternative |
|---------|---------------------|-------------|
| `npm list --all` then reading full tree | Full dependency tree to context | `ctx_execute(language: "shell", code: "npm list --depth=0")` |
| `npm audit` full output | All vulnerabilities listed; focus on fixable ones | `npm audit --json \| jq '.vulnerabilities \| select(.fixable)'` |
| Reading compiled JS/dist output | Generated files are noise; read source `.ts` instead | Always work with TypeScript source when possible |

## TypeScript Anti-Patterns

| Pattern | Why it's a smell | Fix |
|---------|-----------------|-----|
| `any` type | Defeats type safety + makes refactoring risky | Use `unknown` or proper types; use `unknown` at boundaries only |
| Optional chaining `?.` without narrowing | Propagates undefined silently | Use `if` checks; type guards are cheaper than propagating uncertainty |
| Over-generic types | Signature bloat obscures intent | Name types explicitly; use `as` sparingly and only at boundaries |

## TypeScript Skill-Gates

| Skill | Gate | Trigger |
|-------|------|---------|
| Frontend frameworks | Requires `dev-server` running | Prevents testing features without the app running |
| API client generation | Requires `tsc --noEmit` to pass | Prevents breaking the build with bad types |

## Context-Mode Tools in TypeScript Workflows

✅ **DO:**
- `ctx_execute_file` to parse `package.json` and extract dependency versions
- `ctx_execute` to run `tsc --noEmit` and extract type errors
- `ctx_batch_execute` to run tests + linting + type checking in parallel

❌ **DON'T:**
- `npm install` then show full terminal output — let NPM run in background, check `npm list` via `ctx_execute`
- Build the app and show full output — use `npm run build 2>&1 | tail -20` instead


