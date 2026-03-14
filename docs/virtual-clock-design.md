# Virtual Clock Design Document

## Overview

The virtual clock feature provides deterministic timing for kimojio's async
runtime, enabling tests that involve timeouts, sleeps, and deadlines to run
in microseconds instead of waiting for real wall-clock time.

## Architecture

### Component Diagram

```
┌─────────────────────────────────────────────────────┐
│                    User Test Code                    │
│  clock.advance(60s)  ←→  operations::sleep(60s)     │
└───────┬─────────────────────────────┬───────────────┘
        │                             │
        ▼                             ▼
┌───────────────┐         ┌───────────────────────┐
│  VirtualClock │         │   VirtualSleepFuture   │
│  ┌──────────┐ │         │  deadline: Instant     │
│  │ BinaryHeap│◄────────►│  clock: Rc<dyn Clock>  │
│  │ (timers) │ │ register│  timer_id: Option<id>  │
│  └──────────┘ │  /cancel│  completed: bool       │
│  current: Ins │         └───────────────────────┘
└───────┬───────┘
        │ Rc<dyn Clock>
        ▼
┌───────────────────────────────────────────────────┐
│                    TaskState                       │
│  clock: Option<Rc<dyn Clock>>   (feature-gated)   │
└───────────────────────────────┬───────────────────┘
                                │
        ┌───────────────────────┼───────────────────┐
        ▼                       ▼                   ▼
  operations.rs          async_event.rs       async_lock.rs
  clock_now()            clock_now()          clock_now()
```

### Clock Trait

The `Clock` trait abstracts time queries and timer management:

```rust
pub trait Clock: 'static {
    fn now(&self) -> Instant;
    fn register_timer(&self, deadline: Instant, waker: Waker) -> TimerId;
    fn cancel_timer(&self, id: TimerId);
    fn is_real(&self) -> bool { true }
}
```

- **`SystemClock`**: Returns `Instant::now()`. Timer operations are no-ops
  (real timers are managed by io_uring).
- **`VirtualClock`**: Returns a user-controlled instant. Timers are stored in
  a priority queue and fire when `advance()` moves time past their deadlines.

### VirtualClock Timer Wheel

Internally, `VirtualClock` uses a `BinaryHeap<VirtualTimer>` configured as a
min-heap ordered by `(deadline, TimerId)`:

- **Deadline ordering**: Earliest-deadline timers fire first.
- **Tie-breaking**: Equal-deadline timers fire in registration order (lowest
  `TimerId` first), providing fully deterministic behavior.
- **Complexity**: O(log n) for register/cancel, O(k log n) for advance
  (where k = timers fired).

### TaskState Integration

The clock is stored as `Option<Rc<dyn Clock>>` in `TaskState`, the
thread-local runtime state struct. This is accessed by:

1. **`clock_now()`** — Called by deadline I/O functions to get current time.
2. **`sleep()`** — Checks for virtual clock to dispatch to `VirtualSleepFuture`.

The `Option` ensures zero overhead when no virtual clock is installed
(the common case in production).

## Design Decisions

### Why a feature flag?

The `virtual-clock` feature flag ensures:

1. **Zero compile-time overhead**: Without the feature, `clock_now()` compiles
   to a direct `Instant::now()` call. No `Option` check, no trait dispatch.
2. **No public API surface change**: Existing users see no new types or
   methods unless they opt in.
3. **Clean dependency boundary**: Test-only code stays behind a gate.

### Why `Rc<RefCell<>>` instead of `Arc<Mutex<>>`?

Kimojio is a single-threaded runtime. Using `Rc<RefCell<>>`:

- Avoids atomic operations (faster).
- Makes `VirtualClock` `!Send` + `!Sync`, which correctly reflects the
  single-threaded constraint.
- Allows interior mutability without `unsafe`.

### Why `Rc<dyn Clock>` instead of generics?

Making `Runtime` generic over `Clock` would change its public API signature
and require all users to specify a type parameter. Using trait objects:

- Preserves `Runtime::new()` signature exactly.
- `Configuration::set_clock()` accepts `Rc<dyn Clock>`.
- The dynamic dispatch cost is negligible (one virtual call per
  `clock_now()` invocation, on a non-hot path).

### Why dual `#[cfg]` for `SleepFuture`?

`pin_project_lite` does not support `#[cfg]` attributes on enum variants.
Rather than adding a dependency on the full `pin-project` crate, we use two
separate type definitions:

- **Without feature**: Original `pin_project_lite` struct wrapping
  `UnitFuture<'a>` — identical compiled output to before.
- **With feature**: A struct wrapping an internal `SleepFutureInner` enum
  with `Real(UnitFuture)` and `Virtual(VirtualSleepFuture)` variants.
  Pin projection uses `unsafe` but is straightforward since
  `VirtualSleepFuture` is `Unpin`.

### Why `clock_now()` lives in `clock.rs`?

Placing the helper in `clock.rs` rather than `operations.rs` avoids coupling
`async_event.rs` and `async_lock.rs` to the operations module. All modules
that need current time import from the same source.

## Virtual vs Real Timer Paths

### Real path (default)

```
sleep(duration)
  → Timespec::from(duration)
  → opcode::Timeout → SQE
  → io_uring_enter → kernel timer
  → CQE with ETIME → Ready(Ok(()))
```

### Virtual path (with VirtualClock)

```
sleep(duration)
  → check TaskState.clock → is_real() == false
  → VirtualSleepFuture::new(clock.now() + duration)
  → poll: clock.now() < deadline → register_timer → Pending
  → test: clock.advance(duration)
  → advance_to: timer.deadline <= current → waker.wake()
  → poll: clock.now() >= deadline → Ready(Ok(()))
```

## Performance Characteristics

| Scenario | Overhead |
|----------|----------|
| Feature disabled | Zero — `clock_now()` = `Instant::now()` |
| Feature enabled, no virtual clock | One `Option::is_none()` check per `clock_now()` |
| Feature enabled, with virtual clock | `Rc<RefCell>` borrow + virtual dispatch per `clock_now()` |
| Virtual sleep registration | O(log n) heap push |
| Virtual time advance | O(k log n) for k expired timers |

## Known Limitations

1. **Real I/O timeouts are not virtualized**: The `timeout` parameter on I/O
   operations becomes a kernel io_uring timeout. Virtual clock only affects
   the `Instant::now()` used to compute the remaining duration.

2. **No automatic time advancement**: The virtual clock never advances on its
   own. Test code must explicitly call `advance()`.

3. **Single-threaded only**: `VirtualClock` is `!Send`/`!Sync`.

## Future Work

- Auto-advance mode that steps to the next deadline when all tasks are blocked.
- Integration with `#[kimojio::test]` macro for automatic virtual clock setup.
- Virtual clock statistics (timers registered, fired, cancelled).
