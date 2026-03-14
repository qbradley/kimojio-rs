# Virtual Clock Guide

Kimojio's virtual clock feature enables deterministic timing in tests.
Instead of waiting real wall-clock time for sleeps, timeouts, and deadlines,
test code explicitly advances a virtual clock, causing timers to fire
instantly and predictably.

## Getting Started

### Enable the feature

Add the `virtual-clock` feature to your dev-dependencies:

```toml
[dev-dependencies]
kimojio = { version = "0.14", features = ["virtual-clock"] }
```

### Create a virtual runtime

Use `Runtime::new_virtual()` to get a runtime with an integrated virtual clock:

```rust
use kimojio::{Runtime, clock::VirtualClock};
use kimojio::configuration::Configuration;
use std::time::Duration;

let (mut runtime, clock) = Runtime::new_virtual(0, Configuration::new());
let clock2 = clock.clone();
runtime.block_on(async move {
    // Virtual time does not advance on its own.
    // Advance it explicitly:
    clock2.advance(Duration::from_secs(60));
});
```

### Zero migration cost

Existing code requires **no changes** when upgrading to a kimojio version that
includes virtual clock support. The feature is additive and behind a Cargo
feature flag. `Runtime::new()`, `Configuration::new()`, and all existing APIs
remain unchanged.

## API Reference

### `VirtualClock`

| Method | Description |
|--------|-------------|
| `VirtualClock::new(start)` | Create a clock starting at `start` |
| `clock.now()` | Current virtual time |
| `clock.advance(duration)` | Advance time by `duration`, fire expired timers |
| `clock.advance_to(instant)` | Advance to a specific instant |
| `clock.next_deadline()` | Peek at the next pending timer deadline |
| `clock.pending_timers()` | Count of pending timers |

### Timer operations (virtual-aware)

These functions automatically use virtual time when a `VirtualClock` is active:

| Function | Description |
|----------|-------------|
| `operations::sleep(duration)` | Sleep for `duration` |
| `operations::sleep_until(deadline)` | Sleep until `deadline` |
| `operations::timeout_at(deadline, future)` | Run `future` with a deadline |

### Deadline I/O (virtual-aware)

These existing functions use virtual time for deadline computation:

- `write_with_deadline`, `writev_with_deadline`, `read_with_deadline`
- `AsyncEvent::wait_with_deadline`, `AsyncEvent::wait_with_timeout`
- `AsyncLock::lock_with_timeout`

## Common Patterns

### Deterministic sleep testing

```rust
use kimojio::{Runtime, operations};
use kimojio::configuration::Configuration;
use std::time::Duration;

#[test]
fn retry_logic_test() {
    let (mut runtime, clock) = Runtime::new_virtual(0, Configuration::new());
    let clock2 = clock.clone();

    runtime.block_on(async move {
        use std::pin::pin;
        use std::task::Poll;

        // Create a sleep that would take 60 seconds in real time
        let mut sleep = pin!(operations::sleep(Duration::from_secs(60)));

        // Poll once to register the timer
        futures::future::poll_fn(|cx| {
            let _ = sleep.as_mut().poll(cx);
            Poll::Ready(())
        }).await;

        // Advance virtual time — completes instantly
        clock2.advance(Duration::from_secs(60));
        sleep.await.unwrap(); // Ready immediately!
    });
}
```

### Timeout testing with spawned tasks

```rust
use kimojio::{Runtime, TimeoutError, operations};
use kimojio::configuration::Configuration;
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

#[test]
fn timeout_fires_on_advance() {
    let (mut runtime, clock) = Runtime::new_virtual(0, Configuration::new());
    let clock2 = clock.clone();

    runtime.block_on(async move {
        let deadline = clock2.now() + Duration::from_secs(5);
        let result_cell = Rc::new(Cell::new(None));
        let r = result_cell.clone();

        operations::spawn_task(async move {
            let result = operations::timeout_at(
                deadline,
                std::future::pending::<()>(),
            ).await;
            r.set(Some(result));
        });

        // Let the spawned task start
        operations::yield_io().await;

        // Advance past deadline
        clock2.advance(Duration::from_secs(5));
        operations::yield_io().await;

        assert_eq!(result_cell.get(), Some(Err(TimeoutError::Timeout)));
    });
}
```

### Stepping through time

Use `next_deadline()` to advance one timer at a time:

```rust
# use kimojio::clock::VirtualClock;
# use std::time::Instant;
# let clock = VirtualClock::new(Instant::now());
while let Some(next) = clock.next_deadline() {
    clock.advance_to(next);
    // Check state after each timer fires
}
```

## Caveats

### Real I/O deadlines

The `timeout` parameter on I/O operations (e.g., `read_with_deadline`) is
converted to a `Duration` and submitted as a real io_uring timeout SQE. The
virtual clock only affects the `Instant::now()` call used to compute how much
time remains — it does not replace the kernel timeout mechanism itself. For
pure timer testing, use `sleep()`, `sleep_until()`, and `timeout_at()`.

### Timer ordering vs execution ordering

When advancing past multiple deadlines simultaneously, timers fire (wakers are
called) in deadline order. However, the runtime's task scheduler determines
the actual execution order of woken tasks. In practice, single-threaded
kimojio runtimes execute tasks in the order they become ready, so timer
ordering is preserved.

### Single-threaded only

`VirtualClock` is `!Send` and `!Sync`, matching kimojio's single-threaded
runtime model. Do not attempt to share a `VirtualClock` across threads.
