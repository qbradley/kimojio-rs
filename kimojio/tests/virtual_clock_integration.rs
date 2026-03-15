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

#[kimojio::test(virtual)]
async fn sleep_until_completes_after_advance(clock: VirtualClock) {
    let deadline = clock.now() + Duration::from_secs(30);
    let mut sleep = pin!(operations::sleep_until(deadline));

    assert!(
        !operations::poll_once(sleep.as_mut()).await,
        "sleep_until should be pending before advance"
    );

    clock.advance(Duration::from_secs(30));
    sleep.await.unwrap();
}

#[kimojio::test(virtual)]
async fn sleep_until_with_past_deadline_completes_immediately(clock: VirtualClock) {
    clock.advance(Duration::from_secs(100));
    let past_deadline = clock.now() - Duration::from_secs(50);
    operations::sleep_until(past_deadline).await.unwrap();
}

// ── P4: timeout_at with virtual time ─────────────────────────────────

#[kimojio::test(virtual)]
async fn timeout_at_returns_value_when_inner_completes_first(clock: VirtualClock) {
    let deadline = clock.now() + Duration::from_secs(10);
    let result = operations::timeout_at(deadline, async { 42 }).await;
    assert_eq!(result.unwrap(), 42);
}

#[kimojio::test(virtual)]
async fn timeout_at_returns_timeout_when_deadline_reached(clock: VirtualClock) {
    let deadline = clock.now() + Duration::from_secs(5);

    let mut timeout_fut = pin!(operations::timeout_at(
        deadline,
        std::future::poll_fn(|_cx| Poll::<()>::Pending),
    ));

    assert!(
        !operations::poll_once(timeout_fut.as_mut()).await,
        "timeout_at should be pending before advance"
    );

    clock.advance(Duration::from_secs(5));

    let result = timeout_fut.await;
    assert!(
        matches!(result, Err(TimeoutError::Timeout)),
        "should return Timeout error, got: {result:?}",
    );
}

#[kimojio::test(virtual)]
async fn timeout_at_with_long_timeout_allows_inner_to_complete(clock: VirtualClock) {
    let deadline = clock.now() + Duration::from_secs(100);
    let result = operations::timeout_at(deadline, async { "hello" }).await;
    assert_eq!(result.unwrap(), "hello");
}

// ── P5: Drop cancellation ────────────────────────────────────────────

#[kimojio::test(virtual)]
async fn drop_sleep_cancels_timer(clock: VirtualClock) {
    {
        let mut sleep = pin!(operations::sleep(Duration::from_secs(100)));
        operations::poll_once(sleep.as_mut()).await;
        assert_eq!(clock.pending_timers(), 1, "timer should be registered");
    }
    assert_eq!(
        clock.pending_timers(),
        0,
        "timer should be cancelled on drop"
    );
}

#[kimojio::test(virtual)]
async fn cancel_method_prevents_timer_from_firing(clock: VirtualClock) {
    let mut sleep = pin!(operations::sleep(Duration::from_secs(10)));
    operations::poll_once(sleep.as_mut()).await;
    assert_eq!(clock.pending_timers(), 1);

    sleep.as_mut().cancel();
    assert_eq!(
        clock.pending_timers(),
        0,
        "timer should be cancelled after cancel()"
    );
}

// ── Regression: spurious wakeup preserves timer ordering (F-2) ───────

#[kimojio::test(virtual)]
async fn spurious_wakeup_preserves_timer_order(clock: VirtualClock) {
    let mut sleep_a = pin!(operations::sleep(Duration::from_secs(10)));
    let mut sleep_b = pin!(operations::sleep(Duration::from_secs(10)));

    // Poll both to register timers (A gets lower TimerId)
    operations::poll_once(sleep_a.as_mut()).await;
    operations::poll_once(sleep_b.as_mut()).await;

    // Spurious wakeup — should NOT re-register timers
    operations::poll_once(sleep_a.as_mut()).await;
    operations::poll_once(sleep_b.as_mut()).await;

    assert_eq!(
        clock.pending_timers(),
        2,
        "timers should not be duplicated by spurious wakeups"
    );

    clock.advance(Duration::from_secs(10));
    sleep_a.await.unwrap();
    sleep_b.await.unwrap();
}

// ── Regression: advance_to doesn't panic on re-entrant waker (F-1) ──

#[kimojio::test(virtual)]
async fn advance_to_safe_with_reentrant_access(clock: VirtualClock) {
    let mut sleep = pin!(operations::sleep(Duration::from_secs(1)));
    operations::poll_once(sleep.as_mut()).await;

    clock.advance(Duration::from_secs(1));
    sleep.await.unwrap();
}

// ── Proc macro tests ────────────────────────────────────────────────

#[kimojio::test(virtual)]
async fn macro_virtual_sleep_completes(clock: VirtualClock) {
    let mut sleep = pin!(operations::sleep(Duration::from_secs(30)));
    assert!(!operations::poll_once(sleep.as_mut()).await);

    clock.advance(Duration::from_secs(30));
    sleep.await.unwrap();
}

#[kimojio::test(virtual)]
async fn macro_virtual_no_param() {
    // The virtual macro works without a clock parameter
    operations::sleep(Duration::ZERO).await.unwrap();
}
