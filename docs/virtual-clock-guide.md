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

### Enable virtual time in a test

Inside an async runtime context, call `operations::virtual_clock_enable(true)`:

```rust
use kimojio::{Runtime, operations};
use kimojio::configuration::Configuration;
use std::time::Duration;

let mut runtime = Runtime::new(0, Configuration::new());
runtime.block_on(async {
    // Enable virtual clock — all timing is now deterministic
    operations::virtual_clock_enable(true);

    // Advance time explicitly:
    operations::virtual_clock_advance(Duration::from_secs(60));
});
```

### Opt-in via feature flag

The virtual clock is behind the `virtual-clock` Cargo feature flag and is
entirely additive. `Runtime::new()`, `Configuration::new()`, and all existing
APIs remain unchanged when the feature is not enabled. When the feature *is*
enabled, behavior does not change until `virtual_clock_enable(true)` is called.

## API Reference

### Virtual clock control (`operations::`)

| Function | Description |
|----------|-------------|
| `virtual_clock_enable(bool)` | Enable or disable the virtual clock |
| `virtual_clock_advance(duration)` | Advance time by `duration`, fire expired timers |
| `virtual_clock_advance_to(instant)` | Advance to a specific instant |
| `virtual_clock_now()` | Current time (virtual or real) |
| `virtual_clock_epoch()` | Start time of the virtual clock |
| `virtual_clock_next_deadline()` | Peek at the next pending timer deadline |
| `virtual_clock_pending_timers()` | Count of pending timers |
| `virtual_clock_advance_idle(duration)` | Queue an advance that fires at the next idle point |
| `virtual_clock_pending_idle_advances()` | Count of queued idle advances |
| `virtual_clock_advance_idle_default(Option<Duration>)` | Set/clear default idle advance duration |

### Test utilities

| Function | Description |
|----------|-------------|
| `operations::poll_once(fut)` | Poll a future once, return `Some(output)` if ready |
| `#[kimojio::test]` | Proc macro for async tests; call `virtual_clock_enable(true)` inside |

### Timer operations (virtual-aware)

These functions automatically use virtual time when the virtual clock is active:

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

### Using `#[kimojio::test]` with virtual clock

Call `virtual_clock_enable(true)` at the start of your async test body:

```rust
use kimojio::operations;
use std::pin::pin;
use std::time::Duration;

#[kimojio::test]
async fn sleep_completes_after_advance() {
    operations::virtual_clock_enable(true);

    let mut sleep = pin!(operations::sleep(Duration::from_secs(60)));
    assert!(operations::poll_once(sleep.as_mut()).await.is_none());
    operations::virtual_clock_advance(Duration::from_secs(60));
    sleep.await.unwrap();
}
```

### RAII guard for test isolation

To prevent virtual clock state from leaking to subsequent tests (e.g., on
panic), use an RAII guard:

```rust
use kimojio::operations;
use std::time::Duration;

#[kimojio::test]
async fn my_test() {
    operations::virtual_clock_enable(true);
    struct ClockGuard;
    impl Drop for ClockGuard {
        fn drop(&mut self) {
            operations::virtual_clock_enable(false);
        }
    }
    let _guard = ClockGuard;

    operations::virtual_clock_advance(Duration::from_secs(60));
    // ... test body
}
```

### Deterministic sleep testing

```rust
use kimojio::{Runtime, operations};
use kimojio::configuration::Configuration;
use std::time::Duration;

#[test]
fn retry_logic_test() {
    let mut runtime = Runtime::new(0, Configuration::new());

    runtime.block_on(async {
        operations::virtual_clock_enable(true);

        // Queue an idle advance — fires when no tasks are ready
        operations::virtual_clock_advance_idle(Duration::from_secs(60));

        // Sleep completes instantly via the idle advance
        operations::sleep(Duration::from_secs(60)).await.unwrap();
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
    let mut runtime = Runtime::new(0, Configuration::new());

    runtime.block_on(async {
        operations::virtual_clock_enable(true);

        let deadline = operations::virtual_clock_now() + Duration::from_secs(5);
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
        operations::virtual_clock_advance(Duration::from_secs(5));
        operations::yield_io().await;

        assert_eq!(result_cell.get(), Some(Err(TimeoutError::Timeout)));
    });
}
```

### Stepping through time

Use `virtual_clock_next_deadline()` to advance one timer at a time:

```rust
use kimojio::operations;

# async fn example() {
while let Some(next) = operations::virtual_clock_next_deadline() {
    operations::virtual_clock_advance_to(next);
    // Check state after each timer fires
}
# }
```

### Using `poll_once` for the poll-advance-await pattern

The `operations::poll_once()` helper reduces boilerplate when testing
timer registration:

```rust
use std::pin::pin;
use std::time::Duration;
use kimojio::operations;

# async fn example() {
let mut sleep = pin!(operations::sleep(Duration::from_secs(10)));

// One line to poll without completing:
assert!(operations::poll_once(sleep.as_mut()).await.is_none());

operations::virtual_clock_advance(Duration::from_secs(10));
assert!(operations::poll_once(sleep.as_mut()).await.is_some());
# }
```

## Caveats

### Manual advancement only

Virtual time never advances on its own. Test code must explicitly call
`virtual_clock_advance()` or `virtual_clock_advance_to()` to move time
forward and fire timers. When spawned tasks contain internal sleeps (e.g.,
retry loops with backoff), the test must advance the clock far enough to
cover all internal deadlines.

Use `virtual_clock_next_deadline()` to step through timers one at a time:

```rust
use kimojio::operations;

# async fn example() {
while let Some(next) = operations::virtual_clock_next_deadline() {
    operations::virtual_clock_advance_to(next);
    // check state after each timer fires
}
# }
```

A future auto-advance mode is planned to simplify testing of code with
internal sleeps. See `docs/virtual-clock-auto-advance-future.md` for the
design.

### Idle advance

For tests where you know how many time-steps are needed, you can queue
advances that fire automatically when the runtime has no ready tasks:

```rust
use kimojio::operations;
use std::time::Duration;

# async fn example() {
// Queue two successive 30-second advances
operations::virtual_clock_advance_idle(Duration::from_secs(30));
operations::virtual_clock_advance_idle(Duration::from_secs(30));

// First sleep completes when the first idle advance fires
operations::sleep(Duration::from_secs(30)).await.unwrap();

// Second sleep completes when the second idle advance fires
operations::sleep(Duration::from_secs(30)).await.unwrap();
# }
```

Each advance waits for its own idle point — after the first advance fires,
newly woken tasks are polled to completion before the next advance fires.
This makes idle advance safe for multi-step test scenarios.

For tests that don't know how many time-steps they need, set a default
instead of pre-queuing individual advances:

```rust
use kimojio::operations;
use std::time::Duration;

# async fn example() {
// Advance by 1ms on every idle point — sleeps resolve automatically
operations::virtual_clock_advance_idle_default(Some(Duration::from_millis(1)));

operations::sleep(Duration::from_secs(60)).await.unwrap();

// Clear the default when done
operations::virtual_clock_advance_idle_default(None);
# }
```

Explicit queue entries (from `virtual_clock_advance_idle`) are consumed
first; the default only fires when the queue is empty. Pass `None` or
`Some(Duration::ZERO)` to clear the default.

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

The virtual clock uses thread-local state and is `!Send`/`!Sync`, matching
kimojio's single-threaded runtime model.
