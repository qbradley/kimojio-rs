// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for the virtual clock feature.
//!
//! These tests exercise the full stack: virtual clock → runtime → operations,
//! verifying that sleep, sleep_until, timeout_at, and drop cancellation all
//! work correctly with virtual time advancement.

#![cfg(feature = "virtual-clock")]

use std::pin::pin;
use std::task::Poll;
use std::time::Duration;

use kimojio::TimeoutError;
use kimojio::operations;

// ── P2: sleep_until with virtual time ────────────────────────────────

#[kimojio::test]
async fn sleep_until_completes_after_advance() {
    operations::virtual_clock_enable(true);
    let deadline = operations::virtual_clock_now() + Duration::from_secs(30);
    let mut sleep = pin!(operations::sleep_until(deadline));

    assert!(
        operations::poll_once(sleep.as_mut()).await.is_none(),
        "sleep_until should be pending before advance"
    );

    operations::virtual_clock_advance(Duration::from_secs(30));
    sleep.await.unwrap();
}

#[kimojio::test]
async fn sleep_until_with_past_deadline_completes_immediately() {
    operations::virtual_clock_enable(true);
    operations::virtual_clock_advance(Duration::from_secs(100));
    let past_deadline = operations::virtual_clock_now() - Duration::from_secs(50);
    operations::sleep_until(past_deadline).await.unwrap();
}

// ── P4: timeout_at with virtual time ─────────────────────────────────

#[kimojio::test]
async fn timeout_at_returns_value_when_inner_completes_first() {
    operations::virtual_clock_enable(true);
    let deadline = operations::virtual_clock_now() + Duration::from_secs(10);
    let result = operations::timeout_at(deadline, async { 42 }).await;
    assert_eq!(result.unwrap(), 42);
}

#[kimojio::test]
async fn timeout_at_returns_timeout_when_deadline_reached() {
    operations::virtual_clock_enable(true);
    let deadline = operations::virtual_clock_now() + Duration::from_secs(5);

    let mut timeout_fut = pin!(operations::timeout_at(
        deadline,
        std::future::poll_fn(|_cx| Poll::<()>::Pending),
    ));

    assert!(
        operations::poll_once(timeout_fut.as_mut()).await.is_none(),
        "timeout_at should be pending before advance"
    );

    operations::virtual_clock_advance(Duration::from_secs(5));

    let result = timeout_fut.await;
    assert!(
        matches!(result, Err(TimeoutError::Timeout)),
        "should return Timeout error, got: {result:?}",
    );
}

#[kimojio::test]
async fn timeout_at_with_long_timeout_allows_inner_to_complete() {
    operations::virtual_clock_enable(true);
    let deadline = operations::virtual_clock_now() + Duration::from_secs(100);
    let result = operations::timeout_at(deadline, async { "hello" }).await;
    assert_eq!(result.unwrap(), "hello");
}

// ── P5: Drop cancellation ────────────────────────────────────────────

#[kimojio::test]
async fn drop_sleep_cancels_timer() {
    operations::virtual_clock_enable(true);
    {
        let mut sleep = pin!(operations::sleep(Duration::from_secs(100)));
        operations::poll_once(sleep.as_mut()).await;
        assert_eq!(
            operations::virtual_clock_pending_timers(),
            1,
            "timer should be registered"
        );
    }
    assert_eq!(
        operations::virtual_clock_pending_timers(),
        0,
        "timer should be cancelled on drop"
    );
}

#[kimojio::test]
async fn cancel_method_prevents_timer_from_firing() {
    operations::virtual_clock_enable(true);
    let mut sleep = pin!(operations::sleep(Duration::from_secs(10)));
    operations::poll_once(sleep.as_mut()).await;
    assert_eq!(operations::virtual_clock_pending_timers(), 1);

    sleep.as_mut().cancel();
    assert_eq!(
        operations::virtual_clock_pending_timers(),
        0,
        "timer should be cancelled after cancel()"
    );
}

// ── Regression: spurious wakeup preserves timer ordering (F-2) ───────

#[kimojio::test]
async fn spurious_wakeup_preserves_timer_order() {
    operations::virtual_clock_enable(true);
    let mut sleep_a = pin!(operations::sleep(Duration::from_secs(10)));
    let mut sleep_b = pin!(operations::sleep(Duration::from_secs(10)));

    // Poll both to register timers (A gets lower TimerId)
    operations::poll_once(sleep_a.as_mut()).await;
    operations::poll_once(sleep_b.as_mut()).await;

    // Spurious wakeup — should NOT re-register timers
    operations::poll_once(sleep_a.as_mut()).await;
    operations::poll_once(sleep_b.as_mut()).await;

    assert_eq!(
        operations::virtual_clock_pending_timers(),
        2,
        "timers should not be duplicated by spurious wakeups"
    );

    operations::virtual_clock_advance(Duration::from_secs(10));
    sleep_a.await.unwrap();
    sleep_b.await.unwrap();
}

// ── Regression: advance_to doesn't panic on re-entrant waker (F-1) ──

#[kimojio::test]
async fn advance_to_safe_with_reentrant_access() {
    operations::virtual_clock_enable(true);
    let mut sleep = pin!(operations::sleep(Duration::from_secs(1)));
    operations::poll_once(sleep.as_mut()).await;

    operations::virtual_clock_advance(Duration::from_secs(1));
    sleep.await.unwrap();
}

// ── Proc macro tests ────────────────────────────────────────────────

#[kimojio::test]
async fn macro_virtual_sleep_completes() {
    operations::virtual_clock_enable(true);
    let mut sleep = pin!(operations::sleep(Duration::from_secs(30)));
    assert!(operations::poll_once(sleep.as_mut()).await.is_none());

    operations::virtual_clock_advance(Duration::from_secs(30));
    sleep.await.unwrap();
}

#[kimojio::test]
async fn macro_virtual_no_param() {
    operations::virtual_clock_enable(true);
    // The virtual macro works without a clock parameter
    operations::sleep(Duration::ZERO).await.unwrap();
}

// ── epoch() tests ───────────────────────────────────────────────────

#[kimojio::test]
async fn epoch_returns_start_time() {
    operations::virtual_clock_enable(true);
    let epoch = operations::virtual_clock_epoch();
    assert_eq!(
        operations::virtual_clock_now(),
        epoch,
        "initially now == epoch"
    );
    operations::virtual_clock_advance(Duration::from_secs(100));
    assert_eq!(
        operations::virtual_clock_epoch(),
        epoch,
        "epoch doesn't change"
    );
    assert_eq!(
        operations::virtual_clock_now().duration_since(epoch),
        Duration::from_secs(100)
    );
}

// ── Past-deadline and zero-duration edge cases ───────────────────────

/// Tests that a sleep with a past deadline completes immediately.
#[kimojio::test]
async fn past_deadline_sleep_completes_immediately() {
    operations::virtual_clock_enable(true);
    operations::virtual_clock_advance(Duration::from_secs(100));
    let past = operations::virtual_clock_epoch();
    operations::sleep_until(past).await.unwrap();
}

/// Tests that sleep(Duration::ZERO) completes without hanging.
#[kimojio::test]
async fn zero_duration_sleep_completes() {
    operations::virtual_clock_enable(true);
    operations::sleep(Duration::ZERO).await.unwrap();
}

// ── Idle advance tests ───────────────────────────────────────────────

/// Sleep completes automatically via a single idle advance.
#[kimojio::test]
async fn idle_advance_completes_sleep() {
    operations::virtual_clock_enable(true);
    // Queue an advance that will fire when the runtime is idle
    operations::virtual_clock_advance_idle(Duration::from_secs(60));

    // This sleep will register a timer, yield, and the runtime
    // will apply the idle advance (since no tasks are ready),
    // waking the timer, and completing the sleep.
    operations::sleep(Duration::from_secs(60)).await.unwrap();
}

/// Multiple idle advances fire in order at successive idle points.
#[kimojio::test]
async fn idle_advance_fires_in_order() {
    operations::virtual_clock_enable(true);
    let epoch = operations::virtual_clock_epoch();

    operations::virtual_clock_advance_idle(Duration::from_secs(10));
    operations::virtual_clock_advance_idle(Duration::from_secs(20));

    // First sleep completes via first idle advance (+10s)
    operations::sleep(Duration::from_secs(10)).await.unwrap();
    assert_eq!(
        operations::virtual_clock_now().duration_since(epoch),
        Duration::from_secs(10)
    );

    // Second sleep completes via second idle advance (+20s = 30s total)
    operations::sleep(Duration::from_secs(20)).await.unwrap();
    assert_eq!(
        operations::virtual_clock_now().duration_since(epoch),
        Duration::from_secs(30)
    );
}

/// Idle advance wakes spawned tasks correctly.
#[kimojio::test]
async fn idle_advance_wakes_spawned_task() {
    operations::virtual_clock_enable(true);
    let done = std::rc::Rc::new(std::cell::Cell::new(false));
    let d = done.clone();

    operations::virtual_clock_advance_idle(Duration::from_secs(60));

    operations::spawn_task(async move {
        operations::sleep(Duration::from_secs(60)).await.unwrap();
        d.set(true);
    });

    // Main task sleeps too — both pending → idle → advance fires → both wake
    operations::sleep(Duration::from_secs(60)).await.unwrap();
    // Yield to let spawned task (woken by same advance) complete
    operations::yield_io().await;

    assert!(
        done.get(),
        "spawned task should have completed via idle advance"
    );
}

/// Pending idle advances count is correct.
#[kimojio::test]
async fn pending_idle_advances_count() {
    operations::virtual_clock_enable(true);
    assert_eq!(operations::virtual_clock_pending_idle_advances(), 0);

    operations::virtual_clock_advance_idle(Duration::from_secs(10));
    operations::virtual_clock_advance_idle(Duration::from_secs(20));
    assert_eq!(operations::virtual_clock_pending_idle_advances(), 2);

    // After a sleep that consumes one advance
    operations::sleep(Duration::from_secs(10)).await.unwrap();
    assert_eq!(operations::virtual_clock_pending_idle_advances(), 1);
}

// ── Idle advance default tests ───────────────────────────────────────

/// Default idle advance resolves sleeps without pre-queuing.
#[kimojio::test]
async fn idle_advance_default_resolves_sleep() {
    operations::virtual_clock_enable(true);
    operations::virtual_clock_advance_idle_default(Some(Duration::from_millis(1)));
    operations::sleep(Duration::from_secs(60)).await.unwrap();
}

/// Clearing the default stops automatic advancement.
#[kimojio::test]
async fn idle_advance_default_cleared() {
    operations::virtual_clock_enable(true);
    let epoch = operations::virtual_clock_epoch();

    operations::virtual_clock_advance_idle_default(Some(Duration::from_secs(10)));
    operations::sleep(Duration::from_secs(10)).await.unwrap();

    // Clear default, queue one explicit advance for the remaining work
    operations::virtual_clock_advance_idle_default(None);
    operations::virtual_clock_advance_idle(Duration::from_secs(20));
    operations::sleep(Duration::from_secs(20)).await.unwrap();

    assert_eq!(
        operations::virtual_clock_now().duration_since(epoch),
        Duration::from_secs(30)
    );
}
