# Runtime cancellation with wrapped wakers

## Scope

This record covers prerequisite priority 1 of the HTTP/1 wrapper optimization program.
The base commit is `05545524df8517f3017c02e06b64fe8bd4c42991`.
The implementation uses the isolated `runtime` worktree.
It makes no throughput or allocation-performance claim.

## Problem and reproduction

The wrapper calls `operations::io_scope_cancel` from `await_io` after a cancellation request.
A scope can contain both native I/O completions and event waits.
Event waits include channel waits and cancellation-token waits.

`FuturesUnordered` gives each child future a wrapped waker.
The child waker queues that child and can wake the parent task.
Before this correction, cancellation invoked that waker while the runtime held a mutable `TaskState` borrow.
The forwarded wake requested the same borrow again.

The smallest regression needs no HTTP connection:

```rust
#[kimojio::test]
async fn cancel_wrapped_wait() {
    use futures::{StreamExt, stream::FuturesUnordered};
    use kimojio::{AsyncEvent, CanceledError, operations};

    let event = AsyncEvent::new();
    operations::io_scope(async || {
        let mut waits = FuturesUnordered::new();
        waits.push(event.wait());
        assert!(futures::poll!(waits.next()).is_pending());
        operations::io_scope_cancel();
        assert_eq!(waits.next().await, Some(Err(CanceledError {})));
    })
    .await;
}
```

The initial poll installs the wrapped waker before cancellation.
The production fix was absent for the first regression run.
The four `futures_unordered_wait` tests produced these results:

| Path | Before the fix | After the fix |
| --- | --- | --- |
| Explicit `io_scope_cancel` | Panic | Pass |
| Normal scope exit with a pending wait | Panic | Pass |
| Drop of a pending scope | Panic | Pass |
| Normal event completion | Pass | Pass |

The three failures reported `TaskStateCell borrowed recursively` at `kimojio/src/task_state_cell.rs:39`.
The baseline command exited with status 101: one test passed and three failed.
The test code remains in `kimojio/src/operations.rs`.

## Root cause

The explicit cancellation path is:

```text
HTTP await_io
  -> operations::io_scope_cancel
  -> Task::cancel_io_scope_completions
  -> task_ref::wake_task
  -> FuturesUnordered child waker
  -> native parent waker
  -> TaskState::get
```

Scope exit and dropped-scope cleanup share `io_scope_cancel_and_wait_internal`.
That function used the same unsafe borrow boundary.

The old `wake_task` accepted `&mut TaskState`.
Its native-waker branch scheduled the task directly and did not request another borrow.
Its fallback called `wake_by_ref` without releasing the existing borrow.
That interface could not release the owning `TaskStateCellRef`.

`TaskStateCell` checks recursive borrowing only in debug builds.
The old release behavior was not evidence of safety.
The recursive borrow also violated the exclusive-reference invariant in release builds.

Normal kernel completion already releases `TaskStateCellRef` before it invokes a waker in `runtime.rs`.
The correction applies that established boundary to scope cancellation.

## Correction

`wake_task` now takes ownership of `TaskStateCellRef` and the `Waker`.
It returns the state guard to its caller.

- For a native waker, it preserves direct task scheduling.
- For a wrapped waker, it releases the guard with `into_inner`.
- It consumes the wrapped waker with `wake` while no `TaskState` borrow exists.
- It borrows the same cell again after the callback returns.

Both cancellation paths mark each wait as canceled before they wake it.
They take the waker from `WaitData` before the callback.
Thus, the callback also runs outside the mutable borrow of the waker slot.
The canceled wait does not need that stored waker again.

The fix adds no dependency, task metadata copy, unsafe block, or runtime allocation.
The native-waker path retains its direct scheduling behavior.
The production HTTP wrapper does not change.
Its test module adds a `FuturesUnordered` composition of existing native-write scenarios.

## Resource settlement and proof scope

The fix changes the wake boundary, not kernel completion ownership.
`Completion::cancel`, the completion pool, and `RingFuture::drop` do not change.
The scope cleanup loop still waits for captured native operations to reach a terminal state.
Explicit cancellation still requires the caller to await the original result.

The regressions cover these boundaries:

| Test | Evidence |
| --- | --- |
| Four `test_io_scope_*_futures_unordered_wait` tests | Explicit cancellation, exit, drop, and normal completion with wrapped event wakers |
| `test_io_scope_futures_unordered_sibling_connections` | Cancellation settles one native socket read while a sibling scoped read remains pending and later succeeds |
| `io_scope_drop_settles_borrowed_storage_with_wrapped_waker` | Dropped-scope cleanup receives the read CQE before it returns |
| `wrapped_wakers_settle_positive_write_completions` | Three independent native-write scenarios run concurrently under wrapped wakers |

The storage test keeps the read future outside the dropped scope.
It inspects the completion state before it polls or drops that future.
The state must be `Completed` with `ECANCELED`, not merely `Submitted` with a cancellation request.
Only afterward does the test release the buffer borrow and reuse that storage.
A subsequent socket exchange succeeds.
This distinguishes scope settlement from settlement performed later by `RingFuture::drop`.

The write test uses the existing `late_write_completion` fixture without transport changes.
It covers a subsequent vector slice, a replacement write after positive partial progress, and a late complete success.
Each scenario also runs an unrelated HTTP connection.
The fixture asserts positive byte counts, cancellation of pending continuations, preserved success, and close after original-operation settlement.

The full wrapper suite also passes its existing resource tests.
These include producer release during an in-flight payload, dropped requests, dropped body leases, partial-progress errors, and explicit close.

## Commands and results

All build and test commands used CPUs 8 through 31.
No profiling or timing experiment used CPUs 0 through 7.

Run the commands from the isolated worktree:

```sh
cd /workspace/kimojio-rs/target/wrapper-lab/worktrees/runtime
export CARGO_TARGET_DIR=/workspace/kimojio-rs/target/wrapper-lab/build-runtime

# Before the production fix: 1 passed, 3 failed, exit 101.
taskset -c 8-31 cargo test -p kimojio --lib futures_unordered_wait -- --nocapture

# After the production fix: 12 passed with each feature selection.
taskset -c 8-31 cargo test -p kimojio --lib io_scope
taskset -c 8-31 timeout 120s cargo test -p kimojio --lib --all-features io_scope

# Five focused I/O tests passed.
taskset -c 8-31 cargo test -p kimojio-http1 --lib io::tests

# 41 tests passed: 8 library, 17 connection, 10 settlement, 6 benchmark-client.
taskset -c 8-31 timeout 120s cargo test -p kimojio-http1 --all-targets --features virtual-clock

taskset -c 8-31 cargo fmt
taskset -c 8-31 cargo clippy
taskset -c 8-31 cargo clippy --all-targets --all-features
```

Both Clippy commands exited successfully.
Default Clippy reported the existing `question_mark` warning at `examples/http1-static/src/app.rs:413`.
All-target, all-feature Clippy also reported existing `byte_char_slices` warnings at `kimojio/src/pipe.rs:64` and `:65`.
The changed code introduced no warnings.

## Remaining limits

- The tests require Linux and available native io_uring support.
- The proof covers local runtime tasks and `FuturesUnordered`, not cross-thread waker use.
- No Miri, sanitizer, or exhaustive kernel-race proof accompanies this change.
- Dropped-scope cleanup remains synchronous and can stall the runtime thread until native I/O settles.
- Cancellation remains a request, not a rollback of bytes that the kernel already accepted.
- A positive partial write can submit another operation. The wrapper must continue cancellation on each pending poll.
- Custom transports must still cooperate with native cancellation.
- The server example retains bounded native tasks. This fix does not replace its connection scheduler.
- The full workspace test suite was not necessary for this localized correction. Both workspace Clippy configurations did run.
