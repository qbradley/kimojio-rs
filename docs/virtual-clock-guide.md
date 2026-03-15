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
| `clock.epoch()` | Start time of the clock |
| `clock.advance(duration)` | Advance time by `duration`, fire expired timers |
| `clock.advance_to(instant)` | Advance to a specific instant |
| `clock.set_auto_advance(bool)` | Enable/disable auto-advance mode |
| `clock.is_auto_advance()` | Query auto-advance state |
| `clock.next_deadline()` | Peek at the next pending timer deadline |
| `clock.pending_timers()` | Count of pending timers |

### Test utilities

| Function | Description |
|----------|-------------|
| `operations::poll_once(fut)` | Poll a future once, return `true` if ready |
| `#[kimojio::test(virtual)]` | Proc macro for zero-boilerplate virtual tests |

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

### Using `#[kimojio::test(virtual)]`

The proc macro eliminates boilerplate for virtual clock tests:

```rust
use kimojio::clock::VirtualClock;
use kimojio::operations;
use std::pin::pin;
use std::time::Duration;

#[kimojio::test(virtual)]
async fn sleep_completes_after_advance(clock: VirtualClock) {
    let mut sleep = pin!(operations::sleep(Duration::from_secs(60)));
    assert!(!operations::poll_once(sleep.as_mut()).await);
    clock.advance(Duration::from_secs(60));
    sleep.await.unwrap();
}

// Clock parameter is optional:
#[kimojio::test(virtual)]
async fn no_clock_needed() {
    operations::sleep(Duration::ZERO).await.unwrap();
}
```

### Using `poll_once` for the poll-advance-await pattern

The `operations::poll_once()` helper reduces boilerplate when testing
timer registration:

```rust
use std::pin::pin;
use std::time::Duration;
use kimojio::operations;

# async fn example(clock: kimojio::clock::VirtualClock) {
let mut sleep = pin!(operations::sleep(Duration::from_secs(10)));

// Old way — manual poll_fn:
// futures::future::poll_fn(|cx| {
//     let _ = sleep.as_mut().poll(cx);
//     std::task::Poll::Ready(())
// }).await;

// New way — one line:
assert!(!operations::poll_once(sleep.as_mut()).await);

clock.advance(Duration::from_secs(10));
assert!(operations::poll_once(sleep.as_mut()).await);
# }
```

### Auto-advance for retry loops

When testing code that calls `sleep().await` internally (e.g., retry loops
with exponential backoff), manually advancing the clock isn't possible because
the test doesn't control the poll cycle. Enable **auto-advance** to make all
virtual sleeps complete instantly:

```rust
use kimojio::clock::VirtualClock;
use kimojio::operations;
use std::time::Duration;

async fn retry_with_backoff<F, Fut>(
    initial_delay: Duration,
    max_retries: u32,
    mut op: F,
) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let mut delay = initial_delay;
    for _ in 0..=max_retries {
        if op().await { return true; }
        operations::sleep(delay).await.unwrap();
        delay *= 2;
    }
    false
}

#[kimojio::test(virtual)]
async fn retry_backoff_test(clock: VirtualClock) {
    clock.set_auto_advance(true); // sleeps resolve instantly

    let mut attempts = 0u32;
    let succeeded = retry_with_backoff(
        Duration::from_secs(1),
        5,
        || {
            attempts += 1;
            async move { attempts >= 3 }
        },
    ).await;

    assert!(succeeded);
    // Virtual time advanced: 1s + 2s = 3s (2 failures before success)
    let elapsed = clock.now().duration_since(clock.epoch());
    assert_eq!(elapsed, Duration::from_secs(3));
}
```

Auto-advance can be toggled at any point. Combine manual and auto modes:

```rust
# use kimojio::clock::VirtualClock;
# use std::time::Duration;
# fn example(clock: &VirtualClock) {
// Start manual for precise control
clock.advance(Duration::from_secs(5));

// Switch to auto for a retry loop
clock.set_auto_advance(true);
// ... retry_with_backoff(...).await;

// Back to manual
clock.set_auto_advance(false);
clock.advance(Duration::from_secs(1));
# }
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
