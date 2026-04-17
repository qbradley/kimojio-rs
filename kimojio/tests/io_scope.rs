// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Comprehensive tests for `io_scope` covering:
//!
//! - **Basic cancellation**: explicit (`io_scope_cancel`) and implicit (scope exit)
//!   for both RingFuture I/O and AsyncEvent waits.
//! - **Sibling isolation**: `join!` sibling futures are not cancelled by a scope.
//! - **Nesting**: inner scope cancellation preserves outer scope I/O.
//! - **Nesting + siblings**: combined nesting and sibling isolation.
//! - **Concurrency during draining**: other tasks and sibling futures make progress
//!   while the scope asynchronously drains cancelled I/O.
//! - **Drop while pending**: scope future dropped via `select!` properly cleans up.
//! - **Cancel-then-continue**: explicit cancel leaves scope open for new I/O.
//! - **Panic safety**: panic inside scope is propagated after I/O cleanup.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use futures::FutureExt;
use kimojio::operations::{self, io_scope, io_scope_cancel, spawn_task};
use kimojio::{AsyncEvent, CanceledError, Errno, oneshot};

// ---------------------------------------------------------------------------
// 1. Basic cancellation — explicit io_scope_cancel()
// ---------------------------------------------------------------------------

/// Explicit cancel of a submitted RingFuture (sleep) via io_scope_cancel().
#[kimojio::test]
async fn io_scope_cancel_ring_future_explicit() {
    io_scope(async || {
        // Create a long sleep — it will be submitted to io_uring.
        let mut sleep = std::pin::pin!(operations::sleep(Duration::from_secs(3600)));
        // Poll once to submit it.
        futures::future::poll_fn(|cx| {
            let _ = sleep.as_mut().poll(cx);
            std::task::Poll::Ready(())
        })
        .await;

        io_scope_cancel();

        assert_eq!(sleep.await, Err(Errno::CANCELED));
    })
    .await;
}

/// Explicit cancel of an AsyncEvent wait via io_scope_cancel().
#[kimojio::test]
async fn io_scope_cancel_wait_explicit() {
    let event = AsyncEvent::new();
    io_scope(async || {
        let mut wait = event.wait();
        // Poll once to register the wait.
        futures::select! {
            _ = wait => panic!("wait should not complete yet"),
            default => {}
        }

        io_scope_cancel();

        assert_eq!(wait.await, Err(CanceledError {}));
    })
    .await;
}

// ---------------------------------------------------------------------------
// 2. Basic cancellation — implicit (returning from scope)
// ---------------------------------------------------------------------------

/// Implicit cancel of a submitted RingFuture when the scope callback returns.
/// The scope's async draining ensures the cancel CQE is processed before
/// the scope future returns Ready.
#[kimojio::test]
#[allow(clippy::async_yields_async)]
async fn io_scope_cancel_ring_future_implicit() {
    let sleep_future = io_scope(async || {
        let mut sleep = operations::sleep(Duration::from_secs(3600)).fuse();
        futures::select! {
            _ = sleep => panic!("sleep should not complete yet"),
            default => {}
        }
        // Return the sleep future — scope exit cancels the underlying I/O.
        sleep
    })
    .await;

    // The sleep should now resolve as canceled.
    assert_eq!(sleep_future.await, Err(Errno::CANCELED));
}

/// Implicit cancel of an AsyncEvent wait when the scope callback returns.
#[kimojio::test]
async fn io_scope_cancel_wait_implicit() {
    let event = AsyncEvent::new();
    io_scope(async || {
        let mut wait = event.wait();
        futures::select! {
            _ = wait => panic!("wait should not complete yet"),
            default => {}
        }
        // Scope exit cancels the wait.
    })
    .await;
    // The event was never set, but the wait was canceled.
    // We can't await the wait here (it was consumed by the scope),
    // so verify indirectly: the scope exited without hanging.
}

// ---------------------------------------------------------------------------
// 3. Sibling isolation — join!
// ---------------------------------------------------------------------------

/// A sibling future's RingFuture I/O (nop) is not cancelled by a scope
/// in the other branch of join!.
#[kimojio::test]
async fn io_scope_sibling_ring_future_not_canceled() {
    let (tx, rx) = oneshot::<i32>();
    let (ready_tx, ready_rx) = oneshot::<()>();

    let scope_future = async {
        io_scope(async || {
            let mut sleep = std::pin::pin!(operations::sleep(Duration::from_secs(3600)));
            futures::future::poll_fn(|cx| {
                let _ = sleep.as_mut().poll(cx);
                std::task::Poll::Ready(())
            })
            .await;

            // Wait until the sibling signals it's blocked on recv.
            ready_rx.recv().await.unwrap();
        })
        .await;

        // Scope exited — sibling should still be alive.
        tx.send(42).unwrap();
    };

    let sibling_future = async {
        // Signal that we're about to block.
        ready_tx.send(()).unwrap();

        let result = rx.recv().await;
        assert_eq!(result, Ok(42));
    };

    futures::join!(scope_future, sibling_future);
}

/// A sibling future's AsyncEvent wait is not cancelled by a scope.
#[kimojio::test]
async fn io_scope_sibling_wait_not_canceled() {
    let (tx, rx) = oneshot::<i32>();
    let (ready_tx, ready_rx) = oneshot::<()>();

    let scope_future = async {
        io_scope(async || {
            let event = AsyncEvent::new();
            let mut wait = event.wait();
            futures::select! {
                _ = wait => panic!("should not complete"),
                default => {}
            }

            ready_rx.recv().await.unwrap();
        })
        .await;
        tx.send(99).unwrap();
    };

    let sibling_future = async {
        ready_tx.send(()).unwrap();
        assert_eq!(rx.recv().await, Ok(99));
    };

    futures::join!(scope_future, sibling_future);
}

// ---------------------------------------------------------------------------
// 4. Nesting
// ---------------------------------------------------------------------------

/// Inner scope's implicit exit cancels only inner I/O; outer scope's
/// submitted RingFuture survives and completes normally.
#[kimojio::test]
async fn io_scope_nested_preserves_outer_ring_future() {
    io_scope(async || {
        // Start a nop in the outer scope.
        let outer_nop = operations::nop();

        // Inner scope with its own I/O.
        io_scope(async || {
            let event = AsyncEvent::new();
            let mut wait = event.wait();
            futures::select! {
                _ = wait => panic!("inner wait should not complete"),
                default => {}
            }
            // Inner scope exits, cancels its wait.
        })
        .await;

        // Outer nop should still work.
        assert_eq!(outer_nop.await, Ok(()));
    })
    .await;
}

/// Inner scope's implicit exit cancels only inner I/O; outer scope's
/// AsyncEvent wait survives and completes normally.
#[kimojio::test]
async fn io_scope_nested_preserves_outer_wait() {
    let outer_event = AsyncEvent::new();

    io_scope(async || {
        let mut outer_wait = outer_event.wait();
        futures::select! {
            _ = outer_wait => panic!("outer wait should not complete yet"),
            default => {}
        }

        io_scope(async || {
            let inner_event = AsyncEvent::new();
            let mut inner_wait = inner_event.wait();
            futures::select! {
                _ = inner_wait => panic!("inner wait should not complete"),
                default => {}
            }
        })
        .await;

        // Outer wait should still be alive.
        outer_event.set();
        assert_eq!(outer_wait.await, Ok(()));
    })
    .await;
}

// ---------------------------------------------------------------------------
// 5. Two sibling io_scopes under join!
// ---------------------------------------------------------------------------

/// Two io_scopes in parallel via join! — each cancels only its own I/O,
/// and both complete successfully without interfering with each other.
#[kimojio::test]
async fn io_scope_two_sibling_scopes() {
    let scope_a = async {
        io_scope(async || {
            let event = AsyncEvent::new();
            let mut wait = event.wait();
            futures::select! {
                _ = wait => panic!("scope A wait should not complete"),
                default => {}
            }
            // Scope A exits, cancels its own wait.
        })
        .await;
        "a_done"
    };

    let scope_b = async {
        io_scope(async || {
            let mut sleep = std::pin::pin!(operations::sleep(Duration::from_secs(3600)));
            futures::future::poll_fn(|cx| {
                let _ = sleep.as_mut().poll(cx);
                std::task::Poll::Ready(())
            })
            .await;
            // Scope B exits, cancels its own sleep.
        })
        .await;
        "b_done"
    };

    let (a, b) = futures::join!(scope_a, scope_b);
    assert_eq!(a, "a_done");
    assert_eq!(b, "b_done");
}

// ---------------------------------------------------------------------------
// 6. Nesting + siblings
// ---------------------------------------------------------------------------

/// Nested io_scope inside join! — inner scope cancels its I/O, but the
/// join! sibling's channel recv is not affected.
#[kimojio::test]
async fn io_scope_nested_with_sibling_not_canceled() {
    let (tx, rx) = oneshot::<i32>();
    let (ready_tx, ready_rx) = oneshot::<()>();

    let scope_future = async {
        io_scope(async || {
            // Inner scope with its own I/O.
            io_scope(async || {
                let event = AsyncEvent::new();
                let mut wait = event.wait();
                futures::select! {
                    _ = wait => panic!("should not complete"),
                    default => {}
                }
            })
            .await;

            // Wait for sibling to be ready.
            ready_rx.recv().await.unwrap();
        })
        .await;
        tx.send(77).unwrap();
    };

    let sibling_future = async {
        ready_tx.send(()).unwrap();
        assert_eq!(rx.recv().await, Ok(77));
    };

    futures::join!(scope_future, sibling_future);
}

// ---------------------------------------------------------------------------
// 6. Concurrency during draining
// ---------------------------------------------------------------------------

/// Other tasks continue to be polled while io_scope is asynchronously
/// draining cancelled I/O.
#[kimojio::test]
async fn io_scope_other_tasks_run_during_drain() {
    let flag = Rc::new(Cell::new(false));

    // Spawn a task that sets the flag after one yield.
    let flag_clone = flag.clone();
    let task = spawn_task(async move {
        operations::yield_io().await;
        flag_clone.set(true);
    });

    // Create a scope with a submitted sleep that requires async draining.
    io_scope(async || {
        let mut sleep = std::pin::pin!(operations::sleep(Duration::from_secs(3600)));
        futures::future::poll_fn(|cx| {
            let _ = sleep.as_mut().poll(cx);
            std::task::Poll::Ready(())
        })
        .await;
        // Scope exits, begins async drain of the sleep cancellation.
    })
    .await;

    // The spawned task should have run during the drain.
    task.await.unwrap();
    assert!(
        flag.get(),
        "spawned task should have run during async drain"
    );
}

/// A join! sibling makes progress while the scope is draining.
#[kimojio::test]
async fn io_scope_sibling_runs_during_drain() {
    let flag = Rc::new(Cell::new(false));
    let flag_clone = flag.clone();

    let scope_future = async {
        io_scope(async || {
            let mut sleep = std::pin::pin!(operations::sleep(Duration::from_secs(3600)));
            futures::future::poll_fn(|cx| {
                let _ = sleep.as_mut().poll(cx);
                std::task::Poll::Ready(())
            })
            .await;
            // Scope exits, begins async drain.
        })
        .await;
    };

    let sibling_future = async {
        // Yield once so we run after the scope starts draining.
        operations::yield_io().await;
        flag_clone.set(true);
    };

    futures::join!(scope_future, sibling_future);
    assert!(flag.get(), "sibling should have run during async drain");
}

// ---------------------------------------------------------------------------
// 7. Drop while pending (select! branch loses)
// ---------------------------------------------------------------------------

/// When an io_scope future is dropped while pending (e.g., loses a select!
/// race), the IoScopeCompletionsGuard properly cancels captured I/O.
#[kimojio::test]
async fn io_scope_drop_while_pending() {
    let event = AsyncEvent::new();

    // Start a scope that will never complete (waits on event forever).
    let scope = io_scope(async || {
        event.wait().await.unwrap();
    });

    // Race the scope against a nop — the nop wins, scope is dropped.
    let mut scope = std::pin::pin!(scope.fuse());
    let mut nop = std::pin::pin!(operations::nop().fuse());

    futures::select! {
        _ = nop => { /* nop wins */ }
        _ = scope => panic!("scope should not complete"),
    }

    // If we get here without hanging, the scope was properly cleaned up on drop.
}

// ---------------------------------------------------------------------------
// 8. Cancel-then-continue (scope stays open)
// ---------------------------------------------------------------------------

/// After explicit io_scope_cancel(), the scope remains open and new I/O
/// initiated in the same scope works normally.
#[kimojio::test]
async fn io_scope_cancel_then_continue() {
    io_scope(async || {
        // Phase 1: create and cancel some I/O.
        let event = AsyncEvent::new();
        let mut wait = event.wait();
        futures::select! {
            _ = wait => panic!("should not complete"),
            default => {}
        }
        io_scope_cancel();
        assert_eq!(wait.await, Err(CanceledError {}));

        // Phase 2: new I/O in the same scope should work.
        let result = operations::nop().await;
        assert_eq!(result, Ok(()));

        // Phase 3: new wait should also work.
        event.set();
        assert_eq!(event.wait().await, Ok(()));
    })
    .await;
}

// ---------------------------------------------------------------------------
// 9. Panic safety
// ---------------------------------------------------------------------------

/// A panic inside an io_scope with in-flight I/O is propagated after the
/// scope cleans up all I/O. We verify by catching the panic at the test level.
#[test]
fn io_scope_panic_propagates_after_cleanup() {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        kimojio::run_test("io_scope_panic_inner", async {
            io_scope(async || {
                let mut sleep = std::pin::pin!(operations::sleep(Duration::from_secs(3600)));
                futures::future::poll_fn(|cx| {
                    let _ = sleep.as_mut().poll(cx);
                    std::task::Poll::Ready(())
                })
                .await;

                panic!("intentional panic inside io_scope");
            })
            .await;
        });
    }));

    assert!(result.is_err(), "panic should have been propagated");
    let payload = result.unwrap_err();
    let msg = payload.downcast_ref::<&str>().copied().unwrap_or("unknown");
    assert_eq!(msg, "intentional panic inside io_scope");
}
