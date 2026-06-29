---
paths:
  - "**/*.ts"
  - "**/*.tsx"
---

# TypeScript Rules

## Compiler Strictness

Enable all in `tsconfig.json`:

```jsonc
"strict": true,
"noUncheckedIndexedAccess": true,
"exactOptionalPropertyTypes": true,
"noImplicitOverride": true,
"noPropertyAccessFromIndexSignature": true,
"verbatimModuleSyntax": true,
"isolatedModules": true
```

ESM only (`"type": "module"` in `package.json`). No CommonJS, no `require`.

## Naming

| Style | Use for |
|-------|---------|
| `UpperCamelCase` | Classes, interfaces, types, enums, type parameters, React components |
| `lowerCamelCase` | Variables, parameters, functions, methods, properties |
| `CONSTANT_CASE` | Global constants, enum values, `static readonly` class properties |

- Acronyms = words: `loadHttpUrl`, not `loadHTTPURL`
- No `_` prefix/suffix — use `private` keyword
- No Hungarian notation or type-encoding (`strName`, `IFoo`, `EBar`)
- Names descriptive to new reader. No vague abbreviations. Single letters only in scopes <10 lines
- Avoid vague names (`Manager`, `Service`, `Handler`, `Processor`) when domain name exists

## Type Design

### No `any`

Never use `any`. Use `unknown` when type unknown, narrow with guards before use:

```typescript
// Bad
function parse(input: any): void { input.foo(); }

// Good
function parse(input: unknown): void {
  if (typeof input === "object" && input !== null && "foo" in input) {
    (input as { foo: () => void }).foo();
  }
}
```

If `any` unavoidable (e.g., test mocks), inline comment why.

### Type Assertions: Justify, Don't Chain

Use assertions (`as T`) sparingly with clear comments. Never chain through `unknown` (`as unknown as T`) — defeats narrowing, triggers refactoring:

```typescript
// Bad — bypasses type system entirely
const agent = makeTestFixture() as unknown as Actor;

// Good — narrow with runtime check or add a proper helper
const agent = makeTestFixture();
if (!isValidActor(agent)) throw new Error("Invalid fixture");
// agent is now properly narrowed to Actor type

// Acceptable — minimal fixture, justified
// Type narrowing: fixture implements minimal Actor interface for roll execution.
// Full Actor type = 128+ Foundry properties unused here.
const agent = makeTestFixture() as unknown as Actor;
```

If writing `as unknown as`: (1) Type wrapper/constructor available? (2) Runtime check to narrow? (3) Fixture/model incomplete? Fix root cause, don't bypass type system.

### Interfaces vs Type Aliases

- Use `interface` for object shapes (better display, perf, extensibility)
- Use `type` for unions, intersections, tuples, mapped types, primitives

```typescript
// Object shapes → interface
interface User {
  name: string;
  email: string;
}

// Unions, tuples, primitives → type
type Result = Success | Failure;
type Pair = [string, number];
type UserId = string;
```

### Branded/Opaque Types

Domain identifiers: branded types over primitives (like Rust newtypes):

```typescript
type UserId = string & { readonly __brand: unique symbol };
type TenantId = string & { readonly __brand: unique symbol };

function createUserId(raw: string): UserId {
  return raw as UserId;
}
```

Prevents mixing `UserId` and `TenantId`.

### Nullability

- Use `T | null` or `T | undefined` at point of use. Don't bake into aliases
- Optional (`?`) for omittable fields/params. Use `| undefined` for fields always present but may lack value
- Handle null near source, don't propagate through layers

### No Wrapper Types

Never use `String`, `Boolean`, `Number`, `Object`. Always use lowercase primitives: `string`, `boolean`, `number`, `object`.

### Arrays

Use `T[]` for simple types. Use `Array<T>` for complex types (unions, objects):

```typescript
const names: string[];
const items: Array<string | number>;
```

### Enums

Use `enum`, not `const enum`. Always include `default` or exhaustive check in switches.

### Generics

Every type param used. No phantom generics. Avoid return-type-only generics—always specify type arg explicitly.

### Prefer Simplest Type Construct

Avoid `Pick`, `Omit`, mapped, conditional types when spelling fields simpler. Complex utility types hurt IDE support.

## Error Handling

- **Fail fast with context.** Throw `new Error("message")` (never bare `Error()`). Include operation, input, fix when possible
- **Never swallow exceptions.** Every `catch` rethrows, logs+rethrows, or handles. Empty `catch {}` forbidden
- **Use `unknown` for caught errors.** TypeScript catch binds `unknown`—narrow before accessing:

```typescript
try {
  await fetchData();
} catch (err: unknown) {
  const message = err instanceof Error ? err.message : String(err);
  throw new Error(`Failed to fetch data: ${message}`);
}
```

- **Callbacks ignoring errors use `void` return type**, not `any`
- **Validate at system boundaries** (user input, API, files). Trust internal types

## Imports and Exports

- **Named exports only.** No `export default`—inconsistent names, breaks find-references
- **No mutable exports.** Never `export let`. Use getters if value changes
- **No container classes.** Don't wrap statics in class for namespacing—export individually
- **No `import type`/`export type`.** TypeScript auto-distinguishes. Exception: `export type Foo = ...` OK
- **No namespace or `require`.** ESM `import`/`export` only
- **Destructure frequently used** (utils, framework). Use namespace imports (`import * as foo`) for large APIs

## Control Flow

### Strict Equality

Always `===` and `!==`. Exception: `== null` checks both `null` and `undefined`.

### Exhaustive Switches

Every `switch` must have a `default` case. For discriminated unions, use an exhaustiveness check:

```typescript
function assertNever(x: never): never {
  throw new Error(`Unexpected value: ${JSON.stringify(x)}`);
}

switch (action.type) {
  case "create": return handleCreate(action);
  case "delete": return handleDelete(action);
  default: return assertNever(action);
}
```

Non-empty cases must not fall through. Empty cases may group.

### No `forEach`

Never use `forEach` on arrays, sets, maps. Prevents early returns, breaks reachability analysis, harder debug. Use `for...of`:

```typescript
// Bad
items.forEach((item) => { process(item); });

// Good
for (const item of items) {
  process(item);
}
```

### No `for...in`

Never use `for...in`—iterates prototype chain, string indices for arrays. Use `for...of`, `Object.keys()`, `Object.entries()`.

### Blocks Required

Multi-line control flow requires braces. Single-line OK: `if (done) return;`

## Variables

- Always `const` or `let`, never `var`
- Default `const`. Use `let` when reassignment needed
- No use before declaration

## Classes and Visibility

- **Minimize exported surface.** Only export consumers need. Convert private methods to module functions
- **Never write `public`**—it's default. Exception: non-readonly constructor param properties
- **Use `readonly`** on properties not reassigned after construction
- **Use parameter properties** to avoid boilerplate
- **Initialize at declaration** when possible, eliminate constructor
- **No `#private` fields.** Use `private` keyword—`#` causes size/perf regressions downleveled
- **Getters pure** (no side effects). Avoid trivial getter/setter pairs—make `readonly` or public
- **No arrow-function class properties** unless stable `this` reference needed (event handlers)

## Functions

- Use `function` for named (top-level, nested). Can't be reassigned
- Use arrows for callbacks, expressions
- Arrow callbacks ignoring return use block body, not expression
- Never `bind()` for handlers—creates unreferenceable temps. Use arrows or arrow properties
- Semicolons required—don't rely on ASI

## Magic Values

Hardcoded numbers, strings, timeouts need comment explaining *why*:

```typescript
// 30s matches server-side timeout
const POLL_TIMEOUT_MS = 30_000;
```

## Comments and Documentation

- **JSDoc (`/** */`)** for public API—document purpose, not types (TypeScript has them)
- **Line comments (`//`)** for implementation notes only
- No `@param`/`@return` restating name/type. Add only beyond signature info
- No `@override`—not enforced, drifts
- No commented-out code—delete. Git has history
- Place JSDoc before decorators, not between

## Coercion

- String: `String(x)` or templates. Never `"" + x`
- Number: `Number(x)`, check `isNaN`. Never unary `+`. `parseInt` only non-base-10
- Boolean: implicit truthiness OK in conditionals. No `!!x` in `if`/`while`/`for`. Explicit OK (`arr.length > 0`)

## Spread

- Spread objects into objects, iterables into arrays only
- Never spread `null`, `undefined`, primitives
- Conditional spread via ternary: `{ ...base, ...(condition ? extra : {}) }`, not `...(condition && extra)`

## Testing

- **Test behavior, not implementation.** Refactors shouldn't break tests
- **Test edges and errors.** Empty, boundaries, malformed, missing values
- **Mock boundaries only.** Network, filesystem, externals. Never internal logic
- **Colocate tests.** `*.test.ts` next to source
- **Use `vitest`** runner

## Anti-Patterns

| Never do this | Do this instead |
|---------------|-----------------|
| `any` | `unknown` + type narrowing |
| `export default` | Named exports |
| `var` | `const` / `let` |
| `.forEach()` | `for...of` |
| `for...in` | `for...of` / `Object.entries()` |
| `@ts-ignore` | Fix the type error |
| Type assertions (`as Foo`) without justification | Runtime type guards / narrowing |
| `as unknown as Foo` chains | Create type-safe wrappers or add proper narrowing — chaining through `unknown` bypasses the type system |
| Non-null assertions (`x!`) without justification | Null checks |
| `== ` / `!=` (except `== null`) | `===` / `!==` |
| `new String()` / `new Boolean()` / `new Number()` | Primitive literals |
| `Array()` constructor | Array literals `[]` |
| `namespace` | ES modules |
| `require()` | `import` |
| `debugger` | Remove before commit |
| Empty `catch {}` | Handle or rethrow |
| `console.log` in production code | Structured logging |
