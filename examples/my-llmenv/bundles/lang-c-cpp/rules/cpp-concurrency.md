---
paths:
  - "**/*.cpp"
  - "**/*.cc"
  - "**/*.cxx"
  - "**/*.hpp"
  - "**/*.hxx"
  - "**/*.h"
---

# C++ Concurrency (C++17/20)

For brief hot-path / real-time constraints, see `cpp.md`. This file covers the full threading API.

---

## Threading Primitives

### Prefer `std::jthread` (C++20) over `std::thread`

`std::jthread` auto-joins on destruction and carries a `std::stop_token` for cooperative
cancellation. Raw `std::thread` that goes out of scope without `join()`/`detach()` calls
`std::terminate()`.

```cpp
// bad — must manually join; terminate() if exception unwinds before join
std::thread t(worker);
t.join();

// good — joins automatically, accepts stop requests
std::jthread t([](std::stop_token st) {
    while (!st.stop_requested()) { do_work(); }
});
// t.request_stop(); t joins on destruction
```

**Never `detach()`** — you lose ownership and the thread may access destroyed objects.

### Prefer `std::async` over raw threads for one-shot work

`std::async` handles thread lifetime and propagates exceptions through the future.

```cpp
auto f = std::async(std::launch::async, compute, arg);
// ... do other work ...
auto result = f.get();  // blocks; rethrows any exception from compute()
```

**Fire-and-forget pitfall:** `std::async` with a discarded `future` blocks immediately — the
destructor of a `future` from `async` waits for completion.

```cpp
std::async(std::launch::async, fire_and_forget);  // BAD: blocks here
auto f = std::async(std::launch::async, work);    // GOOD: store the future
```

---

## Mutexes and Locking

### Always use RAII wrappers — never raw `lock()`/`unlock()`

| Wrapper | Use when |
|---------|----------|
| `std::lock_guard<M>` | Single mutex, no early unlock needed |
| `std::scoped_lock<M...>` (C++17) | Multiple mutexes at once (deadlock-safe) |
| `std::unique_lock<M>` | Need deferred lock, timed lock, or condition variable |
| `std::shared_lock<M>` | Read-only lock on `std::shared_mutex` |

### Deadlock prevention

Two threads locking the same mutexes in opposite order → deadlock. Fix with `std::scoped_lock`
which uses a deadlock-avoidance algorithm internally.

```cpp
// bad — lock order depends on scheduler
// thread A: lock(m1); lock(m2);
// thread B: lock(m2); lock(m1);  → deadlock

// good — scoped_lock acquires all atomically
std::scoped_lock lk(m1, m2);
```

Hierarchy rule as alternative: always lock lower-numbered mutexes first. Document the order.

### Reader-writer locks (`std::shared_mutex`, C++17)

When reads are frequent and writes rare, `shared_mutex` improves throughput: multiple readers
concurrently, one exclusive writer.

```cpp
std::shared_mutex mu;

void read() {
    std::shared_lock lk(mu);   // many readers allowed simultaneously
    use(data);
}

void write() {
    std::unique_lock lk(mu);   // exclusive — blocks all readers and writers
    data = new_value;
}
```

`std::shared_timed_mutex` (C++14) adds `try_lock_for`/`try_lock_until` variants.

**No upgrade lock in C++17** — you cannot promote a `shared_lock` to `unique_lock`. Unlock
first, then re-acquire exclusive. If upgrade is needed, use a separate mechanism or a
third-party library.

### Spinlocks — only for sub-microsecond critical sections

`std::atomic_flag` implements a correct spinlock. Use only when the critical section is
genuinely tiny and contention is known to be low. Spinning burns a full CPU core.

```cpp
class Spinlock {
    std::atomic_flag flag_ = ATOMIC_FLAG_INIT;
public:
    void lock()   { while (flag_.test_and_set(std::memory_order_acquire)) {} }
    void unlock() { flag_.clear(std::memory_order_release); }
};
```

For anything longer than a few instructions, use `std::mutex` (the OS can sleep the waiter).

---

## Condition Variables

**Always use with a predicate.** Without one, two bugs lurk:

- **Spurious wakeup**: the OS wakes the thread for no reason; the predicate re-evaluates and
  correctly goes back to waiting.
- **Lost wakeup**: the notifier fires before the waiter reaches `wait()`; a bare `wait()` blocks
  forever. The predicate survives this because it is checked before blocking.

```cpp
// bad — lost wakeup and spurious wakeup both cause hangs or wrong results
condVar.wait(lock);

// good — predicate is checked on every wakeup; wait re-sleeps if false
condVar.wait(lock, [] { return dataReady; });
// equivalent: while (!dataReady) condVar.wait(lock);
```

```cpp
// canonical producer/consumer
std::mutex mu;
std::condition_variable cv;
bool ready = false;

// producer
{
    std::lock_guard lk(mu);
    ready = true;
}
cv.notify_one();

// consumer
{
    std::unique_lock lk(mu);
    cv.wait(lk, [] { return ready; });
    consume();
}
```

---

## Tasks: `std::future`, `std::promise`, `std::packaged_task`

Three forms of the same abstraction — a typed data channel between threads:

| Form | Use when |
|------|----------|
| `std::async` | Simple fire-off-and-get, exception propagation |
| `std::packaged_task` | Need to control when/where work executes (thread pool) |
| `std::promise`/`std::future` | Manual signal: set value or exception from any context |

`future::get()` blocks and rethrows exceptions — call only from a thread that can afford to
block. `future::wait_for()` / `wait_until()` for polling.

```cpp
std::promise<int> p;
std::future<int>  f = p.get_future();

std::jthread producer([&p] { p.set_value(compute()); });
int result = f.get();  // blocks until producer calls set_value
```

---

## C++17: Parallel STL Algorithms

Most `<algorithm>` functions accept an execution policy as their first argument.

| Policy | Meaning |
|--------|---------|
| `std::execution::seq` | Sequential (same as no policy) |
| `std::execution::par` | Parallel across threads |
| `std::execution::par_unseq` | Parallel + SIMD vectorization |

```cpp
std::sort(std::execution::par_unseq, v.begin(), v.end());
std::transform(std::execution::par, in.begin(), in.end(), out.begin(), f);
```

**Rules for parallel-safe callbacks:**
- No exceptions in callbacks for `par_unseq` — `std::terminate()` is called.
- No data races — each element must be independent or protected.
- No locks inside callbacks with `par_unseq` — vectorized code cannot call `mutex::lock()`.
- For `par`, exceptions are collected and re-thrown as `std::exception_list` (implementation-
  defined behavior varies — check your toolchain).

---

## C++20: New Synchronization Primitives

### `std::latch` — one-time countdown barrier

A `latch` counts down to zero once and then stays open forever. Good for "wait for N workers
to finish initialization."

```cpp
std::latch ready(N_WORKERS);

for (auto& w : workers)
    std::jthread([&ready] { init(); ready.count_down(); });

ready.wait();  // blocks until all N workers called count_down()
```

### `std::barrier` — reusable phase synchronization

A `barrier` resets after every phase. Optional completion callback runs after each phase.
Use for iterative algorithms where all threads must finish step N before any starts step N+1.

```cpp
std::barrier sync(N_THREADS, [] { swap_buffers(); }); // callback between phases

auto worker = [&](int id) {
    for (int phase = 0; phase < PHASES; ++phase) {
        compute(id, phase);
        sync.arrive_and_wait();  // waits for all; callback runs; resets; continues
    }
};
```

`std::latch` for one-shot; `std::barrier` for repeated.

### `std::atomic<std::shared_ptr<T>>` (C++20)

`std::shared_ptr` reference-count updates are atomic, but the pointer itself is not. Two threads
reading/writing the same `shared_ptr` variable is a data race. Use `std::atomic<shared_ptr<T>>`.

```cpp
// bad: data race on the pointer value itself
std::shared_ptr<Data> g;
// thread A: g = make_shared<Data>(1);
// thread B: auto d = g;   ← UB

// good
std::atomic<std::shared_ptr<Data>> g;
g.store(make_shared<Data>(1));
auto d = g.load();
```

---

## Memory Model

The C++ memory model defines what values a thread can observe when reading a variable that
another thread wrote. Without synchronization, any read of a concurrently-written variable is
undefined behavior — even a plain `int`.

### Three ordering levels

| Level | `memory_order` | Cost | Guarantee |
|-------|---------------|------|-----------|
| Sequential consistency | `seq_cst` (default) | Highest | Global total order visible to all threads |
| Acquire-release | `acquire` / `release` / `acq_rel` | Medium | Synchronize one producer–consumer pair |
| Relaxed | `relaxed` | Lowest | Only atomicity; no ordering |

**Default to `seq_cst`** (`memory_order_seq_cst`). Only drop to weaker ordering after profiling
shows `seq_cst` is a bottleneck and you fully understand the resulting ordering constraints.

### Acquire-release pattern

A `release` store *happens-before* an `acquire` load of the same atomic variable that observes
the stored value. All writes before the release are visible after the acquire.

```cpp
std::atomic<bool> flag{false};
int data = 0;

// producer (thread A)
data = 42;
flag.store(true, std::memory_order_release);  // "releases" data=42

// consumer (thread B)
while (!flag.load(std::memory_order_acquire)) {}  // "acquires" the release
assert(data == 42);  // guaranteed to see 42
```

**Typical misunderstanding:** acquire-release only synchronizes threads that observe the *same*
atomic variable's value. A thread that loads a *stale* value (false) has not acquired the release
and may not see `data == 42`.

### Relaxed — counters and stats only

`memory_order_relaxed` guarantees only that atomic operations on the same variable from the same
thread are not reordered relative to each other. Use exclusively for independent counters where
the exact observation order does not matter.

```cpp
std::atomic<int> counter{0};
counter.fetch_add(1, std::memory_order_relaxed);  // stats counter — order irrelevant
```

### Avoid `memory_order_consume`

The standard defines it for data-dependency chains, but no mainstream compiler implements it
correctly — all promote it to `acquire`. Use `acquire` instead.

### Fences

`std::atomic_thread_fence` can be used to apply an ordering constraint to a group of operations
without attaching it to a specific atomic:

```cpp
// producer
data.store(42, std::memory_order_relaxed);
std::atomic_thread_fence(std::memory_order_release);
flag.store(true, std::memory_order_relaxed);

// consumer
while (!flag.load(std::memory_order_relaxed)) {}
std::atomic_thread_fence(std::memory_order_acquire);
assert(data.load(std::memory_order_relaxed) == 42);
```

Fences are harder to reason about than per-operation ordering. Prefer the per-operation form.

---

## Thread-Safe Initialization

### One-time initialization: `std::call_once`

```cpp
std::once_flag init_flag;
Config* g_config = nullptr;

void ensure_init() {
    std::call_once(init_flag, [] { g_config = new Config(load_file()); });
}
```

### Magic statics (C++11 guarantee)

Function-local `static` variables are initialized exactly once, thread-safely, on first call.
No manual synchronization needed.

```cpp
Config& get_config() {
    static Config cfg = load_config();  // thread-safe in C++11+
    return cfg;
}
```

### `thread_local` for per-thread state

`thread_local` gives each thread its own copy. Initialization happens on first use per thread.
Destructors run when the thread exits.

```cpp
thread_local std::mt19937 rng(std::random_device{}());  // each thread gets its own RNG
```

**Pitfall:** `thread_local` objects in detached threads may outlive the objects they reference.
Don't store references to non-`thread_local` objects with shorter lifetimes.

---

## Pitfalls Summary

| Mistake | Consequence | Fix |
|---------|-------------|-----|
| `volatile` for threading | No synchronization; data race | `std::atomic<T>` |
| Bare `condVar.wait(lock)` | Lost wakeup or spurious wakeup hang | Always pass predicate |
| Discard `std::async` future | Blocks on destruction | Store the future |
| `std::thread` without join | `std::terminate()` on destruction | `std::jthread` or join in dtor |
| `std::thread::detach()` | Lost ownership; dangling references | Avoid; use `jthread` |
| Locks in opposite order | Deadlock | `std::scoped_lock` for multiple mutexes |
| Raw `shared_ptr` across threads | Data race on the pointer | `std::atomic<shared_ptr<T>>` |
| `memory_order_consume` | Compiled as acquire anyway | Use `acquire` explicitly |
| Exceptions in `par_unseq` callback | `std::terminate()` | Use `par` or handle exceptions in callback |
| Spinlock on long critical section | 100% CPU on one core | `std::mutex` instead |
| `thread_local` referencing shorter-lived object | Dangling reference | Review lifetime carefully |
