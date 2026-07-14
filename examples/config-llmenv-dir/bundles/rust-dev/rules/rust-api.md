---
paths:
  - "**/*.rs"
  - "**/Cargo.toml"
---

# Rust API Guidelines

Adapted from the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/). For
closed-source / internal crates, skip `C-METADATA`, `C-PERMISSIVE`, `C-STABLE`.

## Naming

- **`as_` / `to_` / `into_` cost model (C-CONV):**
  - `as_foo()` — borrowed→borrowed, free
  - `to_foo()` — borrowed→owned or expensive
  - `into_foo()` — owned→owned, consumes `self`
- **Getters drop `get_` (C-GETTER):** `cluster.name()`, not `get_name()`. `get_` is reserved for
  fallible accessors (`slice::get`).
- **Iterator methods: `iter()` / `iter_mut()` / `into_iter()` (C-ITER).** Iterator types named
  after the method (`Iter`, `IterMut`, `IntoIter`).
- **Consistent word order (C-WORD-ORDER):** pick a convention per concept and stick to it. If
  `DeploymentFactory` exists, don't add `FactoryForRedisCluster`.

## Interoperability

- **Eagerly derive common traits (C-COMMON-TRAITS):** `Debug`, `Clone`, `PartialEq`, `Eq`,
  `PartialOrd`, `Ord`, `Hash`, `Default`. The orphan rule blocks downstream from adding these —
  omission is permanent. `Default` only when zero/empty is meaningful.
- **Standard conversion traits (C-CONV-TRAITS):** `From` / `TryFrom` / `AsRef` / `AsMut` over
  custom `to_xxx()`.
- **Errors are `Send + Sync + 'static` + `std::error::Error` (C-GOOD-ERR).** `thiserror` gives this
  for free. Required for anything an async runtime touches.
- **`Send` + `Sync` where possible (C-SEND-SYNC).** On a multi-threaded tokio runtime, non-`Send`
  types in spec/state fail at `controller.run()`. Prefer `Arc<T>` over `Rc<T>`. `std::sync::Mutex`
  is fine and faster — use `tokio::sync::Mutex` only when holding a lock across `.await`.
- **`Serialize` / `Deserialize` on boundary types (C-SERDE):** config, wire, and persisted types.
  Skip for internal-only types.

## Predictability

- **Smart pointers — no inherent methods (C-SMART-PTR).** Use associated functions
  (`Foo::into_raw(x)`), not methods (`x.into_raw()`) — avoids shadowing pointee methods via `Deref`.
- **`new` for the primary constructor (C-CTOR).** `from_xxx` / `with_xxx` for alternatives.
  Newtype validators return `Result<Self, _>`.
- **No output parameters (C-NO-OUT).** Return tuples or structs, not `&mut T` for output. `&mut` is
  for in-place mutation only.
- **Operator overloads preserve algebra (C-OVERLOAD).** `impl Add` only for addition-like ops;
  otherwise prefer named methods.

## Flexibility

- **Most permissive parameter type (C-CALLER-CONTROL):**
  - `&str` over `&String`
  - `&[T]` over `&Vec<T>`
  - `&Path` over `&PathBuf`
  - `impl AsRef<str>` / `impl Into<String>` when flexibility is needed
- **Expose intermediate results (C-INTERMEDIATE).** If callers commonly want a value computed on
  the way to the return, return it (tuple/struct) — don't force a recompute.
- **Generics over concrete when only behavior matters (C-GENERIC).**
  `fn foo<I: IntoIterator<Item = T>>(xs: I)` beats `fn foo(xs: &[T])` for iteration. Trade-off:
  monomorphization bloat, signature verbosity.
- **`dyn Trait` for heterogeneous collections (C-OBJECT).** `Vec<Box<dyn Mutator>>` = trait-object
  case. Homogeneous = generics.

## Type Safety

- **Deliberate types over `bool` / `Option<bool>` (C-CUSTOM-TYPE).** `(tls: TlsMode, cache:
  CacheMode)` beats `(enable_tls: bool, use_redis: bool)`.
- **Newtypes for static distinctions (C-NEWTYPE).** See [`rust.md`](rust.md) §Newtypes.
- **Builder pattern for complex construction (C-BUILDER).** Finalize with `build()`. Required
  fields are constructor args, not skippable setters.
- **`bitflags` crate for bitflags (C-BITFLAG).** Don't hand-roll `u32` flag constants.

## Dependability

- **Validate at construction, not every use (C-VALIDATE / parse-don't-validate).** Push validation
  into constructors. `debug_assert!` for invariants worth checking in debug builds.
- **Destructors never fail (C-DTOR-FAIL).** A panicking `Drop` during unwinding aborts the process.
  For fallible teardown, expose `close()` / `shutdown()` returning `Result`; `Drop` does best-effort
  and logs (`tracing::warn!`) on error.

## Debuggability

- **All public types implement `Debug` (C-DEBUG).**
- **`Debug` output is never empty (C-DEBUG-NONEMPTY).** An empty `Debug` is worse than none — it
  implies "no info" when it usually means "didn't bother". Custom impls emit at least the type name.

## Future Proofing

- **Sealed traits for internal extension points (C-SEALED).** Seal traits you control so adding
  methods later isn't a breaking change:

  ```rust
  mod private { pub trait Sealed {} }
  pub trait Factory<T>: private::Sealed { /* ... */ }
  ```

  Documents intent and prevents accidental impls in sibling crates.
- **Private struct fields by default (C-STRUCT-PRIVATE).** Public fields freeze layout and prevent
  adding invariants. Use accessors or constructors.
- **No trait bounds on struct definitions (C-STRUCT-BOUNDS).** Bound the `impl` blocks instead:

  ```rust
  // Bad
  struct Foo<T: Clone> { data: T }

  // Good
  struct Foo<T> { data: T }
  impl<T: Clone> Foo<T> { fn duplicate(&self) -> Self { /* ... */ } }
  ```

## Documentation

For non-trivial public APIs:

- **Document error and panic conditions (C-FAILURE).**
- **Examples use `?`, not `unwrap` (C-QUESTION-MARK).** Required by the workspace lints.
- **Hide impl noise (C-HIDDEN).** `#[doc(hidden)]` on `pub` items that exist only for macro or
  trait-impl needs.
