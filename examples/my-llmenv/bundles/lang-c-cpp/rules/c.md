---
paths:
  - "**/*.c"
  - "**/*.h"
---

# C Conventions

## Toolchain

| Purpose | Tool |
|---------|------|
| build | `cmake` or `make` |
| lint | `clang-tidy`, `cppcheck` |
| format | `clang-format` |
| sanitize | `-fsanitize=address,undefined` in debug builds |

Compile with `-std=c17 -Wall -Wextra -Wpedantic -Werror`. Fix every warning. Suppress only with
`// NOLINT(reason)` and a comment.

## Memory Management

C has no constructors or destructors — ownership is entirely manual. Make it explicit.

- **Every allocation has a paired free in the same logical scope** where possible. If ownership
  transfers, document it clearly in the function contract.
- **Check every `malloc`/`calloc`/`realloc` return value** — never dereference without a null check.
- **Prefer `calloc` over `malloc` + `memset`** for zero-initialized arrays — simpler and
  less error-prone.
- **No VLAs (variable-length arrays)** — undefined behavior on stack overflow; size unknown to
  the compiler for analysis. Use `malloc` or a fixed upper bound with a `static_assert`.

```c
// bad
int* buf = malloc(n * sizeof(int));
buf[0] = 1;                          // crash if malloc returned NULL

// good
int* buf = calloc(n, sizeof(int));
if (!buf) { return ENOMEM; }
```

## String & Buffer Safety

Buffer overflows are the most common C security vulnerability.

- **`strncpy` / `strncat` / `snprintf`** over `strcpy` / `strcat` / `sprintf`. Better: use
  `strlcpy` / `strlcat` (BSD) or `snprintf` for all string construction.
- **Always NUL-terminate.** `strncpy` does not guarantee termination — add explicit `buf[n-1] = '\0'`.
- **Know the difference** between buffer size (bytes allocated) and string length (`strlen` result).
  Off-by-one is endemic. Always pass `sizeof(buf)` not `sizeof(buf) - 1` to `snprintf`.
- **No `gets()`** — removed from C11. Use `fgets(buf, sizeof(buf), stdin)`.

```c
char name[64];
snprintf(name, sizeof(name), "%s_%d", prefix, id);  // always safe
```

## `const` Correctness

- **`const` pointer parameters** for data that the function must not modify.
- **`const` globals** for lookup tables and compile-time constants. Prefer over `#define` for
  typed, scoped constants.

```c
size_t count_spaces(const char* s);   // s is not modified
```

## Integer Safety

- **Never mix signed and unsigned** in comparisons — implicit conversions cause silent bugs.
  Compile with `-Wsign-compare` (included in `-Wextra`).
- **Check for overflow** before arithmetic, not after. `a + b > MAX` is already UB if `a` and `b`
  are signed and overflow occurs.
- **Use `<stdint.h>` fixed-width types** (`uint32_t`, `int64_t`) for protocol fields, serialization,
  and any size-sensitive code. Never assume `int` or `long` width.
- **`size_t` for sizes and indices into memory.** Signed loop counters over `size_t` arrays cause
  signed/unsigned comparison warnings and potential wrapping.

```c
// bad: assumes int is 32-bit, implicit sign conversion
unsigned int len = strlen(s);
if (len - 1 > 0) { ... }    // always true if len==0 (wraps to UINT_MAX)

// good
size_t len = strlen(s);
if (len > 1) { ... }
```

## Error Handling

C has no exceptions. Be explicit and consistent within a module.

- **Return `int` status codes** (0 = success, negative = error) or `bool` (C99) for functions
  that can fail. Document the convention in the header.
- **`errno`** for POSIX/system calls — always check immediately after the call before any other
  call that might reset it.
- **Never silently ignore return values** of functions that can fail (`fclose`, `fwrite`, `munmap`,
  `pthread_mutex_lock`). Mark intentional discards with `(void)`.
- Provide context on error: include the operation, the input, and a corrective hint in error
  messages.

```c
if (fclose(fp) != 0) {
    fprintf(stderr, "fclose(%s): %s\n", path, strerror(errno));
    return -1;
}
```

## Header Conventions

- **Include guards** on every header — `#ifndef PROJECTNAME_MODULE_H` / `#define` / `#endif`.
  `#pragma once` is a common extension but not standard C.
- **Headers declare; `.c` files define.** No non-`inline`, non-`static` definitions in headers.
- **Forward-declare structs** instead of including headers when only a pointer is needed.
  Reduces compilation coupling.
- **`#include` order:** own header first, then standard library, then third-party. Keeps headers
  self-contained and exposes missing includes early.

```c
/* mymodule.h */
#ifndef MYPROJECT_MYMODULE_H
#define MYPROJECT_MYMODULE_H
/* ... */
#endif /* MYPROJECT_MYMODULE_H */
```

## Undefined Behavior — The Landmines

These constructs are silent UB in C. Avoid unconditionally:

| Construct | Problem |
|-----------|---------|
| Signed integer overflow `a + b` | UB; compiler may assume it never happens |
| Null pointer dereference | UB even with volatile |
| Out-of-bounds array access | UB; no runtime check in C |
| Strict aliasing violation (`*(int*)float_ptr`) | Compiler reorders/eliminates loads |
| Using a freed pointer | UB after `free()` — set to `NULL` immediately |
| Uninitialized reads | Indeterminate value; UB in some cases |
| Shift by ≥ width (`x << 32` on `int32_t`) | UB |

Always build with `-fsanitize=undefined,address` during development. UBSan catches most of
these at runtime.

## Concurrency (POSIX threads)

- **`pthread_mutex_t`** initialized before use (`PTHREAD_MUTEX_INITIALIZER` or `pthread_mutex_init`).
  Always paired `pthread_mutex_destroy`.
- **No global mutable state** shared across threads without a mutex.
- **`volatile` does not synchronize** between threads in C — use `_Atomic` (C11) or explicit
  barriers for lock-free code.
- Check every `pthread_*` return value. Use a wrapper macro that aborts on failure in debug builds.

## What Not to Do

| Avoid | Use instead |
|-------|-------------|
| `gets()` | `fgets()` |
| `strcpy` / `strcat` | `strlcpy` / `strlcat` / `snprintf` |
| `sprintf` | `snprintf` |
| VLAs | `malloc` + explicit size |
| `malloc` without null check | `malloc` + `if (!ptr) return ERR` |
| Signed/unsigned mixing | Consistent types + `-Wsign-compare` |
| `(void*)` casts to hide type errors | Fix the type |
| `volatile` for threading | `_Atomic` (C11) |
| Magic numbers | Named `const` or `#define` with units in name |
| Silent `errno` ignore | Check + log immediately after syscall |
