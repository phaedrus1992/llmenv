---
paths:
  - "**/*.cpp"
  - "**/*.cc"
  - "**/*.cxx"
  - "**/*.hpp"
  - "**/*.hxx"
  - "**/*.h"
---

# C++ Conventions (C++17)

## Toolchain

| Purpose | Tool |
| --------- | ------ |
| build | `cmake` (3.15+) or `meson` |
| lint | `clang-tidy` |
| format | `clang-format` |
| sanitize | `-fsanitize=address,undefined` in debug builds |
| analyze | `cppcheck` |

Always compile with at minimum: `-Wall -Wextra -Wpedantic -Werror`. Enable `-std=c++17` explicitly.
Fix every warning — suppress only with `// NOLINT(reason)` and a comment explaining why.

## Resource Management — RAII First

Never manage resources manually. Every resource (memory, file, socket, lock) must have an owner
with automatic cleanup.

- **No raw `new`/`delete`** — use `std::make_unique<T>()` and `std::make_shared<T>()`.
- **No `malloc`/`free`** — incompatible with constructors/destructors and `new`/`delete`.
- Prefer `unique_ptr` over `shared_ptr` unless you genuinely need shared ownership; `shared_ptr`
  has reference-count overhead and makes lifetime reasoning harder.
- Use `std::lock_guard` / `std::scoped_lock` for mutexes, never manual `lock()`/`unlock()`.

```cpp
// bad
int* p = new int[100];
// ...
delete[] p;

// good
auto p = std::make_unique<int[]>(100);
```

## Type Safety

- **`const` by default** — every variable, parameter, and member function that can be `const`
  should be. `const&` for input parameters of non-trivial types.
- **No C-style casts** `(T)x` — use `static_cast`, `reinterpret_cast` (justify in a comment),
  `const_cast` (almost never). Each cast is explicit about what it does.
- **`string_view` over `const char*` or `const string&`** for read-only string parameters (C++17).
  Zero-cost abstraction that accepts both `std::string` and string literals.
- **Prefer `enum class`** over plain `enum` — scoped, no implicit int conversion.
- **`[[nodiscard]]`** on functions whose return value must be checked (error codes, resource handles).

```cpp
void process(std::string_view name);          // accepts string and char* callers
[[nodiscard]] std::error_code write(Span s);  // caller must check
```

## Bounds Safety — No Pointer Arithmetic

Raw pointer arithmetic is undefined behavior waiting to happen.

- **`std::span<T>`** (C++20) or `gsl::span<T>` instead of `(T*, size_t)` pairs.
- **Never index past bounds.** Prefer range-`for` or algorithms over manual indexing.
- **No array-to-pointer decay** at function boundaries — accept `span`, not `T*`.
- Use `.at()` for bounds-checked access in non-performance-critical paths.

```cpp
// bad
void process(int* data, size_t len);

// good
void process(std::span<const int> data);
```

## Functions & Interfaces

- **Return values over out-parameters.** Use structured bindings or `std::pair`/`struct` for
  multiple returns. C++17 NRVO makes return-by-value free for local objects.
- **≤5 parameters** — beyond that, introduce a parameter struct.
- **Pure virtual interfaces** — prefer empty abstract base classes (no data members) over
  concrete base classes. Avoids the fragile base class problem.

```cpp
// bad
void find(const vector<int>& v, vector<int*>& out_matches, int x);

// good
std::vector<int*> find(const std::vector<int>& v, int x);
```

## Error Handling

Choose one strategy per module and be consistent:

| Context | Strategy |
| --------- | ---------- |
| General application code | Exceptions + RAII |
| Performance-critical / embedded | `std::expected<T,E>` (C++23) or `std::optional<T>` + error enum |
| Constructor failure | Throw — there is no "return" from a constructor |

- **`noexcept`** on functions that cannot throw (move operations, destructors, pure arithmetic).
  Allows compiler optimisations and is required for strong exception safety of containers.
- **Never `throw` in a destructor.** Destructors must be `noexcept`.
- **Never swallow exceptions silently** — at minimum log + rethrow.
- Constructors establish invariants. If the invariant cannot be established, throw. Don't create
  zombie objects that require an `init()` call.

```cpp
std::string to_upper(std::string s) noexcept;   // cannot throw
```

## C++17 Features — Use Them

### Structured Bindings

```cpp
auto [iter, inserted] = my_map.emplace(key, value);
if (!inserted) { /* key already existed */ }
```

### `if` / `switch` with Init Statement

```cpp
if (auto it = m.find(key); it != m.end()) {
    use(it->second);
}
```

### `std::optional<T>`

Replace sentinel values (`-1`, `nullptr`, `""`) and boolean out-parameters with `optional`.

```cpp
std::optional<Config> load_config(std::string_view path);
```

### `std::variant<Ts...>` + `std::visit`

Type-safe tagged unions. Prefer over `union` + manual tag.

```cpp
using Result = std::variant<Value, ParseError>;
std::visit(overloaded{
    [](Value v)      { use(v); },
    [](ParseError e) { report(e); },
}, result);
```

### `std::filesystem`

```cpp
#include <filesystem>
namespace fs = std::filesystem;
fs::path p = fs::current_path() / "data" / "config.json";
```

### `if constexpr`

Replaces SFINAE and `enable_if` for compile-time branching inside templates.

```cpp
template<typename T>
void serialize(T val) {
    if constexpr (std::is_integral_v<T>) { write_int(val); }
    else                                 { write_float(val); }
}
```

### Fold Expressions (variadic templates)

```cpp
template<typename... Args>
bool all_positive(Args... args) { return (... && (args > 0)); }
```

### Class Template Argument Deduction (CTAD)

```cpp
std::pair p{1, 2.0};     // deduced pair<int, double>
std::vector v{1, 2, 3};  // deduced vector<int>
```

## `override` and `final`

- **Always mark overriding virtual functions `override`** — the compiler will error if the
  signature drifts from the base, catching silent non-override bugs.
- **`final`** on classes or virtual functions where further derivation is not intended. Enables
  devirtualization optimisations and makes intent explicit.
- **Never re-declare `virtual` on an override** — `override` implies virtual; spelling both is
  redundant noise.

```cpp
struct Base {
    virtual void draw() = 0;
    virtual ~Base() = default;
};

struct Widget final : Base {
    void draw() override;   // good — override, not virtual
};
```

## Classes — Rule of Zero / Five

**Rule of Zero:** Prefer classes with no user-defined destructor, copy, or move operations.
Let member types (smart pointers, containers) handle resource cleanup automatically.

**Rule of Five:** If you define *any* of destructor, copy-ctor, copy-assign, move-ctor, move-assign
— define or `=delete` *all five*. Partial definitions cause subtle bugs.

```cpp
struct Buffer {
    std::vector<std::byte> data;  // Rule of Zero: vector handles everything
};
```

- **Polymorphic base classes** must have a `virtual` destructor (or `protected` non-virtual).
- Mark move operations `noexcept` — containers check this to choose move vs. copy.
- `= default` is preferable to writing trivial implementations.

## Concurrency

- **`volatile` does not synchronize.** Use `std::atomic<T>` for shared variables across threads.
- **`std::mutex` + `std::lock_guard`** for critical sections. `std::scoped_lock` (C++17) for
  multiple mutexes at once — avoids deadlock via internal acquisition ordering.
- `std::jthread` (C++20) over `std::thread` — auto-joins on destruction, carries `stop_token`.
- **Never raw `lock()`/`unlock()`** — use RAII wrappers so exceptions can't leak a locked mutex.

```cpp
std::atomic<int> counter{0};   // correct shared counter
// NOT: volatile int counter = 0;
```

For full threading API detail — condition variables, futures, parallel STL, C++20 latches/barriers,
memory model (acquire/release/relaxed), thread-safe initialization:
[`cpp-concurrency.md`](cpp-concurrency.md)

## Source File Conventions

- **`#pragma once`** is acceptable and portable enough for modern toolchains. If strict ISO C++
  portability matters, use `#ifndef PROJECTNAME_PATH_FILE_H` guards instead.
- **Headers declare; `.cpp` files define.** No non-`inline`, non-`constexpr` definitions in headers
  — causes ODR violations.
- **Forward-declare instead of including** where the full type is not needed (pointer/reference
  parameters). Reduces compile-time coupling.
- Include order: own header → standard library → third-party → project. Keeps header
  self-containment visible.

## Hot-Path / Real-Time Safety

In any tight loop where latency or jitter matters (audio callbacks, game tick, interrupt handlers,
lock-free queues):

- **No allocations** — `malloc`/`new`/container growth all potentially block on the OS allocator.
  Pre-allocate at init time; use fixed-size ring buffers or pre-allocated pools at runtime.
- **No locks** — `std::mutex::lock()` can block indefinitely. Use lock-free queues
  (`std::atomic`, SPSC ring buffer) for hot-path cross-thread communication.
- **No blocking I/O** — no file reads, no sockets, no `printf` (on some platforms it acquires
  a lock). Use a separate logging thread with a lock-free queue.
- **No exceptions in hot paths** — exception throwing/catching has non-trivial overhead on some
  ABIs. `noexcept` lets the compiler generate tighter code.
- **Avoid virtual dispatch in inner loops** — if the vtable lookup cost is measured to matter,
  use templates/CRTP or devirtualize with `final`.

```cpp
// audio/realtime callback — everything here must be allocation-free
void ProcessBlock(double** inputs, double** outputs, int nFrames) noexcept {
    for (int i = 0; i < nFrames; ++i) {
        outputs[0][i] = mFilter.Process(inputs[0][i]);  // pre-allocated state
    }
    mSender.TransmitData(*this, mMeterData);  // lock-free queue to UI thread
}
```

## What Not to Do

| Avoid | Use instead |
| ------- | ------------- |
| `malloc` / `free` | `make_unique` / `make_shared` / containers |
| Raw `new` / `delete` | RAII types |
| `char*` for strings | `std::string` / `std::string_view` |
| C-style arrays `T arr[]` | `std::array<T,N>` / `std::vector<T>` / `std::span<T>` |
| `union` for variant data | `std::variant` |
| `void*` | templates or virtual dispatch |
| `(T)x` casts | `static_cast<T>(x)` |
| `volatile` for threading | `std::atomic<T>` |
| `unsigned` to avoid negatives | Signed with bounds checks |
| Output parameters `T&` | Return values / `std::optional` |
| Pointer arithmetic | `std::span` + iterators |
| Bare `catch (...)` swallow | Log + rethrow |
