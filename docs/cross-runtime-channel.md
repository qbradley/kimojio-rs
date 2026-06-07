# Cross Runtime Channel

`kimojio-stack::channel::cross_thread` provides bounded in-memory channels for
communication across kimojio-stack runtimes, OS threads, and Tokio-compatible
async tasks.

Unlike the existing stackful channels, cross-runtime channels use thread-safe
state and explicit endpoint types. Choose endpoints by where each side runs:

| Participant | Endpoint type | Waiting method |
|-------------|---------------|----------------|
| OS thread | `ThreadSender` / `ThreadReceiver` | `send_blocking` / `recv_blocking` |
| kimojio-stack coroutine | `StackfulSender` / `StackfulReceiver` | `send(cx, value)` / `recv(cx)` |
| Tokio-compatible task | `AsyncSender` / `AsyncReceiver` | `send(value).await` / `recv().await` |

## Getting Started

For ordinary thread-to-thread communication:

```rust
use kimojio_stack::channel::cross_thread;

let (tx, rx) = cross_thread::thread(1);
tx.send_blocking("hello").unwrap();
assert_eq!(rx.recv_blocking().unwrap(), "hello");
```

For stackful runtime code, use stackful endpoints from a spawned coroutine:

```rust,no_run
use kimojio_stack::Runtime;
use kimojio_stack::channel::cross_thread;

let (tx, rx) = cross_thread::stackful(1);
let mut runtime = Runtime::new();

runtime.block_on(|cx| {
    cx.scope(|scope| {
        let sender = scope.spawn(move |cx| tx.send(cx, 42).unwrap());
        let receiver = scope.spawn(move |cx| rx.recv(cx).unwrap());

        sender.join(cx).unwrap();
        assert_eq!(receiver.join(cx).unwrap(), 42);
    });
});
```

For mixed participants, use the builder:

```rust,no_run
use kimojio_stack::channel::cross_thread;

let (thread_tx, stackful_rx) = cross_thread::bounded::<u64>(64)
    .thread_to_stackful();
```

Available builder combinations are:

- `thread()`
- `stackful()`
- `tokio()`
- `thread_to_stackful()`
- `stackful_to_thread()`
- `tokio_to_stackful()`
- `stackful_to_tokio()`
- `thread_to_tokio()`
- `tokio_to_thread()`

## Behavior

The channel is bounded and has nonzero capacity. Creating one with capacity
`0` panics.

All endpoint types support nonblocking fast paths:

- `try_send(value)` returns `Ok(())`, `TrySendError::Full(value)`, or
  `TrySendError::Closed(value)`.
- `try_recv()` returns `Ok(value)`, `TryRecvError::Empty`, or
  `TryRecvError::Closed`.

Blocking or waiting methods only wait when the operation cannot complete
immediately. Ready send and receive operations do not park, await, block, or
enter a kernel wait.

Dropping the final sender wakes receivers. They may drain buffered messages and
then receive `RecvError` once the channel is empty. Dropping the final receiver
wakes senders, which receive `SendError<T>` with their unsent value.

Stackful waiting methods are intended for spawned stackful coroutine contexts.
From root runtime code, use `try_send`/`try_recv` or spawn a coroutine before
calling the waiting methods.

## Performance Notes

The message queue is backed by a preallocated `crossbeam_queue::ArrayQueue`.
Ready paths are designed to avoid channel-owned heap allocation after channel
construction. Contended paths allocate or register endpoint-specific waiters as
needed.

Stackful cross-thread wakeups use kimojio-stack external wake integration. A
wake from another thread queues the target coroutine in the runtime's external
ready queue. If the runtime root may be blocked in `io_uring_enter`, an
eventfd-backed poll operation interrupts the kernel wait; otherwise a condition
variable wakes a root thread waiting without I/O.

Benchmark smoke coverage is available with:

```sh
cargo bench -p kimojio-stack --bench runtime_baseline -- --test
```

The benchmark includes ready send/receive, thread ping-pong, stackful
cross-runtime ping-pong, and Tokio-compatible ping-pong cases.
