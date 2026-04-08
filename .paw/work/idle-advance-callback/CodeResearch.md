# CodeResearch: Idle Advance Callback

> Branch: `feature/idle-advance-callback` (based on `feature/virtual-clock-deterministic-timing`)
> Feature flag: `virtual-clock`

---

## §1 — VirtualClockState idle advance fields and methods

**File:** `kimojio/src/virtual_clock.rs`

### Struct definition (lines 48–55)

```rust
pub(crate) struct VirtualClockState {
    epoch: Instant,                          // :49
    current: Instant,                        // :50
    timers: BinaryHeap<VirtualTimer>,        // :51
    next_timer_id: u64,                      // :52
    idle_advances: VecDeque<Duration>,       // :53
    idle_advance_default: Option<Duration>,  // :54
}
```

Fields initialized in `new()` at lines 96–97:

```rust
idle_advances: VecDeque::new(),   // :96
idle_advance_default: None,       // :97
```

### Methods operating on idle advance fields

| Method | Vis | Line | Signature |
|--------|-----|------|-----------|
| `queue_idle_advance` | `pub(crate)` | 193 | `fn queue_idle_advance(&mut self, duration: Duration)` |
| `has_idle_advances` | `pub` (`#[cfg(test)]`) | 199 | `fn has_idle_advances(&self) -> bool` |
| `take_next_idle_advance` | `pub` (`#[cfg(test)]`) | 208 | `fn take_next_idle_advance(&mut self) -> Option<(usize, Vec<Waker>)>` |
| `pending_idle_advances` | `pub(crate)` | 214 | `fn pending_idle_advances(&self) -> usize` |
| `set_idle_advance_default` | `pub(crate)` | 224 | `fn set_idle_advance_default(&mut self, duration: Option<Duration>)` |
| `idle_advance_default` | `pub` (`#[cfg(test)]`) | 230 | `fn idle_advance_default(&self) -> Option<Duration>` |
| `can_idle_advance` | `pub(crate)` | 236 | `fn can_idle_advance(&self) -> bool` |
| `take_next_idle_advance_or_default` | `pub(crate)` | 246 | `fn take_next_idle_advance_or_default(&mut self) -> Option<(usize, Vec<Waker>)>` |

### Key method bodies

**`queue_idle_advance`** (line 193–195): Pushes to back of `idle_advances` deque.

**`can_idle_advance`** (line 236–238):
```rust
pub(crate) fn can_idle_advance(&self) -> bool {
    !self.idle_advances.is_empty() || self.idle_advance_default.is_some()
}
```

**`take_next_idle_advance_or_default`** (line 246–252):
```rust
pub(crate) fn take_next_idle_advance_or_default(&mut self) -> Option<(usize, Vec<Waker>)> {
    let duration = self
        .idle_advances
        .pop_front()
        .or(self.idle_advance_default)?;
    Some(self.advance(duration))
}
```

**`set_idle_advance_default`** (line 224–226): Filters out `Duration::ZERO`:
```rust
self.idle_advance_default = duration.filter(|d| !d.is_zero());
```

### `next_deadline()` (line 157–159)

```rust
pub(crate) fn next_deadline(&self) -> Option<Instant> {
    self.timers.peek().map(|t| t.deadline)
}
```

Accesses `self.timers` (`BinaryHeap<VirtualTimer>`) via `.peek()` — returns the minimum deadline. This is a read-only `&self` method. **Safe to call before or after taking the callback — no borrow conflict.**

### `now()` (line 110–112)

```rust
pub(crate) fn now(&self) -> Instant {
    self.current
}
```

Returns `self.current`. Also `&self`, no borrow issue. **Both `now()` and `next_deadline()` can be called to extract callback parameters while holding a `&VirtualClockState` reference.**

---

## §2 — Runtime event loop integration

**File:** `kimojio/src/runtime.rs`

### Busy-poll override for virtual clock (lines 392–399)

```rust
#[cfg(feature = "virtual-clock")]
let busy_poll = busy_poll
    || (task_state.clock.is_some()
        && (task_state.any_ready()
            || task_state
                .clock
                .as_ref()
                .is_some_and(|c| c.can_idle_advance())));
```

**Purpose:** When the virtual clock is active and idle advances are available (or tasks are self-woken), force `busy_poll = true` to prevent `submit_and_wait(1)` from blocking on io_uring indefinitely. Without this, self-wakeups and idle advances would never fire because they don't produce io_uring CQEs.

### Idle advance detection + execution block (lines 419–437)

```rust
#[cfg(feature = "virtual-clock")]
if !task_state.any_ready()                             // :420
    && task_state                                       // :421
        .clock                                          // :422
        .as_ref()                                       // :423
        .is_some_and(|c| c.can_idle_advance())          // :424
{
    let wakers = {                                      // :426
        let clock = task_state.clock.as_mut().unwrap(); // :427
        let (_, wakers) = clock.take_next_idle_advance_or_default().unwrap(); // :428
        wakers                                          // :429
    };                                                  // :430
    let cell = task_state.into_inner();                 // :431
    for w in wakers {                                   // :432
        w.wake();                                       // :433
    }                                                   // :434
    task_state = cell.borrow_mut();                     // :435
    continue;                                           // :436
}                                                       // :437
```

### `into_inner()` / re-borrow pattern (TaskState borrow lifecycle)

The pattern appears in three places in `runtime.rs`:

1. **`poll_task`** (line 37): `task_state.into_inner()` before `task.poll()`, re-borrow at line 58.
2. **`submit_and_complete_io`** (line 184): `task_state.into_inner()` before `waker.wake()`, re-borrow at line 186.
3. **Idle advance block** (line 431): `task_state.into_inner()` before waker loop, re-borrow at line 435.

The pattern is always:
```
let cell = task_state.into_inner();   // Release RefCell borrow
/* do work that may re-borrow TaskState (via wake → schedule_io) */
task_state = cell.borrow_mut();       // Re-acquire borrow
```

This is required because `Waker::wake()` may call back into `TaskState::get()` (thread-local) to schedule I/O or mark tasks ready.

---

## §3 — Public API in operations.rs

**File:** `kimojio/src/operations.rs`

### `virtual_clock_advance_idle` (lines 2123–2162)

```rust
/// Queues a time advance to be applied when the runtime is idle.
///
/// Instead of manually polling a future, advancing, then awaiting, this
/// schedules the advance to happen automatically the next time no tasks are
/// ready to poll. Multiple calls queue in order — each advance waits for its
/// own idle point, allowing newly woken tasks to run between advances.
///
/// # Panics
///
/// Panics if the virtual clock is not enabled.
///
/// # Examples
/// ...
#[cfg(feature = "virtual-clock")]
pub fn virtual_clock_advance_idle(duration: std::time::Duration) {
    TaskState::get()
        .clock
        .as_mut()
        .expect("virtual clock not enabled; call virtual_clock_enable(true) first")
        .queue_idle_advance(duration);
}
```

### `virtual_clock_pending_idle_advances` (lines 2164–2176)

```rust
/// Returns the number of queued idle advances.
///
/// # Panics
///
/// Panics if the virtual clock is not enabled.
#[cfg(feature = "virtual-clock")]
pub fn virtual_clock_pending_idle_advances() -> usize {
    TaskState::get()
        .clock
        .as_ref()
        .expect("virtual clock not enabled; call virtual_clock_enable(true) first")
        .pending_idle_advances()
}
```

### `virtual_clock_advance_idle_default` (lines 2178–2221)

```rust
/// Sets the default idle advance duration.
///
/// When the runtime is idle and no explicit idle advances (from
/// [`virtual_clock_advance_idle`]) are queued, this default is used
/// instead — the clock advances by `duration` on every idle point
/// until the default is cleared.
///
/// Pass `None` (or `Some(Duration::ZERO)`) to clear the default.
///
/// # Panics
///
/// Panics if the virtual clock is not enabled.
///
/// # Examples
/// ...
#[cfg(feature = "virtual-clock")]
pub fn virtual_clock_advance_idle_default(duration: Option<std::time::Duration>) {
    TaskState::get()
        .clock
        .as_mut()
        .expect("virtual clock not enabled; call virtual_clock_enable(true) first")
        .set_idle_advance_default(duration);
}
```

---

## §4 — Existing tests

### Integration tests (`kimojio/tests/virtual_clock_integration.rs`)

| Test | Line | What it tests |
|------|------|---------------|
| `idle_advance_completes_sleep` | 225 | Single queued idle advance completes a 60s sleep |
| `idle_advance_fires_in_order` | 238 | Two queued advances fire in order; verifies clock position after each |
| `idle_advance_wakes_spawned_task` | 262 | Idle advance wakes both main + spawned task sleeping on same duration |
| `pending_idle_advances_count` | 287 | Verifies count goes 0→2→1 as advances are queued then consumed |
| `idle_advance_default_resolves_sleep` | 304 | Default of 1ms resolves a 60s sleep automatically |
| `idle_advance_default_cleared` | 312 | Set default, sleep, clear default, queue explicit advance, sleep |

**Patterns used:**
- Queue advance(s) before `sleep()` — the sleep yields, runtime detects idle, applies advance
- `virtual_clock_advance_idle_default(Some(...))` before sleep — default repeatedly fires
- `virtual_clock_advance_idle_default(None)` to clear, then switch to explicit queuing
- `yield_io().await` after sleep to let spawned tasks (woken by same advance) run

### Unit tests in `operations.rs` (lines 3232–3346)

| Test | Line | What it tests |
|------|------|---------------|
| `idle_advance_completes_sleep` | 3234 | Basic idle advance → sleep completion |
| `idle_advance_60s_completes_fast` | 3242 | Wall-clock < 1s assertion for 60s virtual sleep |
| `multiple_idle_advances_fire_in_sequence` | 3252 | Two advances consumed in order, clock positions verified |
| `idle_advance_spawned_task` | 3274 | Spawned task + main task both sleep, both wake on same advance |
| `idle_advance_default_completes_sleep` | 3299 | Default 1ms resolves 60s sleep, wall-clock < 1s |
| `idle_advance_default_none_is_noop` | 3309 | `None` and `ZERO` don't cause advancement |
| `queue_drains_before_default` | 3322 | Explicit 60s advance consumed first, then 1ms default kicks in |

### Unit tests in `virtual_clock.rs` (lines 607–730)

| Test | Line | What it tests |
|------|------|---------------|
| `queue_idle_advance_enqueues` | 610 | Verifies `pending_idle_advances()` and `has_idle_advances()` |
| `take_next_idle_advance_applies_in_order` | 624 | Two queued advances, first doesn't fire timer, second does |
| `take_next_idle_advance_returns_none_when_empty` | 657 | Empty queue returns `None` |
| `idle_advance_default_none_by_default` | 665 | Default is `None`, `can_idle_advance()` is false |
| `set_idle_advance_default` | 672 | Set, verify, clear, verify |
| `zero_duration_default_treated_as_none` | 685 | `Duration::ZERO` filtered out |
| `take_next_uses_default_when_queue_empty` | 693 | Default persists across calls, advances cumulatively |
| `queue_takes_priority_over_default` | 711 | Queue entry consumed before default fallback |

---

## §5 — Documentation references

**File:** `docs/virtual-clock-guide.md`

### API Reference table (lines 49–61)

The table at lines 49–61 includes three idle-advance rows:

| Line | Entry |
|------|-------|
| 58 | `virtual_clock_advance_idle(duration)` — Queue an advance that fires at the next idle point |
| 59 | `virtual_clock_pending_idle_advances()` — Count of queued idle advances |
| 60 | `virtual_clock_advance_idle_default(Option<Duration>)` — Set/clear default idle advance duration |

**Will need updating:** A new `virtual_clock_set_idle_advance_callback` (or similar) function must be added to this table.

### Idle advance section (lines 260–306)

Prose explanation at lines 260–284: describes queuing pattern, one-advance-per-idle-point semantics, and newly woken tasks completing before next advance.

Default advance prose at lines 286–306: describes `advance_idle_default`, clearing with `None`/`ZERO`, queue priority over default.

**Will need updating:** Add documentation for the new callback API, explaining when it fires and what parameters it receives.

### Caveats section — "Manual advancement only" (lines 237–258)

Lines 257–258 mention a planned "auto-advance mode" and reference `docs/virtual-clock-auto-advance-future.md`. The idle advance callback is a step toward this.

---

## §6 — Borrow pattern analysis

### Current idle advance block (runtime.rs:419–437)

```rust
#[cfg(feature = "virtual-clock")]
if !task_state.any_ready()
    && task_state.clock.as_ref().is_some_and(|c| c.can_idle_advance())
{
    let wakers = {
        let clock = task_state.clock.as_mut().unwrap();
        let (_, wakers) = clock.take_next_idle_advance_or_default().unwrap();
        wakers
    };
    let cell = task_state.into_inner();
    for w in wakers {
        w.wake();
    }
    task_state = cell.borrow_mut();
    continue;
}
```

### Borrow lifecycle (step by step)

1. **Line 420–424:** `task_state` is borrowed (`&self` check via `any_ready()`, then `&self` via `as_ref()` on clock). These are shared references — the mutable borrow from `TaskStateCellRef` is still live.

2. **Line 426–430 (inner block):** `task_state.clock.as_mut().unwrap()` borrows the clock mutably through `task_state`. `take_next_idle_advance_or_default()` pops from the queue and calls `self.advance(duration)` which mutates `self.current` and drains `self.timers`. The returned `wakers: Vec<Waker>` are **moved out** of the clock. When this block ends, the mutable borrow on `clock` is released but `task_state` is still borrowed.

3. **Line 431:** `task_state.into_inner()` **drops the `TaskStateCellRef`**, releasing the `RefCell` borrow entirely. `cell` is the raw `&TaskStateCell`.

4. **Lines 432–434:** Wakers are woken. Each `w.wake()` may re-enter `TaskState::get()` (thread-local `borrow_mut()`) to schedule tasks. This is safe because we released the borrow at step 3.

5. **Line 435:** Re-acquire: `task_state = cell.borrow_mut()`.

### Implications for callback insertion

The callback needs to execute between **step 2** (advance applied, wakers collected) and **step 4** (wakers woken). The callback parameters (`now`, `next_deadline`) can be extracted from the clock in the same `&mut` borrow as step 2.

**Two viable approaches:**

**Option A — Extract callback + params, then invoke after `into_inner()`:**
```rust
let (wakers, callback, now, next_deadline) = {
    let clock = task_state.clock.as_mut().unwrap();
    let (_, wakers) = clock.take_next_idle_advance_or_default().unwrap();
    let cb = clock.idle_advance_callback.take(); // Option::take()
    let now = clock.now();
    let next = clock.next_deadline();
    (wakers, cb, now, next)
};
let cell = task_state.into_inner();
if let Some(cb) = callback {
    cb(now, next_deadline);
    // Put callback back? Or is it one-shot?
}
for w in wakers { w.wake(); }
task_state = cell.borrow_mut();
```

**Option B — Extract params only, invoke callback while clock is borrowed:**

Not viable if the callback needs to call any `TaskState` methods. The `RefCell` borrow is still held.

**Recommended:** Option A with `Option::take()` to extract the callback. If the callback is persistent (not one-shot), put it back after invocation:

```rust
// After invoking:
let clock = task_state.clock.as_mut().unwrap();
clock.idle_advance_callback = Some(cb);
```

Or better: store the callback as `Rc<dyn Fn(...)>` so we can `.clone()` it out without removing it from the struct, avoiding the take/put-back dance:

```rust
let cb = clock.idle_advance_callback.clone(); // Rc clone, cheap
// ... into_inner() ...
if let Some(cb) = cb { cb(now, next_deadline); }
```

**Key constraint:** The callback must NOT call `TaskState::get()` or any `operations::` function that borrows `TaskState`, because we haven't released the borrow yet at invocation time (if invoked before `into_inner()`). If invoked **after** `into_inner()`, this constraint is relaxed — the callback can freely use `TaskState`.

**Safest ordering:** Extract params → `into_inner()` → invoke callback → wake wakers → re-borrow. This allows the callback maximum freedom.
