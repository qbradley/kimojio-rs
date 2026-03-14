---
date: 2026-03-14T00:00:00Z
git_commit: 4de6e2a
branch: feature/virtual-clock-deterministic-timing
repository: kimojio-rs
topic: "Virtual Clock Deterministic Timing — Codebase Research"
tags: [research, codebase, runtime, operations, task-state, clock, sleep, deadline]
status: complete
last_updated: 2026-03-14
---

# Research: Virtual Clock Deterministic Timing

## Research Question

What are the exact implementation details, integration points, and architectural constraints in the kimojio codebase that must be understood to add a virtual clock feature for deterministic timing in tests?

## Summary

Kimojio is a single-threaded, thread-per-core io_uring async runtime. All timer operations flow through `operations::sleep()` which creates an io_uring `Timeout` SQE. Deadline-based I/O functions (`read_with_deadline`, `write_with_deadline`, `writev_with_deadline`) compute remaining duration from `Instant::now()` and pass it as a `LinkTimeout` SQE. The runtime state is stored in a thread-local `TaskState` accessed via `TaskState::get()` through a custom `TaskStateCell` (an optimized `RefCell`). The event loop in `Runtime::block_on()` alternates between submitting/completing I/O and polling ready tasks. There are 6 production `Instant::now()` call sites that are candidates for virtualization (3 in `operations.rs`, 2 in `async_event.rs`, 1 in `async_lock.rs`), plus 10 additional sites in test code and unrelated infrastructure that do not require virtualization.

## Documentation System

- **Framework**: Plain markdown (no mkdocs, docusaurus, or sphinx)
- **Docs Directory**: N/A — no `docs/` directory exists
- **Navigation Config**: N/A
- **Style Conventions**: Rust doc comments (`///` and `//!`) on public API items; README.md at repo root with getting-started guide
- **Build Command**: N/A (no separate documentation build)
- **Standard Files**: `README.md` (repo root), `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `SECURITY.md`, `LICENSE.txt`

## Verification Commands

- **Test Command**: `cargo test` (uses `#[crate::test]` macro for async tests, standard `#[test]` for sync)
- **Lint Command**: `cargo clippy` and `cargo clippy --all-targets --all-features`
- **Build Command**: `cargo build` / `cargo check`
- **Type Check**: `cargo check`
- **Format**: `cargo fmt`

## Detailed Findings

### 1. Runtime Struct and Constructors

The `Runtime` struct is minimal (`kimojio/src/runtime.rs:297-301`):

```rust
pub struct Runtime {
    busy_poll: BusyPoll,
    server_pipe: OwnedFd,
    client_pipe: OwnedFd,
}
```

**Constructor** `Runtime::new()` (`kimojio/src/runtime.rs:304-318`):
- Signature: `pub fn new(thread_index: u8, configuration: Configuration) -> Self`
- Destructures `Configuration` into `trace_buffer_manager` and `busy_poll`
- Accesses `TaskState::get()` to set thread index and trace config on the thread-local
- Creates a bidirectional pipe via `crate::pipe::bipipe()`
- Stores only `busy_poll`, `server_pipe`, `client_pipe` — no clock field

**`block_on()`** (`kimojio/src/runtime.rs:328-411`):
- Signature: `pub fn block_on<Fut>(&mut self, main: Fut) -> Option<Result<Fut::Output, Box<dyn Any + Send + 'static>>>`
- Gets `TaskState::get()`, schedules the main future as a task
- Main event loop (`runtime.rs:376-396`): `while task_state.get_task_count() > 0 && task_state.keep_running`
  - Calls `submit_and_complete_io_all(task_state, busy_poll)`
  - Calls `task_state.prepare_cohort()`
  - Inner loop: `while let Some(task) = task_state.get_ready()` → `poll_task(task, task_state)`
- After loop: drops all tasks, returns `task.result()`

### 2. TaskState Thread-Local

**Thread-local definition** (`kimojio/src/task.rs:524-525`):

```rust
thread_local! {
    static TASK_STATE: TaskStateCell = TaskStateCell::new(TaskState::default());
}
```

**`TaskState::get()`** (`kimojio/src/task.rs:563-570`):

```rust
pub fn get() -> TaskStateCellRef<'static> {
    let task_state = TASK_STATE.with(|task_state| task_state as *const TaskStateCell);
    let task_state = unsafe { &*task_state };
    task_state.borrow_mut()
}
```

Returns a `TaskStateCellRef<'static>` — a mutable borrow guard.

**`TaskState` struct fields** (`kimojio/src/task.rs:457-522`):

| Field | Type | Purpose |
|-------|------|---------|
| `task_count` | `usize` | Number of live tasks |
| `last_ready_cpu` | `bool` | Scheduling heuristic |
| `thread_id` | `ThreadId` | Owning thread |
| `ring` | `Ring` | io_uring instance (non-polled) |
| `ring_poll` | `Ring` | io_uring instance (IOPOLL) |
| `trace_buffer` | `TraceBuffer` | Tracing infrastructure |
| `stats` | `URingStats` | I/O statistics |
| `enter_stats` / `enter_stats_wait` | `EnterStats` | Kernel enter timing |
| `current_task` | `Option<Rc<Task>>` | Currently running task |
| `fill_ready_task_list_io` | `VecDeque<Rc<Task>>` | New ready tasks (IO priority) |
| `ready_task_list_io` | `VecDeque<Rc<Task>>` | Current cohort (IO priority) |
| `ready_task_list_cpu` | `VecDeque<Rc<Task>>` | CPU-priority ready tasks |
| `completion_pool` | `VecDeque<Rc<Completion>>` | Recycled completions |
| `tasks` | `HandleTable<Rc<Task>>` | All tasks |
| `completed_tasks` | `VecDeque<Rc<Task>>` | Tasks awaiting drop |
| `next_tag` | `u32` | Tracing tag counter |
| `probe` | `rustix_uring::Probe` | Kernel capability probe |
| `keep_running` | `bool` | Event loop control flag |
| `fault` | `Option<(usize, Errno)>` | (feature: fault_injection) |

No clock-related field exists. A new field for the clock reference would be added here.

**`TaskStateCell`** (`kimojio/src/task_state_cell.rs:20-82`):
- Custom `RefCell`-like wrapper around `UnsafeCell<TaskState>`
- Debug-only borrow checking (release mode skips it for performance)
- `borrow_mut()` returns `TaskStateCellRef<'_>` which `Deref`/`DerefMut` to `TaskState`
- `into_inner()` releases the borrow without dropping the reference — critical pattern for re-entrant access from task polling

### 3. operations::sleep()

**Implementation** (`kimojio/src/operations.rs:1066-1079`):

```rust
pub fn sleep(duration: Duration) -> SleepFuture<'static> {
    let timespec = Box::new(Timespec::from(duration));
    let entry = opcode::Timeout::new(timespec.as_ref()).build();
    let fut = UnitFuture::with_polled(
        entry,
        -1,
        None,
        IOType::Timeout,
        false,
        CompletionResources::Box(timespec),
    );
    SleepFuture { fut }
}
```

- Creates a boxed `Timespec` from the `Duration`
- Builds an io_uring `Timeout` SQE pointing to the timespec
- Wraps in `UnitFuture::with_polled()` with `CompletionResources::Box(timespec)` to own the timespec memory
- The `Timespec` lives as long as the `Completion` (cancel-safe)

**`SleepFuture` type** (`kimojio/src/operations.rs:1082-1114`):

```rust
pin_project_lite::pin_project! {
    pub struct SleepFuture<'a> {
        #[pin]
        fut: UnitFuture<'a>,
    }
}
```

- Thin wrapper around `UnitFuture<'a>`
- `Future` impl maps `Err(Errno::TIME)` → `Ok(())` (timeout is the success case for sleep)
- `cancel()` method: `self.project().fut.cancel()` (`operations.rs:1090-1092`)
- `FusedFuture` impl delegates to `self.fut.is_terminated()` (`operations.rs:1110-1114`)

### 4. Deadline I/O Functions

Three `*_with_deadline` functions exist, all following the same pattern:

**`writev_with_deadline`** (`kimojio/src/operations.rs:560-581`):
- Signature: `pub fn writev_with_deadline<'a>(fd, iovec, offset, deadline: Option<Instant>) -> ErrnoOrFuture<UsizeFuture<'a>>`
- **`Instant::now()` at line 567**: `deadline.checked_duration_since(Instant::now())`
- If past deadline → returns `ErrnoOrFuture::Error { errno: Errno::TIMEDOUT }`
- Otherwise → delegates to `writev_with_timeout()`

**`write_with_deadline`** (`kimojio/src/operations.rs:624-644`):
- Signature: `pub fn write_with_deadline<'a>(fd, buf, deadline: Option<Instant>) -> ErrnoOrFuture<UsizeFuture<'a>>`
- **`Instant::now()` at line 630**: same pattern

**`read_with_deadline`** (`kimojio/src/operations.rs:839-858`):
- Signature: `pub fn read_with_deadline<'a>(fd, buf, deadline: Option<Instant>) -> ErrnoOrFuture<UsizeFuture<'a>>`
- **`Instant::now()` at line 845**: same pattern

**Common pattern** (all three):
```rust
let timeout = if let Some(deadline) = deadline {
    if let Some(duration) = deadline.checked_duration_since(Instant::now()) {
        Some(duration)
    } else {
        return ErrnoOrFuture::Error { errno: Errno::TIMEDOUT };
    }
} else {
    None
};
```

**`ErrnoOrFuture` type** (`kimojio/src/operations.rs:1572-1607`):
```rust
pin_project_lite::pin_project! {
    #[project = ErrnoOrFutureProj]
    pub enum ErrnoOrFuture<Fut> {
        Error { errno: Errno },
        Future { #[pin] fut: Fut },
    }
}
```
- Implements `Future`, `FusedFuture`, and `IsIoPoll`

### 5. async_event.rs wait_with_deadline

**`wait_with_deadline`** (`kimojio/src/async_event.rs:146-176`):

```rust
pub async fn wait_with_deadline(&self, deadline: Option<Instant>) -> Result<(), TimeoutError> {
    if let Some(deadline) = deadline {
        let wait_remaining = || match (
            self.state.get(),
            deadline.checked_duration_since(Instant::now()),  // line 150
        ) {
            (true, _) => None,
            (false, Some(remaining)) => Some(remaining),
            (false, None) => None,
        };

        while let Some(remaining) = wait_remaining() {
            futures::select! {
                result = self.wait() => ...,
                result = operations::sleep(remaining) => ...  // line 162
            }
        }
        ...
    }
}
```

- **`Instant::now()` at line 150**: Called in the `wait_remaining` closure on every loop iteration
- The closure computes remaining duration until deadline
- If state is already set or deadline passed → returns `None` (exit loop)
- Otherwise → races `self.wait()` against `operations::sleep(remaining)` via `futures::select!`

**`wait_with_timeout`** (`kimojio/src/async_event.rs:186-188`):
- **`Instant::now()` at line 187**: `let deadline = timeout.map(|timeout| Instant::now() + timeout);`
- Converts timeout to deadline, then delegates to `wait_with_deadline`

### 6. async_lock.rs Instant::now() Usage

**`lock_with_timeout`** (`kimojio/src/async_lock.rs:80-86`):
```rust
pub async fn lock_with_timeout(&self, timeout: Option<Duration>) -> Result<AsyncLockRef<'_, T>, TimeoutError> {
    let deadline = timeout.map(|timeout| Instant::now() + timeout);  // line 84
    self.lock_with_deadline(deadline).await
}
```

- **`Instant::now()` at line 84**: Converts timeout → deadline, delegates to `lock_with_deadline`
- `lock_with_deadline` (`async_lock.rs:64-75`) itself takes `Option<Instant>` and delegates to `AsyncEvent::wait_with_deadline` — no direct `Instant::now()` call

### 7. submit_and_complete_io_all

**Implementation** (`kimojio/src/runtime.rs:197-224`):

```rust
pub(crate) fn submit_and_complete_io_all(
    mut task_state: TaskStateCellRef<'_>,
    busy_poll: bool,
) -> TaskStateCellRef<'_> {
    task_state = submit_and_complete_io(task_state, busy_poll, true);   // IOPOLL ring
    task_state = submit_and_complete_io(task_state, busy_poll, false);  // normal ring

    // Drop completed tasks with task_state released
    if !task_state.completed_tasks.is_empty() {
        let mut completed_tasks = std::mem::take(&mut task_state.completed_tasks);
        let task_state_cell = task_state.into_inner();
        completed_tasks.clear();
        task_state = task_state_cell.borrow_mut();
        task_state.completed_tasks = completed_tasks;
    }
    task_state
}
```

- Calls `submit_and_complete_io` twice: once for IOPOLL, once for normal ring
- Handles completed task drops with careful borrow management

**`submit_and_complete_io`** (`kimojio/src/runtime.rs:233-295`):
- Determines whether to block: blocks only when no ready tasks AND not busy-polling AND not IOPOLL
- Calls `ring.submit_and_wait(want)` — `want=0` (non-blocking) or `want=1` (blocking)
- Then calls `process_completions()` to drain CQEs and wake tasks

**`process_completions`** (`kimojio/src/runtime.rs:87-195`):
- Iterates CQEs, reconstitutes `Rc<Completion>` from user_data pointer
- Transitions `CompletionState::Submitted → Completed`
- Wakes the associated task's waker (with task_state released)

### 8. Event Loop in block_on

**Main loop** (`kimojio/src/runtime.rs:376-396`):

```rust
while task_state.get_task_count() > 0 && task_state.keep_running {
    let busy_poll = match self.busy_poll { ... };
    task_state = submit_and_complete_io_all(task_state, busy_poll);
    task_state.prepare_cohort();
    while let Some(task) = task_state.get_ready() {
        task_state = poll_task(task, task_state);
        // reset busy_poll cooldown on task poll
    }
}
```

- `prepare_cohort()` (`task.rs:782-792`): swaps `fill_ready_task_list_io` into `ready_task_list_io` as a batch
- `get_ready()` (`task.rs:794-821`): pops from IO queue first, then CPU queue (with alternation to prevent starvation)
- `poll_task()` (`runtime.rs:19-76`): releases TaskState borrow, polls the future, re-borrows

### 9. lib.rs Public API

**Module declarations** (`kimojio/src/lib.rs:18-53`):
- Public modules: `configuration`, `io_type`, `operations`, `pipe`, `socket_helpers`, `task`, `task_pool`, `timer`
- Crate-internal: `ring_future` (`pub(crate)`)
- Feature-gated: `ssl2`, `tlscontext`, `tlsstream` (behind `tls` / `ssl2` features)

**Key public re-exports** (`kimojio/src/lib.rs:55-99`):
- `AsyncEvent`, `AsyncLock`, `AsyncLockRef`, `AsyncSemaphore`, `AsyncReaderWriterLock`
- `CancellationToken`, `Runtime`, `RuntimeHandle`
- Channel types: `Receiver`, `Sender`, `async_channel`, etc.
- Stream types: `AsyncStreamRead`, `AsyncStreamWrite`, `OwnedFdStream`, etc.
- Error types: `CanceledError`, `TimeoutError`, `ChannelError`, `TaskHandleError`
- Macro re-exports: `pub use kimojio_macros::{main, test}` (`lib.rs:99`)

**Top-level functions** (`kimojio/src/lib.rs:190-290`):
- `run(thread_index, main)` → calls `run_with_configuration` with `Configuration::new()`
- `run_test(test_name, main)` → for tests
- `run_with_configuration(thread_index, main, configuration)` → creates `Runtime::new()`, calls `block_on()`
- `run_test_with_handle(test_name, main, trace, busy_poll)` → creates runtime with handle, used by `#[kimojio::test]`

**Internal types declared in lib.rs** (`lib.rs:103-145`):
- `CompletionState` enum: `Idle`, `Submitted`, `Completed`, `Terminated`
- `Completion` struct: `state`, `owned_resources`, `timespec`, `tag`, `task_index`, `iopoll`
- `CompletionResources` enum: `None`, `Timespec(Timespec)`, `Box(Box<dyn Any>)`, `Rc(Rc<dyn Any>)`, `InlineBuffer([u8; 8])`

### 10. RingFuture and UnitFuture

**Type aliases** (`kimojio/src/ring_future.rs:41-49`):
```rust
pub type UnitFuture<'a> = RingFuture<'a, (), ResultToUnit>;
pub type UsizeFuture<'a> = RingFuture<'a, usize, ResultToUsize>;
pub type OwnedFdFuture<'a> = RingFuture<'a, OwnedFd, ResultToOwnedFd>;
```

**`RingFuture` struct** (`ring_future.rs:58-61`):
```rust
pub struct RingFuture<'a, T: Unpin, C: MakeResult<T>> {
    handle: Option<Rc<Completion>>,
    _marker: PhantomData<(&'a (), T, C)>,
}
```

**`with_polled()` constructor** (`ring_future.rs:80-128`):
- Gets `TaskState::get()`, extracts `current_task`, gets next tag
- Converts `timeout: Option<Duration>` into `Timespec`
- Creates `Completion` with `CompletionState::Idle { entry, timespec: bool }`
- Registers with `task.register_io(&handle)`, writes IoStart trace event

**`Future::poll` impl** (`ring_future.rs:148-284`):
- `Idle` state → submits SQE to ring (with `LinkTimeout` if timespec present), transitions to `Submitted`
- `Submitted` state → updates waker, returns `Pending`
- `Completed` state → extracts result, transitions to `Terminated`, returns `Ready`

**`Drop` impl** (`ring_future.rs:287-351`):
- Cancels pending I/O via `completion.cancel()`
- If borrowed resources, blocks in `submit_and_complete_io` loop until I/O completes

### 11. Existing Test Patterns

Tests use the `#[crate::test]` attribute macro (re-exported as `kimojio::test` from `kimojio-macros`):

```rust
#[crate::test]
async fn test_name() {
    // async test body — runs inside a kimojio runtime
}
```

- Tests are `async fn` with no parameters
- Module structure: `#[cfg(test)] mod test { ... }`
- Common imports: `use crate::{AsyncEvent, Errno, operations}; use std::rc::Rc;`
- Tests use real I/O: `operations::sleep()`, `operations::read/write()`, `crate::pipe::bipipe()`
- Task spawning: `operations::spawn_task(async { ... })`
- Synchronization: `Rc<AsyncEvent>` for signaling between tasks
- Time: `std::time::Duration::from_millis(...)` / `from_secs(...)` for sleeps/timeouts

**Example test files with `#[crate::test]`**:
- `operations.rs`: ~20+ tests (line 1705, 1762, 1773, 1795, etc.)
- `runtime.rs`: 3 tests (lines 420, 465, 528)
- `async_event.rs`: 6 tests (lines 599, 666, 682, 711, 727, 768)
- `async_lock.rs`: 3 tests (lines 157, 180, 210)
- `ring_future.rs`: 5 tests (lines 405, 423, 441, 488, 500)

### 12. Cargo.toml Features

**Current feature flags** (`kimojio/Cargo.toml:34-51`):

| Feature | Description |
|---------|-------------|
| `default = ["tls"]` | TLS enabled by default |
| `tls` | OpenSSL support via `kimojio-tls` |
| `setup_single_issuer` | io_uring optimization (Linux 6.0+) |
| `io_uring_cmd` | NVMe pass-through (128-byte SQE/32-byte CQE) |
| `noencrypt` | (undocumented) |
| `enter_stats` | Kernel enter timing stats |
| `fault_injection` | Test fault injection |
| `ssl2` | Extended TLS |

No `virtual-clock` feature exists yet.

**Key dependencies**: `futures`, `pin-project-lite`, `rustix`, `rustix-uring`, `libc`, `uuid`, `scopeguard`, `intrusive-collections`, `impls`, `static_assertions`

**Dev dependencies**: `criterion`, `rcgen`, `tempfile`

### 13. All Instant::now() Usage Sites

#### Production Code (in `kimojio/src/`)

| # | File:Line | Context | Classification |
|---|-----------|---------|----------------|
| 1 | `operations.rs:567` | `writev_with_deadline`: `deadline.checked_duration_since(Instant::now())` | Deadline computation |
| 2 | `operations.rs:630` | `write_with_deadline`: `deadline.checked_duration_since(Instant::now())` | Deadline computation |
| 3 | `operations.rs:845` | `read_with_deadline`: `deadline.checked_duration_since(Instant::now())` | Deadline computation |
| 4 | `async_event.rs:150` | `wait_with_deadline`: `deadline.checked_duration_since(Instant::now())` | Deadline computation |
| 5 | `async_event.rs:187` | `wait_with_timeout`: `Instant::now() + timeout` | Timeout → deadline conversion |
| 6 | `async_lock.rs:84` | `lock_with_timeout`: `Instant::now() + timeout` | Timeout → deadline conversion |

#### Test-Only Code

| # | File:Line | Context | Classification |
|---|-----------|---------|----------------|
| 7 | `operations.rs:1950` | `starvation_test`: `Instant::now() + Duration::from_secs(10)` | Test timeout guard |
| 8 | `operations.rs:1963` | `starvation_test`: `Instant::now() < terminate_time` | Test timeout check |
| 9 | `operations.rs:1981` | `starvation_test`: `Instant::now() < terminate_time` | Test timeout check |
| 10 | `operations.rs:2378` | file ops test: `Instant::now() + Duration::from_secs(120)` | Test deadline |

#### Other Files (not directly relevant to virtual clock)

| # | File:Line | Context | Classification |
|---|-----------|---------|----------------|
| 11 | `async_channel.rs:658` | test: `Instant::now() + Duration::from_millis(10)` | Test deadline |
| 12 | `async_channel.rs:664` | test: `Instant::now() + Duration::from_millis(100)` | Test deadline |
| 13 | `async_oneshot.rs:190,205,230,345,356` | tests: deadline construction | Test deadline |
| 14 | `tlscontext.rs:158` | test: `Instant::now() - Duration::from_secs(1)` | TLS certificate test |
| 15 | `timer.rs:41` | `Timer::ticks()` fallback for non-x86/aarch64 | TSC calibration fallback |
| 16 | `timer.rs:53,71` | `compute_ticks_per_us()` calibration | TSC calibration |

**Virtualization scope**: Sites #1-6 are production code that must use `clock.now()` instead of `Instant::now()`. Sites #7-16 are in test code or unrelated infrastructure (timer.rs TSC calibration).

### 14. CompletionResources Enum

**Definition** (`kimojio/src/lib.rs:138-145`):

```rust
#[allow(dead_code)]
enum CompletionResources {
    None,
    Timespec(rustix_uring::types::Timespec),
    Box(Box<dyn std::any::Any>),
    Rc(Rc<dyn std::any::Any>),
    InlineBuffer([u8; 8]),
}
```

Used by `sleep()` as `CompletionResources::Box(timespec)` to own the `Timespec` memory for the io_uring SQE. The `Timespec` variant exists but `sleep()` uses the `Box` variant instead (the boxed `Timespec` is stored as `Box<dyn Any>`).

### 15. Configuration Struct

**Definition** (`kimojio/src/configuration.rs:13-26`):

```rust
#[derive(Default)]
pub struct Configuration {
    pub(crate) trace_buffer_manager: Option<Box<dyn crate::TraceConfiguration>>,
    pub(crate) busy_poll: BusyPoll,
}
```

**Builder methods** (`configuration.rs:53-73`):
- `Configuration::new()` → `Self::default()`
- `set_trace_buffer_manager(self, Box<dyn TraceConfiguration>) -> Self`
- `set_busy_poll(self, BusyPoll) -> Self`

**`BusyPoll` enum** (`configuration.rs:32-42`): `Never` (default), `Always`, `Until(Duration)`

**Usage in Runtime** (`runtime.rs:304-308`): `Runtime::new()` destructures `Configuration` into its fields:
```rust
let Configuration { trace_buffer_manager, busy_poll } = configuration;
```

A `set_clock()` builder method and corresponding field would follow the existing pattern.

### 16. Timespec Type

**Origin**: `rustix_uring::types::Timespec` — imported at `kimojio/src/operations.rs:68`

**Usage in sleep()** (`operations.rs:1069`):
```rust
let timespec = Box::new(Timespec::from(duration));
```

`Timespec` is a kernel-compatible time specification used by io_uring timeout SQEs. It has `.nsec()` and `.sec()` builder methods (used in `ring_future.rs:94-96`) and `From<Duration>`.

**Also used**: In `Completion` struct (`lib.rs:132`): `timespec: rustix_uring::types::Timespec` — stores the timespec for `LinkTimeout` SQEs directly in the completion.

### 17. Documentation Infrastructure

- **No `docs/` directory** — confirmed via filesystem check
- **No mdbook/docusaurus/sphinx** configuration files
- **README.md** at repo root: Contains project description, prerequisites (Linux 5.15+), installation (`cargo add kimojio`), "Hello World" example, contributing link, license
- **Examples directory** (`examples/`): Contains `copyfile` and `tls-echo-server` subdirectories
- **Rust doc comments**: Comprehensive `///` and `//!` comments on public API items throughout the codebase
- **Standard files**: `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `SECURITY.md`, `LICENSE.txt` at repo root

## Code References

### Critical Integration Points for Virtual Clock

- `kimojio/src/lib.rs:103-145` — `CompletionState`, `Completion`, `CompletionResources` definitions
- `kimojio/src/lib.rs:190-198` — `run()` function, entry point
- `kimojio/src/lib.rs:272-290` — `run_with_configuration()`, creates Runtime + calls block_on
- `kimojio/src/runtime.rs:297-318` — `Runtime` struct and `new()` constructor
- `kimojio/src/runtime.rs:328-411` — `block_on()` event loop
- `kimojio/src/runtime.rs:197-224` — `submit_and_complete_io_all()`
- `kimojio/src/task.rs:457-522` — `TaskState` struct definition
- `kimojio/src/task.rs:524-525` — `TASK_STATE` thread-local
- `kimojio/src/task.rs:563-570` — `TaskState::get()`
- `kimojio/src/task_state_cell.rs:20-82` — `TaskStateCell` and `TaskStateCellRef`
- `kimojio/src/configuration.rs:13-73` — `Configuration` struct and builder
- `kimojio/src/operations.rs:1066-1114` — `sleep()` and `SleepFuture`
- `kimojio/src/operations.rs:560-581` — `writev_with_deadline()` (Instant::now at line 567)
- `kimojio/src/operations.rs:624-644` — `write_with_deadline()` (Instant::now at line 630)
- `kimojio/src/operations.rs:839-858` — `read_with_deadline()` (Instant::now at line 845)
- `kimojio/src/async_event.rs:146-176` — `wait_with_deadline()` (Instant::now at line 150)
- `kimojio/src/async_event.rs:186-188` — `wait_with_timeout()` (Instant::now at line 187)
- `kimojio/src/async_lock.rs:80-86` — `lock_with_timeout()` (Instant::now at line 84)
- `kimojio/src/ring_future.rs:58-128` — `RingFuture` struct and constructors
- `kimojio/src/ring_future.rs:148-284` — `RingFuture::poll()` — SQE submission path
- `kimojio/Cargo.toml:34-51` — Feature flags

## Architecture Documentation

### TaskState Borrow Pattern

The `TaskStateCell` borrow pattern is the most critical architectural constraint. The runtime carefully manages when `TaskStateCellRef` borrows are held:

1. `TaskState::get()` acquires a mutable borrow (`task.rs:563-570`)
2. `poll_task()` releases the borrow before polling a task's future (`runtime.rs:37` — `task_state.into_inner()`)
3. After poll returns, the borrow is re-acquired (`runtime.rs:55-58`)
4. Task code (futures being polled) can call `TaskState::get()` because the borrow was released

This means any code called during `Future::poll` (including `operations::sleep()` via `RingFuture::with_polled()`) can safely call `TaskState::get()`. A clock stored in `TaskState` would be accessible from `sleep()` and deadline functions through this same mechanism.

### SQE Submission Flow

1. `operations::sleep(duration)` creates a `Timespec` and `opcode::Timeout` SQE
2. `UnitFuture::with_polled()` creates a `Completion` in `Idle` state
3. On first `poll()`, the SQE is submitted to the io_uring ring (`ring_future.rs:179-230`)
4. If a timeout is specified for I/O operations, a `LinkTimeout` SQE is chained
5. When the CQE arrives, `process_completions()` transitions to `Completed` and wakes the task
6. Next poll sees `Completed`, returns `Ready`

### Deadline-to-Duration Conversion Pattern

All deadline functions follow identical logic:
```
deadline: Option<Instant> → compute remaining = deadline - Instant::now() → pass Duration to _with_timeout variant
```

The `_with_timeout` variants create the actual io_uring SQEs. The virtual clock intercept point is at the `Instant::now()` call in each `_with_deadline` function.

### Task Scheduling

- Tasks are scheduled into `fill_ready_task_list_io` or `ready_task_list_io` (I/O priority) or `ready_task_list_cpu` (CPU priority)
- `prepare_cohort()` batches fill→ready swap
- `get_ready()` drains IO queue first, alternates with CPU queue

## Key Findings

1. **Clock storage location**: `TaskState` is the natural home for a clock reference. It has no clock field currently. Adding one follows the existing pattern of thread-local state. `TaskState::get()` is called from `sleep()`, `RingFuture::with_polled()`, and other operation functions, confirming it is accessible at all interception points.

2. **Six production `Instant::now()` sites** need virtualization: three in `operations.rs` deadline functions, two in `async_event.rs`, one in `async_lock.rs`. All follow two patterns: (a) `deadline.checked_duration_since(Instant::now())` for remaining time computation, (b) `Instant::now() + timeout` for timeout→deadline conversion.

3. **`sleep()` interception point**: The function directly creates an io_uring `Timeout` SQE. For virtual clock, the entire SQE creation path would be bypassed — instead, a timer would be registered in the virtual timer wheel. The `SleepFuture` type wraps `UnitFuture` (which is `RingFuture<(), ResultToUnit>`) — a virtual variant would need a different underlying future type.

4. **`Configuration` builder pattern**: Adding `set_clock()` follows the established `set_busy_poll()` / `set_trace_buffer_manager()` pattern. `Runtime::new()` destructures configuration, so the clock would flow: `Configuration` → `Runtime::new()` → `TaskState` thread-local.

5. **Borrow safety**: The `TaskStateCell` borrow release pattern (`into_inner()` before polling, re-borrow after) ensures that task code can access `TaskState`. Clock access from `sleep()` and deadline functions goes through `TaskState::get()` which is safe during task execution.

6. **Feature flag precedent**: The codebase already uses several feature flags (`tls`, `fault_injection`, `io_uring_cmd`, `enter_stats`). Adding `virtual-clock` follows established patterns, using `#[cfg(feature = "virtual-clock")]` gating.

7. **Test macro**: Tests use `#[crate::test]` (resolves to `#[kimojio::test]`). Virtual clock tests would use the same macro or potentially `run_test_with_handle` for direct runtime access with a clock parameter.

8. **No documentation infrastructure**: There is no docs framework, build system, or navigation config. Documentation for the virtual clock feature would be inline Rust doc comments plus potentially new markdown files if specified by the implementation plan.

## Open Questions

None — all research questions have been answered with file:line references. The codebase structure is well-understood for implementation planning.
