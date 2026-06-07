# Cross Runtime Channel

## Overview

The cross runtime channel adds a bounded, in-memory message-passing primitive for code that crosses the original `kimojio-stack` single-runtime boundary. It lets stackful runtimes, ordinary OS threads, and Tokio-compatible async tasks communicate through one shared channel while each endpoint waits in the way appropriate for its execution model.

The implementation is intentionally separate from the existing single-runtime `bounded` and `unbounded` channels. Existing channels remain optimized for one cooperative scheduler. The new `channel::cross_thread` module uses cross-thread-safe shared state, explicit endpoint types, and an external stackful wake path so one runtime or thread can wake a stackful coroutine parked in another runtime.

## Architecture and Design

### High-Level Architecture

Each bounded channel owns:

- a preallocated `crossbeam_queue::ArrayQueue<T>` for message storage;
- atomic sender and receiver counts for closure detection;
- one wait queue for blocked senders and one wait queue for blocked receivers;
- endpoint handles selected by `cross_thread::bounded(capacity)` builder methods.

The queue handles the hot message path. The custom wrapper handles semantics that a generic queue does not know about: channel closure, endpoint-specific waiting, wake ordering, and integration with `kimojio-stack` stackful runtimes.

Wait queues can hold three classes of waiters:

| Endpoint type | Wait representation | Waiting behavior |
|---------------|---------------------|------------------|
| Thread | `Condvar` generation wait | Blocks the current OS thread |
| Stackful | `ExternalWaiter` | Parks only the current stackful coroutine |
| Tokio-compatible async | `Waker` | Returns `Poll::Pending` and wakes the async task |

Ready send and receive operations try the queue first. If the operation succeeds, the channel wakes one opposite-side waiter and returns. If the operation cannot proceed, the blocking path registers an endpoint-specific waiter, rechecks readiness under the wait queue lock, and only then sleeps, parks, or returns `Pending`.

### Design Decisions

**Crossbeam for storage, not waiting.** `ArrayQueue<T>` provides a bounded MPMC queue with a preallocated ring buffer and efficient ready paths. It is not enough by itself because it cannot park stackful coroutines or manage Tokio task wakers. Channel wait and close behavior is therefore implemented around the queue.

**Explicit endpoint types.** The builder returns different handle types for thread, stackful, and async use. This keeps blocking behavior visible in type and method names:

- `ThreadSender::send_blocking` and `ThreadReceiver::recv_blocking` may block an OS thread.
- `StackfulSender::send` and `StackfulReceiver::recv` require `&RuntimeContext` and park a stackful coroutine.
- `AsyncSender::send` and `AsyncReceiver::recv` return futures compatible with Tokio and other standard `Future` executors.

**External stackful wake path.** Existing stackful waiters are runtime-thread-local. Cross-runtime channels use the new external wake infrastructure instead, so a sender on another thread can enqueue a task ID into the target runtime's external ready queue. When the runtime root is waiting without io_uring work, it uses a condition variable. When it may be blocked in `io_uring_enter`, it arms an eventfd-backed `POLL_ADD` wake source so external notification interrupts the kernel wait. Correctness does not depend on new-kernel-only io_uring futex operations.

**No kernel wait on ready paths.** A send with capacity or a receive with data completes by operating on the in-memory queue and notifying already-registered waiters if needed. Kernel-visible wakeups only matter after a stackful coroutine has actually parked and the runtime root must be interrupted.

**Closure wakes all opposite waiters.** Dropping the final sender wakes receivers so they can drain remaining messages and then observe `RecvError`. Dropping the final receiver wakes senders so they can recover the unsent value through `SendError<T>`.

### Integration Points

The public module is `kimojio_stack::channel::cross_thread`. It reuses the existing channel error types from `kimojio_stack::channel`:

- `SendError<T>`
- `TrySendError<T>`
- `RecvError`
- `TryRecvError`

Stackful endpoints integrate directly through their `send(cx, value)` and `recv(cx)` methods. They do not implement `Waitable` in this version because cross-thread external waiter registrations need cancellation support before they are safe for timeout or multi-wait APIs.

Tokio compatibility is implemented through `std::future::Future` and `std::task::Waker`. Tokio is used for tests and benchmarks, but the channel implementation does not depend on Tokio runtime internals.

## User Guide

### Prerequisites

Use the `kimojio-stack` crate on Linux. Cross-runtime stackful waiting requires a valid stackful coroutine context for `StackfulSender::send` and `StackfulReceiver::recv`; do not use those blocking methods from the root `Runtime::block_on` closure without spawning a stackful coroutine.

Capacity must be nonzero. `cross_thread::bounded(0)` panics with the same fail-fast behavior used by other bounded channel constructors in this crate.

### Basic Usage

Pick endpoint types based on where each side runs.

| If this side runs in... | Use this endpoint | Waiting method |
|-------------------------|-------------------|----------------|
| Ordinary OS thread | `ThreadSender` / `ThreadReceiver` | `send_blocking` / `recv_blocking` |
| `kimojio-stack` coroutine | `StackfulSender` / `StackfulReceiver` | `send(cx, value)` / `recv(cx)` |
| Tokio-compatible async task | `AsyncSender` / `AsyncReceiver` | `send(value).await` / `recv().await` |

For same-kind endpoints, use the convenience constructors:

- `cross_thread::thread(capacity)`
- `cross_thread::stackful(capacity)`
- `cross_thread::tokio(capacity)`

For mixed endpoints, use the builder:

- `cross_thread::bounded(capacity).thread_to_stackful()`
- `cross_thread::bounded(capacity).stackful_to_thread()`
- `cross_thread::bounded(capacity).tokio_to_stackful()`
- `cross_thread::bounded(capacity).stackful_to_tokio()`
- `cross_thread::bounded(capacity).thread_to_tokio()`
- `cross_thread::bounded(capacity).tokio_to_thread()`

### Advanced Usage

Use `try_send` and `try_recv` on any endpoint type for nonblocking fast paths. These methods never park, await, or block. They return `Full`, `Empty`, or `Closed` states when the operation cannot complete immediately.

Use cloned endpoints when multiple producers or consumers share one side of the channel. The channel remains open for a side until all clones on that side are dropped.

Use stackful endpoints from spawned stackful coroutines when the operation may wait. From root runtime code, use `try_send` or `try_recv`, or spawn a coroutine that can safely park.

## API Reference

### Key Components

| Component | Purpose |
|-----------|---------|
| `cross_thread::bounded<T>(capacity)` | Builder entry point for explicit endpoint selection |
| `Builder::thread` | Blocking thread sender and receiver |
| `Builder::stackful` | Stackful sender and receiver |
| `Builder::tokio` | Tokio-compatible async sender and receiver |
| `Builder::*_to_*` | Mixed endpoint pairs for thread, stackful, and Tokio-compatible participants |
| `ThreadSender<T>` / `ThreadReceiver<T>` | OS-thread handles with nonblocking and blocking methods |
| `StackfulSender<T>` / `StackfulReceiver<T>` | Stackful handles with nonblocking and coroutine-parking methods |
| `AsyncSender<T>` / `AsyncReceiver<T>` | Standard future/waker async handles |

### Configuration Options

The only channel configuration is bounded capacity. Capacity controls backpressure and memory reserved by the underlying queue. There is no unbounded variant and no fairness or spinning configuration in this implementation.

## Testing

### How to Test

The primary verification commands are:

```sh
cargo test -p kimojio-stack --all-targets
cargo bench -p kimojio-stack --bench runtime_baseline -- --test
cargo clippy -p kimojio-stack --all-targets -- -D warnings
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

The runtime baseline benchmark contains named cross-thread channel cases for ready send/receive, thread ping-pong, stackful cross-runtime ping-pong, and Tokio-compatible ping-pong.

### Edge Cases

The implementation handles:

- zero-capacity construction rejection;
- full sends and empty receives across each endpoint type;
- final sender and final receiver drops waking blocked opposite-side waiters;
- send/receive races while registering waiters;
- async future drop cleanup for registered wakers;
- warmed ready-path send/receive loops with zero channel-owned heap allocations.

## Limitations and Future Work

This is a bounded in-process channel. It is not cross-process IPC, shared memory IPC, networking, or a replacement for the existing single-runtime channels.

Strict global fairness among competing senders and receivers is not guaranteed. Wakeups preserve progress and favor one waiter at a time for ordinary state transitions.

The stackful external wake baseline uses eventfd plus io_uring poll when the runtime root may be sleeping in the kernel. `MSG_RING` may be a future optimization for runtime-to-runtime wakeups, but it does not replace the need for an OS-thread-compatible wake source. io_uring futex operations may become useful on newer kernels, but correctness does not depend on them.
