# Why stacks?

`WHY_NOT_RUST_ASYNC.md` argues that Rust async is not automatically zero-cost:
it replaces the call stack with compiler-generated future state, and current
codegen can make those states large, duplicated, and hard to reason about.

This file asks the opposite question: if stackless async has those costs, why is
it reasonable to use stackful coroutines in `kimojio-stack` and
`kimojio-stack-steal`, given that Rust removed its original green-thread
runtime?

The answer is not "Rust was wrong." The answer is narrower:

1. Rust was right not to put a universal green-thread runtime under `std`.
2. Rust was right that segmented stacks are a bad fit for predictable systems
   performance.
3. Rust cannot use Go-style stack copying without giving up important Rust
   properties.
4. A library can still deliberately choose fixed, guarded, tunable stacks when
   it also accepts the API and density constraints that follow.

`kimojio-stack` is that deliberate choice. It is not trying to make all Rust code
magically green-threaded. It provides a capability object, scoped spawning,
runtime-owned I/O, and explicit scheduling rules. Code that adopts it chooses
the model instead of accidentally inheriting it.

## What Rust removed

Rust's pre-1.0 green-thread runtime was removed by RFC 230. The key problem was
not merely "green threads are bad." The problem was the shape of the abstraction:
`std` tried to support both native threads and green threads behind one standard
I/O API.

RFC 230 calls out several concrete failure modes:

- `std::io` had to work for both native and green tasks, forcing the models to
  co-evolve even when they wanted different APIs.
- The runtime abstraction used trait objects for the scheduler and I/O entry
  points, adding dynamic dispatch and sometimes allocation.
- Task-local storage was slower because it had to work across both models.
- Some native or FFI blocking calls could still block the green scheduler's OS
  worker thread.
- Embedding Rust into other runtimes became harder because `libstd` required
  runtime setup.
- `libstd` paid complexity and maintenance cost for a model many programs did
  not want.

The long-term plan in the RFC was explicit: bind `std` directly to native
threads and native I/O, then let green-thread packages live outside `std` with
their own I/O APIs. That is almost exactly the lesson `kimojio-stack` follows.
It does not modify `std::io`, does not hide behind the standard library, and
does not promise that arbitrary blocking Rust code becomes safe to run on a
green task.

The RFC also documents the stack problem. Rust initially used segmented stacks,
then removed them in favor of preallocated stacks because segmented stacks had
significant performance and complexity costs. Once stacks are preallocated, green
threads are no longer "free" or Go-like in density.

## The stack problem

Stackful coroutines need stack memory. There are only a few known strategies,
and each has a cost.

### Segmented stacks

Segmented stacks start small and allocate new stack segments when a call would
overflow the current segment. They solve worst-case reservation, but they make a
function call potentially allocate. The bad case is the "hot split": a loop calls
a function near a segment boundary, allocates a segment, returns, frees it, and
does the same thing on the next iteration.

Both Rust and Go tried segmented stacks and moved away from them. The Cloudflare
writeup on Go stacks describes the same "hot split problem"; the Stanford green
threads paper describes the Rust/Go shared "stack thrashing" issue.

For `kimojio-stack`, segmented stacks would violate one of the main reasons to
avoid async Rust: they hide unpredictable allocation at ordinary call sites.
They are not the right answer.

### Stack copying

Go's answer is stack copying. A goroutine starts with a small stack. When it
needs more, the runtime allocates a larger contiguous stack, copies the old
stack, and updates pointers into the stack. This avoids the hot-split cost
because later shrink/grow cycles can reuse the larger stack space.

This works in Go because the language and runtime cooperate:

- Go has precise pointer metadata for garbage collection.
- Go can identify pointers inside a goroutine stack.
- Go's pointer rules mean stack pointers that must be rewritten are themselves
  discoverable by the runtime.
- Go accepts a runtime and garbage collector as part of the language contract.

That is not Rust's contract. Rust permits references and raw pointers into a
stack to appear in places a library cannot enumerate: heap objects, other
stacks, FFI, unsafe code, and optimized compiler temporaries. A library cannot
move a Rust stack and update all outstanding references. Doing so would require
compiler/runtime cooperation comparable to GC stack maps plus additional rules
about where stack references may live.

So Go-style stack copying is not available to `kimojio-stack` as a normal Rust
library. We should not pretend otherwise.

### Large virtual stacks

Another approach is to reserve a large virtual stack for every task and rely on
virtual memory to commit physical pages lazily. This is simple but has its own
risks:

- On 32-bit systems it runs out of virtual address space quickly.
- On 64-bit systems it relies on overcommit and creates large virtual
  reservations.
- Page faults become part of the latency profile.
- Many stacks mean many mappings and guard pages.
- A high default size moves the design back toward OS-thread economics.

`kimojio-stack` avoids the worst version of this by defaulting to small fixed
stacks, not multi-megabyte thread stacks. But fixed stacks are still a real
budget, not magic.

### Compiler-generated stackless state

Stackless coroutines avoid per-task stacks by moving live state into explicit
state machines. That is Rust async. It can be extremely memory-efficient in the
best case, but `WHY_NOT_RUST_ASYNC.md` explains the current costs: duplicated
states, large future layouts, poll/wake protocol overhead, and source-level
contortions to control generated code.

This is the tradeoff. If we reject segmented stacks and cannot copy stacks, then
we choose between:

- fixed guarded stacks with explicit density limits, or
- stackless compiler/language state machines with async's costs.

`kimojio-stack` chooses fixed guarded stacks.

## Why Go succeeded

Go succeeds with goroutines because the whole language and runtime are designed
around them.

Official Go documentation describes goroutines as lightweight functions running
concurrently in the same address space. Their stacks start small and grow as
needed. Goroutines are multiplexed onto multiple OS threads, so if one blocks
waiting for I/O, others continue to run.

The important ingredients are:

- A single blessed concurrency model for most server code.
- M:N scheduling integrated into the runtime.
- Network I/O integrated with the scheduler.
- Channels integrated into the language and runtime.
- Growable/copying stacks supported by GC metadata and pointer rules.
- Runtime preemption. Since Go 1.14, goroutines are asynchronously preemptible,
  so loops without function calls no longer indefinitely delay the scheduler or
  garbage collector.
- A culture that accepts the runtime, garbage collector, and FFI tradeoffs.

Go did not make green threads successful as an add-on library. It made them part
of the platform.

This matters for `kimojio-stack`: we can copy the ergonomic lesson, but not the
implementation trick that gives Go very high goroutine density. We do not have a
moving stack runtime. We do not have compiler-managed stack maps for arbitrary
Rust frames. We should not claim goroutine-equivalent density.

## Why modern Java virtual threads succeeded where old Java green threads did not

Old Java green threads were effectively M:1 in common deployments: many Java
threads shared one OS thread. That meant no multicore parallelism, and a blocking
system call could block the whole process. As native OS threads matured, old Java
green threads were abandoned in favor of platform threads.

Java 21 virtual threads are different. JEP 444 makes several key design choices:

- Virtual threads are not a replacement for platform threads; both coexist.
- The goal is thread-per-request server code with better scalability, not faster
  CPU execution.
- Virtual threads are mapped M:N onto carrier OS threads.
- Blocking Java APIs can unmount the virtual thread and release the carrier
  thread, or compensate when unmounting is impossible.
- The JVM owns enough of the runtime, stack representation, blocking APIs, and
  tooling to make the abstraction observable and debuggable.
- Users are told not to pool virtual threads. They should represent tasks, while
  semaphores and other constructs limit external resources.

Java's success is therefore not evidence that any Rust library can transparently
virtualize all blocking code. It is evidence that a stackful model can work when
the platform owns the blocking APIs, scheduler, stack representation, and
tooling story.

`kimojio-stack` owns only the code that uses its `RuntimeContext`. That is enough
for a useful library, but it is not enough for transparent virtual threads.

## Why library-level green threads often fail

The small green-thread implementations in C and Rust are valuable because they
show the core mechanism: allocate a stack, save callee-saved registers, switch
the stack pointer, and jump through a trampoline. The C article and the
`book-green-threads-explained` Rust implementation both make that approachable.

They also show the boundary of the toy model:

- Safe stack creation requires unsafe platform-specific code.
- Closure bootstrapping in Rust is much harder than function-pointer
  bootstrapping in C.
- Borrowing rules are hard for a scheduler to express because only one green
  thread runs at a time, but the compiler cannot generally see that.
- General I/O integration is the real problem. The Stanford paper explicitly
  notes that async I/O libraries were coupled to other event-loop paradigms and
  not easy to integrate.
- Blocking syscalls, FFI, page faults, and CPU loops still block the carrier OS
  thread unless the runtime controls those operations.

A green-thread library is easy to demo and hard to make into a coherent
ecosystem.

## The constraints `kimojio-stack` accepts

`kimojio-stack` and `kimojio-stack-steal` should be judged against the constraints
they actually accept, not against the fantasy of transparent goroutines for all
Rust code.

### 1. No universal API

Rust's old runtime tried to make one I/O API work for native and green tasks.
`kimojio-stack` does not. Code receives a `RuntimeContext` and uses the methods
available through that context. If code calls arbitrary blocking `std`, `libc`,
or FFI APIs, it may block the carrier thread.

This is a cost, but it is also the main reason the model is viable. Adopters can
decide whether the restricted API is worth the simpler stackful control flow.

### 2. Fixed guarded stacks

The current stack model is fixed, guarded, and tunable:

- `kimojio-stack` defaults to a 64 KiB usable stack.
- `kimojio-stack-steal` defaults to the same per coroutine.
- Stack sizes are rounded to page boundaries and must satisfy corosensei's
  minimum.
- `corosensei::DefaultStack` supplies guard-page protection so overflow traps
  instead of corrupting adjacent memory.
- `Scope::spawn_with_stack_size` allows per-coroutine overrides.
- `RuntimeContext::stack_usage` and `JoinHandle::join_with_stack_usage` report
  stack usage so applications can tune.
- Completed stacks are cached in bounded pools to avoid paying stack allocation
  and guard setup on hot spawn paths.

This is the Rust-compatible answer to stack movement: do not move stacks. Keep
Rust references stable. Use guard pages for safety. Make stack size a real knob.

### 3. Cooperative scheduling

`kimojio-stack` is cooperative. A task makes progress until it yields, parks on a
runtime-aware operation, or returns. `kimojio-stack-steal` maps this model over
worker threads, but a running coroutine still owns its worker until it yields or
parks.

This means:

- runtime I/O can park without blocking the worker;
- channels, joins, scopes, and runtime waits can park safely;
- long CPU loops need explicit yield points;
- blocking `std`/FFI calls can pin a worker;
- page faults can pin a worker;
- there is no Go 1.14-style asynchronous preemption.

That is acceptable only if it is documented and validated.

### 4. Structured concurrency

`RuntimeContext::scope` and `Scope::spawn` are not just ergonomic. They bound the
lifetime of borrowed stack data. This is important because fixed stacks preserve
normal Rust borrowing: spawned closures may borrow from the parent, and the
scope waits for all children before returning.

This is a strength over unstructured green-thread APIs. It also means stackful
tasks are not detached by default. Detached or cross-runtime work must be
designed separately.

### 5. Density is configurable, not automatic

The default 64 KiB stack is much smaller than the multi-megabyte default stack of
many OS threads, so `kimojio-stack` can be denser than thread-per-request in many
server workloads.

But it is not Go:

- 10,000 live tasks at 64 KiB usable stack reserve roughly 640 MiB of usable
  stack address space, plus guard pages and mapping overhead.
- 100,000 live tasks reserve roughly 6.4 GiB usable stack address space, plus
  guard pages and mapping overhead.
- Physical RSS depends on touched pages and stack high-water behavior.
- Cached stacks retain mappings after bursts.
- In `kimojio-stack-steal`, `max_cached_stacks_per_worker` is per worker. The
  default `1024` cached stacks per worker at 64 KiB is a large retention budget
  when worker counts are high.

The right density claim is therefore:

`kimojio-stack` can support high task density when stacks are shallow, stack sizes
are tuned, and cache bounds are chosen for the deployment. It should not claim
goroutine-like million-task density until measurements prove it for a specific
configuration and workload.

## What we need to validate

The stack model is reasonable, but it needs explicit validation. The most
important missing validation is a density and stack-budget matrix.

### Stack density benchmark

Add a benchmark or perf harness that runs both `kimojio-stack` and
`kimojio-stack-steal` with stack sizes such as 8 KiB, 16 KiB, 32 KiB, 64 KiB, and
128 KiB, and task counts such as 1k, 10k, 50k, and 100k where the machine can
support them.

For each run, record:

- spawn time;
- join/quiescence time;
- scheduler latency while many tasks are parked;
- steady-state RSS;
- virtual memory size;
- page-fault counts if available;
- stack high-water distributions from `StackUsage`;
- retained cached-stack count and capacity after burst;
- failure mode when the configured stack is too small.

Run the same matrix with `kimojio-stack-steal` worker counts of 1, 2, 4, 8, 16,
32, 64, and 128 where feasible. This is especially important because stack cache
retention is per worker.

### Stack cache retention validation

The recent allocator work added pool diagnostics. Use those diagnostics to make
cached stack retention visible in density tests.

Questions to answer:

- Is `max_cached_stacks_per_worker = 1024` too high for high worker counts?
- Should `kimojio-stack-steal` add a global stack-cache budget across workers?
- Should examples for high concurrency lower `max_cached_stacks_per_worker`?
- Should stack cache defaults depend on worker count?

Do not change defaults blindly. Measure burst workloads first.

### Stack-size guidance

Create a public guide section that explains:

- default stack size;
- guard-page behavior;
- how to use `spawn_with_stack_size`;
- how to use `stack_usage` and `join_with_stack_usage`;
- how to interpret high-water results;
- expected memory budget per live task and per cached task;
- why stack overflow traps are safety, not recovery.

This guide should be linked from both stack and stack-steal docs.

### Blocking escape hatch

Decide whether to provide a sanctioned `spawn_blocking` or `block_in_place` style
escape hatch for operations that cannot be expressed through runtime I/O.

If added, it should be explicit and bounded. It must not recreate Rust's old
problem by making arbitrary blocking look free. Possible design:

- a separate blocking thread pool;
- bounded queue;
- cancellation/deadline semantics documented as best-effort;
- metrics for queued/running blocking jobs;
- clear guidance that this is for rare integration points, not normal I/O.

### Fairness and preemption validation

Because scheduling is cooperative, add fairness tests or benchmarks showing the
effect of:

- long CPU loops without yields;
- loops with periodic `yield_now`;
- I/O-heavy tasks mixed with CPU-heavy tasks;
- stack-steal worker saturation when one worker is pinned.

The expected result should be documented: no preemption is provided, so users
must yield or offload CPU/blocking work.

### FFI constraints

Document that FFI called from a stackful task is just native code running on the
carrier thread. If it blocks, page faults heavily, or stores pointers to the
coroutine stack beyond the call, the runtime cannot make that safe or cheap.

For FFI-heavy users, the recommended pattern should be:

- call nonblocking APIs and integrate readiness with the runtime, or
- offload to a bounded blocking pool, or
- use OS threads directly for that subsystem.

## Design work to consider

### Custom stack allocator investigation

`corosensei::DefaultStack` gives the most important property: guarded stacks.
Still, for density work we should investigate whether a custom stack provider is
worth it.

Questions:

- Can we reduce VMA overhead for many stacks?
- Can we reserve stacks with predictable `mmap`/guard behavior?
- Can we expose stack mapping and residency diagnostics more directly?
- Can we keep guard-page safety while improving cache locality?
- Can we integrate stack pools with NUMA/worker locality?

This should stay fixed-stack. It should not become segmented stacks or stack
copying unless Rust gains compiler/runtime support.

### Stack classes

Many applications have a few stack-depth classes: tiny request dispatch,
moderate protocol parsing, and deep compression/TLS/codec paths. Consider a
small "stack class" API or helper pattern so users can pick known stack sizes
without scattering raw byte counts.

This can probably be documentation first:

- tiny: measured shallow handlers;
- default: general request work;
- large: recursive or library-heavy paths.

### Budget-aware examples

Canonical examples should not just set a fast worker count. They should show the
memory budget:

```text
workers = 64
stack_size = 64 KiB
max_cached_stacks_per_worker = 128
maximum cached stack reservation ~= 64 * 128 * 64 KiB = 512 MiB
```

That kind of arithmetic should be visible in example docs and benchmark output.

## Constraints we should accept

Some constraints are not bugs.

- We should not support arbitrary `std::io` transparently inside stackful tasks.
- We should not hide a global runtime under normal Rust APIs.
- We should not attempt segmented stacks.
- We should not attempt moving/copying stacks in a library.
- We should not promise preemption.
- We should not promise goroutine-equivalent task density.
- We should not make FFI blocking look safe.

These constraints are the price of preserving Rust's stable references, small
runtime surface, explicit adoption, and predictable performance model.

## The positive case

The positive case for stacks is still strong:

- Source code keeps normal sequential control flow.
- Borrowing works naturally inside structured scopes.
- Blocking-looking runtime I/O can park the coroutine without allocating async
  future trees.
- The suspension state is the stack, not a compiler-generated nested future.
- Stack overflow is guarded.
- Stack size is tunable.
- Stack usage is measurable.
- Stack allocation cost is amortized by bounded stack pools.
- `kimojio-stack-steal` can run the model across OS workers without imposing it
  on all Rust code.

This is a different point in the design space than Rust async. It is not
strictly better. It is more constrained and more explicit. For workloads with
many mostly-shallow, I/O-heavy tasks that can stay inside the runtime API, it may
be a better performance and clarity tradeoff.

## Bottom line

`kimojio-stack` resolves the old green-thread objections by not repeating the
old Rust design:

- no universal `std` integration;
- no hidden global runtime;
- no segmented stacks;
- no stack copying that Rust cannot support;
- no claim that arbitrary blocking code is safe;
- no claim that stacks are free.

It accepts fixed, guarded, tunable stacks and an explicit runtime API. That is a
valid Rust-compatible design, but it must be paired with density benchmarks,
cache-budget diagnostics, blocking/FFI guidance, and documentation that clearly
states the constraint: high density is a measured configuration property, not an
automatic consequence of being stackful.

## Sources followed

- Rust RFC 230, ["Remove runtime"](https://rust-lang.github.io/rfcs/0230-remove-runtime.html)
- Rust RFC 230 raw text, [`0230-remove-runtime.md`](https://github.com/rust-lang/rfcs/blob/master/text/0230-remove-runtime.md)
- without.boats, ["Why async Rust?"](https://without.boats/blog/why-async-rust/)
- Adam Szpilewicz, ["Rust's early green threads pre-1.0"](https://medium.com/@adamszpilewicz/rusts-early-green-threads-pre-1-0-e0bfee127121) - inaccessible during this research pass due HTTP 403; core historical claims were cross-checked against RFC 230, without.boats, and the Stanford paper instead.
- Kevin Rosendahl, ["Green Threads in Rust"](https://stanford-cs242.github.io/f17/assets/projects/2017/kedero.pdf)
- Go Tour, ["Goroutines"](https://go.dev/tour/concurrency/1)
- Effective Go, ["Goroutines"](https://go.dev/doc/effective_go#goroutines)
- Jay Conrod, ["Goroutines: the concurrency model we wanted all along"](https://jayconrod.com/posts/128/goroutines-the-concurrency-model-we-wanted-all-along)
- Coding Time, ["Goroutines implementation"](https://coding-time.co/goroutines-implementation/)
- Cloudflare, ["How Stacks are Handled in Go"](https://blog.cloudflare.com/how-stacks-are-handled-in-go/)
- Go 1.14 release notes, [runtime preemption section](https://go.dev/doc/go1.14#runtime)
- Wikipedia, ["Green thread"](https://en.wikipedia.org/wiki/Green_thread)
- Wikipedia, ["Fiber (computer science)"](https://en.wikipedia.org/wiki/Fiber_(computer_science))
- OpenJDK JEP 444, ["Virtual Threads"](https://openjdk.org/jeps/444)
- Oracle Java 21 docs, ["Virtual Threads"](https://docs.oracle.com/en/java/javase/21/core/virtual-threads.html)
- k0nserv/cfsamson, ["Green threads explained"](https://github.com/k0nserv/book-green-threads-explained)
- Quentin Carbonneaux, ["Green Threads Explained in C"](https://c9x.me/articles/gthreads/intro.html)
- Stack Overflow, ["Why did Rust remove the green threading model?"](https://stackoverflow.com/questions/29428318/why-did-rust-remove-the-green-threading-model-whats-the-disadvantage)
- Software Engineering StackExchange, ["Why not green threads?"](https://softwareengineering.stackexchange.com/questions/120384/why-not-green-threads)
