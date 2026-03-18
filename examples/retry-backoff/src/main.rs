// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Demonstrates testing retry logic with exponential backoff using kimojio's
//! virtual clock. The test completes in milliseconds of wall time while
//! exercising minutes of virtual time.

#[cfg(test)]
use kimojio::operations;
#[cfg(test)]
use std::time::Duration;

/// A simulated service that fails the first N calls, then succeeds.
#[cfg(test)]
struct FlakyService {
    remaining_failures: std::cell::Cell<usize>,
}

#[cfg(test)]
impl FlakyService {
    fn new(failures: usize) -> Self {
        Self {
            remaining_failures: std::cell::Cell::new(failures),
        }
    }

    fn call(&self) -> Result<&str, &str> {
        let remaining = self.remaining_failures.get();
        if remaining > 0 {
            self.remaining_failures.set(remaining - 1);
            Err("service unavailable")
        } else {
            Ok("success")
        }
    }
}

/// Retries a fallible operation with exponential backoff.
///
/// With virtual clock, each `sleep()` resolves instantly when time is advanced,
/// so the entire retry sequence completes in milliseconds of wall time.
#[cfg(test)]
async fn retry_with_backoff<T, E>(
    mut op: impl FnMut() -> Result<T, E>,
    max_retries: usize,
    initial_delay: Duration,
) -> Result<T, E> {
    let mut delay = initial_delay;
    let mut last_err = None;

    for _ in 0..=max_retries {
        match op() {
            Ok(val) => return Ok(val),
            Err(e) => {
                last_err = Some(e);
                operations::sleep(delay).await.unwrap();
                delay *= 2; // exponential backoff
            }
        }
    }

    Err(last_err.unwrap())
}

#[kimojio::test]
async fn retry_backoff_with_virtual_clock() {
    operations::virtual_clock_enable(true);
    let epoch = operations::virtual_clock_epoch();

    // Queue enough idle advances for each retry's sleep.
    // With 3 failures: delays are 1s, 2s, 4s (then success on 4th call).
    operations::virtual_clock_advance_idle(Duration::from_secs(1));
    operations::virtual_clock_advance_idle(Duration::from_secs(2));
    operations::virtual_clock_advance_idle(Duration::from_secs(4));

    let service = FlakyService::new(3);
    let result = retry_with_backoff(|| service.call(), 5, Duration::from_secs(1)).await;

    assert_eq!(result, Ok("success"));

    // Verify virtual time accumulated correctly: 1 + 2 + 4 = 7 seconds
    let elapsed = operations::virtual_clock_now().duration_since(epoch);
    assert_eq!(elapsed, Duration::from_secs(7));
}

/// Manual advancement variant — precise control over each step.
#[kimojio::test]
async fn retry_backoff_manual_advance() {
    operations::virtual_clock_enable(true);
    let epoch = operations::virtual_clock_epoch();

    let service = FlakyService::new(2);

    // Attempt 1: fails, wait 1s
    assert!(service.call().is_err());
    let mut sleep = std::pin::pin!(operations::sleep(Duration::from_secs(1)));
    operations::poll_once(sleep.as_mut()).await; // register timer
    operations::virtual_clock_advance(Duration::from_secs(1));
    sleep.await.unwrap();

    // Attempt 2: fails, wait 2s
    assert!(service.call().is_err());
    let mut sleep = std::pin::pin!(operations::sleep(Duration::from_secs(2)));
    operations::poll_once(sleep.as_mut()).await;
    operations::virtual_clock_advance(Duration::from_secs(2));
    sleep.await.unwrap();

    // Attempt 3: succeeds
    assert_eq!(service.call(), Ok("success"));

    // Total virtual time: 1 + 2 = 3 seconds
    let elapsed = operations::virtual_clock_now().duration_since(epoch);
    assert_eq!(elapsed, Duration::from_secs(3));
}

fn main() {
    // This example is test-only; the main function is a placeholder.
    println!("Run with: cargo test -p retry-backoff --all-features");
}
