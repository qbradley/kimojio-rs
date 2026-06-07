# Why not Rust async?

Rust async is a good idea with a still-expensive implementation. The problem is
not that nonblocking I/O is bad, or that every async program is slow. The
problem is that Rust's `async fn` asks the compiler to synthesize a nested,
lazy, poll-driven state machine, and today's compiler still emits too much of
that machinery too often. For performance-sensitive code, the result is not a
zero-cost abstraction. It is an abstraction whose costs are hard to see, hard to
measure locally, and often pushed back onto the programmer as source-level
"debloating" work.

This argument is based on the two Tweede golf posts, ["Debloat your async
Rust"](https://tweedegolf.nl/en/blog/235/debloat-your-async-rust) and ["Async
Rust never left the MVP state"](https://tweedegolf.nl/en/blog/237/async-rust-never-left-the-mvp-state),
plus linked Rust project goals, Rust Reference, standard library, Async Book,
zero-cost-futures, async-borrowing, LLVM coroutine, and historical green-thread
runtime material.

## The core issue: every async function is a future-shaped object

The Rust Reference states the essential semantic constraint: async functions do
no work when called; they capture their arguments into a future, and the body
executes only when that future is polled. That laziness is important for Rust's
executor model, but it means an ordinary-looking function call has become a
value construction plus a later state-machine resume.

The standard `Future` contract reinforces the shape of the generated code.
Futures are inert unless actively polled. If they return `Pending`, they must
arrange a later wakeup, and after wakeup the executor polls again. After
completion, clients should not poll again, but `poll` is safe, so a completed
future may panic, block forever, or otherwise misbehave as long as it does not
cause undefined behavior. That safe-but-not-reusable contract is one reason the
compiler emits defensive states and panic paths.

In other words, async Rust's suspension unit is not the call stack. It is a
compiler-created data structure that must encode the live locals, the current
suspend point, child futures, completion state, panic state, and wakeup behavior.
That is a lot of machinery for a feature whose surface syntax looks like normal
blocking code.

## "No await" still means "state machine"

The most damning example is:

```rust
async fn foo() -> i32 {
    5
}
```

Semantically, this does not return `5`; it returns a future that returns `5`
when polled. The Tweede golf analysis shows that rustc still generates the
default coroutine states for such a future: unresumed, returned, and panicked.
It switches on the discriminant and contains a post-completion panic path. A
manual future can instead be a tiny `Poll::Ready(5)` implementation, and the
standard library's `future::ready` exists precisely as the named future for an
immediately-ready value.

This matters because no-await async functions are not rare accidents. They
appear naturally through traits and abstraction boundaries: a trait method may
need to be async for database-backed or file-backed implementations, while a
default implementation just returns an already-known value. The abstraction
forces the cheap case through async machinery unless the author manually avoids
`async fn`.

That is not zero cost. It is a tax on uniform interfaces.

## Pass-through async functions multiply state machines

Rust programmers build abstractions by forwarding through layers: trait
implementations delegate to drivers, adapters translate signatures, middleware
wraps services, and helper functions call other helpers. In blocking Rust, a
function that just calls another function is the optimizer's easiest case. In
async Rust, this common pattern creates another future with another state
machine that exists only to poll the inner future.

The Tweede golf I2C example captures the problem:

```rust
async fn transaction(...) -> Result<(), Error> {
    self.transaction(...).await
}
```

This is source-level pass-through, but codegen-level duplication: a trait
future polls a driver future. The recommended optimization is to stop writing
`async fn` and return the inner `impl Future` directly. For preambles and
postambles, the advice becomes combinators such as `FutureExt::map`.

That workaround is a concession. It says that the high-level async syntax is
not the performance syntax. Worse, moving a preamble out of an async block can
change laziness: code that previously ran only when polled now runs when the
future is constructed. The programmer must know the lowering model and preserve
semantics by hand.

## Control flow duplicates suspend states

The compiler also fails to collapse obviously equivalent async control-flow
states. A match such as:

```rust
match get_command().await {
    CommandId::A => send_response(123).await,
    CommandId::B => send_response(456).await,
}
```

can generate separate suspend states for the two `send_response` calls, even
though they await the same kind of future. Rewriting it as:

```rust
let response = match get_command().await {
    CommandId::A => 123,
    CommandId::B => 456,
};
send_response(response).await;
```

removes a state. Again, the fix is not a better algorithm in application code;
it is manually shaping source to compensate for a naive coroutine transform.
This is exactly the kind of cost Rust users were told zero-cost abstractions
would hide.

## Panic paths poison optimization

Generated futures include a returned state whose post-completion poll panics,
and a panicked state used to prevent resuming a future after unwinding. Those
states are defensible under the safe `poll` contract, but they are not free.
They introduce side-effectful branches that are hard for LLVM to delete.

The async state-machine optimization project goal says the current transform
naively emits full state machines even when not required, and that every state
machine carries a panicking branch. It also notes that `opt-level=3` sometimes
cleans this up, but deep future trees defeat the optimizer, and size-optimized
builds (`s` or `z`) fare worse.

Tweede golf's compiler experiments found a 2-5 percent binary-size reduction in
embedded firmware by replacing the returned-state panic with `Poll::Pending` in
release-like builds, plus smaller gains from not generating a state machine for
no-await futures. That is a large result for removing behavior that should
never be hit by a correct executor.

## Future layout can explode memory usage

Async Rust stores values that live across await points inside the future. That
is unavoidable in principle, but current layout is still wasteful. The first
Tweede golf post gives a simple example where moving a `[u8; 1024]` buffer into
an async function produces a future around 2080 bytes, while passing a reference
produces one around 40 bytes. The compiler reserved space for the buffer more
than once.

The accepted Rust 2026 "Async Future Memory Optimisation" project goal is even
more blunt: exponential growth of async future types is a long-standing issue.
Its motivating example nests futures around a 64 KiB blob and reports a future
size of at least 1,000,000 bytes. The goal document says this has caused
unexpected stack overflows in cloud and network applications, and is a major
adoption concern for embedded systems and the Linux kernel.

The usual workaround is more indirection: box the future, move data to the heap,
or pass references carefully. That may restore reliability, but it gives up the
original performance story. Heap allocation and pointer chasing become the
escape hatch from an abstraction marketed as allocation-free composition.

## Poll/wake is a real runtime protocol

The Async Book's executor chapter is clear: top-level futures need an executor,
and executors repeatedly poll tasks when wakeups say progress is possible. The
waker chapter shows the per-future responsibility: clone and store the current
waker, then call `wake()` later so the executor polls again.

Good runtimes optimize this protocol aggressively, but the protocol remains
real. Futures must be quick to poll. Blocking work must be moved elsewhere.
Wakeups must identify tasks. Tasks must be queued. Wakers may be cloned and
updated because futures can move between tasks. In simple teaching executors
this involves boxes, `Arc`, `Mutex`, and channels; production runtimes replace
these with optimized machinery, but not with nothing.

This means async Rust performance is a three-party negotiation among source
shape, compiler lowering, and executor implementation. A local function body no
longer has a local cost model.

## The optimizer is asked to erase semantics too late

LLVM can sometimes clean up bad coroutine input, but the linked state-machine
project goal and Tweede golf post both argue that this is the wrong place to
rely on. Rust async lowering happens in MIR before LLVM. Each async block is
transformed individually, and useful facts about nested futures are not kept in
a form that enables broad future inlining.

The result is especially poor for the exact code Rust encourages:

```rust
async fn bar(x: X) -> Y {
    foo(x).await
}
```

Ideally, `bar` could become `foo`, or at least reuse `foo`'s state. Today it
gets its own future that polls `foo`'s future. Once that is lowered, LLVM sees a
larger program containing panics, discriminants, nested calls, and storage. It
may optimize simple cases at high optimization levels, but large async systems
quickly exceed the pattern-matching budget.

This is why the Rust project-goal work is framed as compiler work, not library
tuning. The problem is baked into the coroutine transform.

## Async performance depends on writing unnatural Rust

The first post's mitigation list is practical and revealing:

- Avoid async functions that do not need to be async.
- Return `impl Future` manually for pass-through functions.
- Use combinators to avoid extra async wrappers.
- Refactor branches to share await points.
- Pass references to large values instead of moving them into futures.

These are reasonable tactics, but together they indict the model. Good async
performance requires thinking in generated state machines, not in the
source-level Rust the feature was meant to recover. The programmer is not just
expressing concurrency; the programmer is hand-feeding the compiler a friendlier
MIR shape.

## The old green-thread lesson cuts both ways

Rust removed its old built-in green-thread runtime because it imposed global
costs, complicated embedding and FFI, coupled I/O and scheduling models, and
introduced overhead through runtime indirection. Async Rust avoided an imposed
runtime, which was the right systems-language instinct.

But that history should make us skeptical of hidden concurrency machinery, not
complacent. Async Rust moved the runtime decision out of `std`, but it also
moved a large amount of control-flow representation into compiler-generated
future types. The cost is no longer a universal runtime; it is an invisible
state machine at every async boundary, plus an executor protocol around it.

## The case against async Rust on performance grounds

Async Rust is defensible when ecosystem compatibility, enormous numbers of idle
network tasks, or integration with existing async libraries dominate the design.
But if the goal is predictable, low-overhead performance, async Rust has serious
obstacles:

1. It turns ordinary calls into lazy state-machine construction.
2. It generates state even for no-await or pass-through functions.
3. It duplicates states across ordinary control flow.
4. It carries panic and completion branches that defeat optimization.
5. It stores live data in future layouts that can grow dramatically.
6. It relies on poll/wake/executor machinery for progress.
7. It makes performance depend on compiler internals and optimization level.
8. It often forces manual source contortions or heap indirection to recover
   performance.

The most generous interpretation is that async Rust has not failed, but it is
not finished. The accepted compiler project goals acknowledge the same thing:
future layout needs packing, coroutine state needs inlining, no-op state
machines should be eliminated, panic paths should be reconsidered, and duplicate
states should be collapsed.

Until that work lands and proves itself in real systems, "async Rust is
zero-cost" should be treated as an aspiration. For performance-critical code,
especially code with fine-grained operations, tight memory budgets, large future
trees, or size-optimized builds, the safer default is skepticism.

## Sources followed

- Tweede golf, ["Debloat your async Rust"](https://tweedegolf.nl/en/blog/235/debloat-your-async-rust)
- Tweede golf, ["Async Rust never left the MVP state"](https://tweedegolf.nl/en/blog/237/async-rust-never-left-the-mvp-state)
- Rust Project Goals 2026, ["Async statemachine optimisation"](https://rust-lang.github.io/rust-project-goals/2026/async-statemachine-optimisation.html)
- Rust Project Goals 2026, ["Async Future Memory Optimisation"](https://rust-lang.github.io/rust-project-goals/2026/async-future-memory-optimisation.html)
- Rust Blog, ["Async-await on stable Rust"](https://blog.rust-lang.org/2019/11/07/Async-await-stable/)
- Aaron Turon, ["Zero-cost futures in Rust"](https://aturon.github.io/blog/2016/08/11/futures/)
- Aaron Turon, ["Borrowing in async code"](https://aturon.github.io/tech/2018/04/24/async-borrowing/)
- Rust Reference, ["Async functions"](https://doc.rust-lang.org/reference/items/functions.html#async-functions)
- Rust standard library, [`Future`](https://doc.rust-lang.org/std/future/trait.Future.html)
- Rust standard library, [`future::ready`](https://doc.rust-lang.org/std/future/fn.ready.html)
- Rust Async Book, ["Task Wakeups with Waker"](https://rust-lang.github.io/async-book/02_execution/03_wakeups.html)
- Rust Async Book, ["Applied: Build an Executor"](https://rust-lang.github.io/async-book/02_execution/04_executor.html)
- LLVM, ["Coroutines"](https://llvm.org/docs/Coroutines.html)
- Historical Rust RFC draft, ["Remove runtime"](https://github.com/aturon/rfcs/blob/remove-runtime/active/0000-remove-runtime.md)
