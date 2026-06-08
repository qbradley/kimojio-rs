# An apology for kimojio-stack

`kimojio-stack` is an apology in the old sense: a defense. It defends the
apparently unfashionable idea that a stack is still a good representation for
I/O-heavy control flow, provided the runtime is explicit, scoped, cooperative,
and honest about its tradeoffs.

The critique in [WHY_NOT_RUST_ASYNC.md](WHY_NOT_RUST_ASYNC.md) is not that
nonblocking I/O is bad. It is that Rust async pays for nonblocking I/O by
turning source-level control flow into generated future state machines whose
size, branches, panic paths, layout, and wakeup behavior are difficult to reason
about locally. `kimojio-stack` takes the opposite bet: keep ordinary stack
frames as the unit of suspension, use io_uring for nonblocking kernel work, and
park only the current coroutine when work cannot proceed.

## The value proposition

`kimojio-stack` offers:

1. Blocking-style Rust APIs that do not block the OS thread.
2. Scoped stackful coroutines that can borrow from their parent stack.
3. A single-threaded cooperative scheduler with predictable task placement.
4. io_uring integration, including fixed-file and fixed-buffer support.
5. Heterogeneous waits over I/O, task joins, channels, locks, and other
   readiness conditions.
6. Cooperative synchronization primitives that avoid OS blocking.
7. A hot-path allocation model designed around reuse and inline storage.

That is a coherent alternative to async Rust for systems where the primary
performance goal is not "fit into the async ecosystem" but "make latency,
memory, scheduling, and I/O costs explicit."

## Stackful coroutines recover the ordinary call-chain cost model

The crate-level documentation is direct: `kimojio-stack` is intentionally not an
async runtime. `Runtime` owns a single-threaded cooperative scheduler, and
`RuntimeContext` is the capability object for spawning, yielding, io_uring
operations, and waiting.

A stackful coroutine keeps its locals on its stack. If it calls `read`, then
`parse`, then `write`, those calls are still calls. The compiler does not need
to manufacture a distinct future type for each layer, and the programmer does
not need to rewrite pass-through functions as `impl Future` to avoid wrapper
state machines. Suspension happens by saving the coroutine stack, not by
encoding every live local and await point into a generated enum-like future.

This is the heart of the stackful case: pay for a guarded stack per coroutine,
then stop paying a state-machine tax at every abstraction boundary.

## Scoped spawning makes stack borrowing safe

`RuntimeContext::scope` and `Scope::spawn` mirror `std::thread::scope`.
Spawned coroutines may borrow from the caller, but the scope waits for every
child before returning. `JoinHandle::join` retrieves the return value directly;
panics that escape a child closure propagate to the parent when joined or when
the scope finishes.

That gives stackful concurrency a crucial Rust shape: it is not "fire off a
green thread and hope the borrowed data survives." The lifetime of a coroutine
is bounded by the lexical scope that created it. This is what makes ordinary
borrowed APIs possible inside concurrent code.

## Blocking-looking I/O parks only the coroutine

The synchronous-looking I/O methods are explicit about their behavior:
`RuntimeContext::read`, `write`, `send`, `recv`, `accept`, `connect`, `open`,
`fsync`, `close`, `sleep`, and the rest submit one SQE, park only the current
stackful coroutine, and return after the CQE is reaped.

That means code can use borrowed buffers naturally:

```rust
let mut buf = [0_u8; 4096];
let n = cx.read(&fd, &mut buf)?;
cx.write(&fd, &buf[..n])?;
```

In async Rust, a buffer that lives across an await point becomes part of a
future's layout, and moving large values into futures can explode memory use.
In `kimojio-stack`, the blocking-style API uses the coroutine stack and parks
the coroutine while the kernel owns the operation. The programmer gets the
source shape of blocking code with the scheduling behavior of nonblocking I/O.

When true detached I/O is needed, the crate also provides `read_async` and
`write_async` returning `IoResult`. Those operations own their buffers until
completion so safe code cannot free memory while io_uring still has a pointer.
Successful completions return the original buffer in `ReadOutput` or
`WriteOutput`; pending dropped handles deliberately leak their backing buffer
rather than risk kernel writes into freed memory. That is a blunt but sound
tradeoff: safety first, explicit completion when reclamation matters.

## The scheduler is flat and predictable

The runtime deliberately avoids recursive scheduler driving. A coroutine that
cannot proceed records a `Waiter` and suspends with `yielder.suspend`. Only the
root context drives the scheduler loop. Waking a waiter requeues the suspended
task by ID, and the scheduler resumes it later on the same coroutine stack.

The scheduler data structures are simple and visible: a table of tasks, a ready
queue of task IDs, an io_uring instance, fixed-resource free lists, an I/O state
pool, completion scratch storage, and an in-flight I/O count. The ring-enter
policy controls when ready tasks give way to io_uring submission and completion.
If tasks are ready, ring entry is nonblocking. If no tasks are ready but I/O is
in flight, the scheduler can wait in the ring.

This design has an important performance property: runnable coroutines do not
migrate between threads, and they do not require cross-thread wakeup
synchronization. The cost model is closer to "a local queue and a coroutine
resume" than "a future, a waker, an executor task, and a poll tree."

## Heterogeneous wait without erasing results

`Waitable` is deliberately readiness-only. I/O results, join handles, mutexes,
semaphores, channels, watch receivers, and other primitives can be waited on by
reference with `wait_any`, `wait_all`, `select`, or `join`. Once a condition is
ready, the caller uses the typed API to consume the result: `IoResult::try_get`,
`JoinHandle::try_join`, `Mutex::try_lock`, channel `try_recv`, semaphore
`try_acquire`, and so on.

This is a useful middle ground. The runtime can wait on heterogeneous
conditions, but it does not erase ownership or result types into a universal
boxed output. A timed-out wait does not consume the pending operation. The typed
handle remains responsible for recovering its own value.

## Cooperative sync primitives match the runtime

The synchronization modules are stackful versions of familiar tools:
`Mutex`, `Semaphore`, `Notify`, `watch`, `Barrier`, `RwLock`, bounded and
unbounded channels, and a one-shot channel. They do not block the OS thread.
When a primitive cannot make progress, it records a waiter and calls `cx.park`.
When state changes, it wakes one or more waiters by requeueing coroutine task
IDs.

Because the runtime is single-threaded, these primitives can use `Rc`,
`RefCell`, simple counters, and local wait queues instead of atomics and OS
mutexes on their core path. That is not a universal concurrency model, but it
is a strong one for thread-per-core I/O: partition work explicitly, then keep
the hot path local.

## The allocation story is concrete

`kimojio-stack` does not pretend setup is free. Runtime containers, io_uring,
coroutine stacks, join state, and channel state allocate. The important claim is
narrower and more valuable: warmed hot I/O paths are designed to avoid Rust heap
allocation.

The implementation backs that claim with specific mechanisms:

- I/O operation state is recycled through an internal pool.
- Completion scratch storage is reused across ring entries.
- The common single waiter is stored inline before falling back to a vector.
- Test-only allocation tracking asserts zero Rust allocations on warmed
  blocking and async pipe ping-pong paths.
- The benchmark suite covers scheduler yield, spawn/join, sync primitives,
  channels, io_uring dispatch, pipe round trips, fixed files, fixed buffers,
  and two-task ping-pong.

This is the kind of performance work async Rust often obscures. The stackful
runtime makes the allocation boundaries visible: allocate the coroutine stack
and runtime structures up front, then keep the steady-state path boring.

## Fixed resources turn io_uring into a first-class performance feature

The runtime supports registered file and buffer tables through
`Runtime::with_registered_resources`, `register_fd`, and `register_buffer`.
Fixed-resource I/O records resource leases in the operation state and releases
them at CQE completion. Dropping a registered fd or buffer while I/O is pending
retires the slot only after the kernel is finished with it.

That matters because io_uring performance is not just "submit async I/O." The
fast path often depends on reducing per-operation kernel bookkeeping. A stackful
runtime that owns the ring, operation state, and fixed-resource lifetimes can
make those optimizations part of the normal API instead of an afterthought.

## It is not a free lunch, and that is a strength

`kimojio-stack` pays costs async Rust tries to avoid:

- Each coroutine has a stack, defaulting to a small guarded stack.
- The runtime is currently single-threaded.
- It is Linux/io_uring oriented.
- It does not integrate with Rust's `Future` ecosystem.
- Work distribution across cores is explicit rather than automatic.

Those constraints are real. They are also legible. A stack is a simple resource
to budget. A single-threaded scheduler has a simple synchronization story.
Linux/io_uring is a clear target. Not integrating with `Future` avoids importing
the future state-machine and poll/wake cost model by accident.

The design does not claim to be the universal Rust concurrency answer. It
claims to be the right answer for a particular class of programs: I/O-bound,
latency-sensitive, Linux systems code that benefits from thread-per-core
partitioning, borrowed blocking-style APIs, explicit scheduling, and tight
control over allocation.

## The defense

Async Rust asks the compiler to recover efficient machine code from a forest of
lazy state machines. Sometimes it succeeds. Sometimes it leaves behind large
future layouts, duplicated states, panic branches, pass-through wrappers, and
manual debloating advice.

`kimojio-stack` avoids that gamble. It represents suspended work as suspended
stacks. It represents readiness as waiters. It represents I/O as io_uring
operations. It represents structured concurrency as scopes. It represents
performance as concrete data structures and tests, not as a promise that LLVM
will erase the abstraction later.

That is the apology: stackful coroutines are not nostalgia. In this design,
they are a performance tool. They trade implicit compiler-generated machinery
for explicit runtime machinery that can be inspected, bounded, benchmarked, and
reasoned about.
