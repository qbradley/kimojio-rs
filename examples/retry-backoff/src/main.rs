// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Retry with exponential backoff — tested instantly with virtual time.
//!
//! This example shows how to write a retry loop with exponential backoff and
//! test it with kimojio's virtual clock. Real-world retry logic can involve
//! delays of minutes or hours, but virtual time makes the test complete in
//! microseconds.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use kimojio::operations;

/// Retry an async operation with exponential backoff.
///
/// Returns `Ok(value)` on success or `Err(last_error)` after `max_retries`
/// attempts. The delay doubles after each failure, starting from `initial_delay`
/// and capped at `max_delay`.
async fn retry_with_backoff<T, E, F, Fut>(
    initial_delay: Duration,
    max_delay: Duration,
    max_retries: u32,
    mut operation: F,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    let mut delay = initial_delay;

    for attempt in 0..=max_retries {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(err) if attempt == max_retries => return Err(err),
            Err(_) => {
                operations::sleep(delay).await.unwrap();
                delay = (delay * 2).min(max_delay);
            }
        }
    }

    unreachable!()
}

/// Simulates an unreliable service that fails the first N calls.
fn flaky_service(
    fail_count: u32,
) -> impl FnMut() -> std::future::Ready<Result<&'static str, &'static str>> {
    let calls = Rc::new(Cell::new(0u32));
    move || {
        let n = calls.get();
        calls.set(n + 1);
        if n < fail_count {
            std::future::ready(Err("service unavailable"))
        } else {
            std::future::ready(Ok("success"))
        }
    }
}

fn main() {
    // ── Demo 1: Successful retry ────────────────────────────────────────
    println!("=== Demo 1: Successful retry after failures ===\n");

    let (mut runtime, clock) =
        kimojio::Runtime::new_virtual(0, kimojio::configuration::Configuration::new());
    clock.set_auto_advance(true);

    let ch = clock.clone();
    runtime.block_on(async move {
        let service = flaky_service(3);
        let result =
            retry_with_backoff(Duration::from_secs(1), Duration::from_secs(30), 5, service).await;

        assert_eq!(result, Ok("success"));
        // 3 failures → sleeps of 1s + 2s + 4s = 7s of virtual time
        println!("  Result: {:?}", result);
        println!(
            "  Virtual time elapsed: {:?}",
            ch.now().duration_since(ch.epoch())
        );
    });

    println!();

    // ── Demo 2: All retries exhausted ───────────────────────────────────
    println!("=== Demo 2: All retries exhausted ===\n");

    let (mut runtime, clock) =
        kimojio::Runtime::new_virtual(0, kimojio::configuration::Configuration::new());
    clock.set_auto_advance(true);

    let ch = clock.clone();
    runtime.block_on(async move {
        let service = flaky_service(100);
        let result =
            retry_with_backoff(Duration::from_secs(1), Duration::from_secs(30), 3, service).await;

        assert_eq!(result, Err("service unavailable"));
        // 3 retries → sleeps of 1s + 2s + 4s = 7s
        println!("  Result: {:?}", result);
        println!(
            "  Virtual time elapsed: {:?}",
            ch.now().duration_since(ch.epoch())
        );
    });

    println!();

    // ── Demo 3: Max delay cap ───────────────────────────────────────────
    println!("=== Demo 3: Max delay cap ===\n");

    let (mut runtime, clock) =
        kimojio::Runtime::new_virtual(0, kimojio::configuration::Configuration::new());
    clock.set_auto_advance(true);

    let ch = clock.clone();
    runtime.block_on(async move {
        let service = flaky_service(6);
        let result = retry_with_backoff(
            Duration::from_millis(100),
            Duration::from_secs(1),
            8,
            service,
        )
        .await;

        assert_eq!(result, Ok("success"));
        // Delays: 100ms, 200ms, 400ms, 800ms, 1s (capped), 1s (capped) = 3.5s
        println!("  Result: {:?}", result);
        println!(
            "  Virtual time elapsed: {:?}",
            ch.now().duration_since(ch.epoch())
        );
    });

    drop(clock);
    println!("\nAll demos passed — no real time was consumed!");
}

// ── Tests using #[kimojio::test(virtual)] ───────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[kimojio::test(virtual)]
    async fn retry_succeeds_after_transient_failures(clock: VirtualClock) {
        clock.set_auto_advance(true);
        let service = flaky_service(2);

        let result =
            retry_with_backoff(Duration::from_secs(1), Duration::from_secs(30), 5, service).await;

        assert_eq!(result, Ok("success"));
        // 2 failures → sleeps of 1s + 2s = 3s virtual time
        let elapsed = clock.now().duration_since(clock.epoch());
        assert_eq!(elapsed, Duration::from_secs(3));
    }

    #[kimojio::test(virtual)]
    async fn retry_exhausted_returns_last_error(clock: VirtualClock) {
        clock.set_auto_advance(true);
        let service = flaky_service(100);

        let result =
            retry_with_backoff(Duration::from_secs(1), Duration::from_secs(30), 2, service).await;

        assert_eq!(result, Err("service unavailable"));
        // 2 retries → sleeps of 1s + 2s = 3s
        let elapsed = clock.now().duration_since(clock.epoch());
        assert_eq!(elapsed, Duration::from_secs(3));
    }

    #[kimojio::test(virtual)]
    async fn max_delay_is_respected(clock: VirtualClock) {
        clock.set_auto_advance(true);
        let service = flaky_service(5);

        let result = retry_with_backoff(
            Duration::from_millis(100),
            Duration::from_secs(1), // cap
            7,
            service,
        )
        .await;

        assert_eq!(result, Ok("success"));
        // Delays: 100ms, 200ms, 400ms, 800ms, 1s (capped) = 2.5s
        let elapsed = clock.now().duration_since(clock.epoch());
        assert_eq!(elapsed, Duration::from_millis(2500));
    }

    #[kimojio::test(virtual)]
    async fn immediate_success_skips_all_retries(clock: VirtualClock) {
        clock.set_auto_advance(true);
        let service = flaky_service(0); // succeeds on first try

        let result = retry_with_backoff(
            Duration::from_secs(60),
            Duration::from_secs(3600),
            10,
            service,
        )
        .await;

        assert_eq!(result, Ok("success"));
        // No failures → no sleep → 0 virtual time
        let elapsed = clock.now().duration_since(clock.epoch());
        assert_eq!(elapsed, Duration::ZERO);
    }
}
