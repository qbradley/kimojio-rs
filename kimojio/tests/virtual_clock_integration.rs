// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for the virtual clock feature.
//!
//! These tests exercise the full stack: virtual clock → runtime → operations,
//! verifying that sleep, sleep_until, timeout_at, and drop cancellation all
//! work correctly with virtual time advancement.

#![cfg(feature = "virtual-clock")]

use std::future::Future;
use std::pin::pin;
use std::task::Poll;
use std::time::Duration;

use kimojio::Runtime;
use kimojio::TimeoutError;
use kimojio::clock::VirtualClock;
use kimojio::configuration::Configuration;
use kimojio::operations;

/// Helper: run a test with a virtual clock runtime.
fn run_virtual<F, Fut>(test: F)
where
    F: FnOnce(VirtualClock) -> Fut,
    Fut: Future<Output = ()> + 'static,
{
    let (mut runtime, clock) = Runtime::new_virtual(0, Configuration::new());
    let clock_for_test = clock.clone();
    let result = runtime.block_on(test(clock_for_test));
    if let Some(Err(payload)) = result {
        std::panic::resume_unwind(payload);
    }
    drop(clock);
}

// ── P2: sleep_until with virtual time ────────────────────────────────

#[test]
fn sleep_until_completes_after_advance() {
    run_virtual(|clock| async move {
        let deadline = clock.now() + Duration::from_secs(30);
        let mut sleep = pin!(operations::sleep_until(deadline));

        // Poll once to register timer
        let completed = futures::future::poll_fn(|cx| match sleep.as_mut().poll(cx) {
            Poll::Pending => Poll::Ready(false),
            Poll::Ready(_) => Poll::Ready(true),
        })
        .await;
        assert!(!completed, "sleep_until should be pending before advance");

        // Advance past the deadline
        clock.advance(Duration::from_secs(30));
        sleep.await.unwrap();
    });
}

#[test]
fn sleep_until_with_past_deadline_completes_immediately() {
    run_virtual(|clock| async move {
        // Advance time forward, then create a sleep_until with a deadline in the past
        clock.advance(Duration::from_secs(100));
        let past_deadline = clock.now() - Duration::from_secs(50);

        let result = operations::sleep_until(past_deadline).await;
        result.unwrap();
    });
}

// ── P4: timeout_at with virtual time ─────────────────────────────────

#[test]
fn timeout_at_returns_value_when_inner_completes_first() {
    run_virtual(|clock| async move {
        let deadline = clock.now() + Duration::from_secs(10);
        let result = operations::timeout_at(deadline, async { 42 }).await;
        assert_eq!(result.unwrap(), 42);
    });
}

#[test]
fn timeout_at_returns_timeout_when_deadline_reached() {
    run_virtual(|clock| async move {
        let deadline = clock.now() + Duration::from_secs(5);

        // We race a never-completing future against the timeout.
        // Use poll_fn to poll timeout_at, then advance the clock.
        let mut timeout_fut = pin!(operations::timeout_at(
            deadline,
            std::future::poll_fn(|_cx| Poll::<()>::Pending), // never completes
        ));

        // First poll — should be Pending (inner is pending, timer not yet expired)
        let completed = futures::future::poll_fn(|cx| match timeout_fut.as_mut().poll(cx) {
            Poll::Pending => Poll::Ready(false),
            Poll::Ready(_) => Poll::Ready(true),
        })
        .await;
        assert!(!completed, "timeout_at should be pending before advance");

        // Advance past the deadline
        clock.advance(Duration::from_secs(5));

        // Now it should complete with Timeout
        let result = timeout_fut.await;
        assert!(
            matches!(result, Err(TimeoutError::Timeout)),
            "should return Timeout error, got: {:?}",
            result,
        );
    });
}

#[test]
fn timeout_at_with_long_timeout_allows_inner_to_complete() {
    run_virtual(|clock| async move {
        let deadline = clock.now() + Duration::from_secs(100);

        // Inner future completes immediately — timeout should not trigger
        let result = operations::timeout_at(deadline, async { "hello" }).await;
        assert_eq!(result.unwrap(), "hello");
    });
}

// ── P5: Drop cancellation ────────────────────────────────────────────

#[test]
fn drop_sleep_cancels_timer() {
    run_virtual(|clock| async move {
        // Create a sleep and poll it to register the timer
        {
            let mut sleep = pin!(operations::sleep(Duration::from_secs(100)));

            futures::future::poll_fn(|cx| {
                let _ = sleep.as_mut().poll(cx);
                Poll::Ready(())
            })
            .await;

            assert_eq!(clock.pending_timers(), 1, "timer should be registered");
            // sleep dropped here
        }

        assert_eq!(
            clock.pending_timers(),
            0,
            "timer should be cancelled on drop"
        );
    });
}

#[test]
fn cancel_method_prevents_timer_from_firing() {
    run_virtual(|clock| async move {
        let mut sleep = pin!(operations::sleep(Duration::from_secs(10)));

        // Poll to register timer
        futures::future::poll_fn(|cx| {
            let _ = sleep.as_mut().poll(cx);
            Poll::Ready(())
        })
        .await;

        assert_eq!(clock.pending_timers(), 1);

        // Cancel the sleep
        sleep.as_mut().cancel();

        assert_eq!(
            clock.pending_timers(),
            0,
            "timer should be cancelled after cancel()"
        );
    });
}

// ── Regression: spurious wakeup preserves timer ordering (F-2) ───────

#[test]
fn spurious_wakeup_preserves_timer_order() {
    run_virtual(|clock| async move {
        // Create two sleeps with the same deadline
        let mut sleep_a = pin!(operations::sleep(Duration::from_secs(10)));
        let mut sleep_b = pin!(operations::sleep(Duration::from_secs(10)));

        // Poll both to register timers (A gets lower TimerId)
        futures::future::poll_fn(|cx| {
            let _ = sleep_a.as_mut().poll(cx);
            let _ = sleep_b.as_mut().poll(cx);
            Poll::Ready(())
        })
        .await;

        // Spurious wakeup: poll both again without advancing time.
        // With the F-2 fix, this should NOT re-register timers (waker unchanged).
        futures::future::poll_fn(|cx| {
            let _ = sleep_a.as_mut().poll(cx);
            let _ = sleep_b.as_mut().poll(cx);
            Poll::Ready(())
        })
        .await;

        // Timer count should still be 2 (not 4 from re-registration)
        assert_eq!(
            clock.pending_timers(),
            2,
            "timers should not be duplicated by spurious wakeups"
        );

        // Advance past both deadlines — both should complete
        clock.advance(Duration::from_secs(10));
        sleep_a.await.unwrap();
        sleep_b.await.unwrap();
    });
}

// ── Regression: advance_to doesn't panic on re-entrant waker (F-1) ──

#[test]
fn advance_to_safe_with_reentrant_access() {
    run_virtual(|clock| async move {
        // Register a sleep — its waker will be called during advance.
        // The advance_to fix (F-1) ensures the RefCell borrow is dropped
        // before calling wake(), so any waker that accesses the clock
        // state won't panic.
        let mut sleep = pin!(operations::sleep(Duration::from_secs(1)));

        futures::future::poll_fn(|cx| {
            let _ = sleep.as_mut().poll(cx);
            Poll::Ready(())
        })
        .await;

        // This should not panic (waker may access clock state)
        clock.advance(Duration::from_secs(1));
        sleep.await.unwrap();
    });
}
