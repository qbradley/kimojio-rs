// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::time::Duration;

use crate::{BodyReplay, Error, ErrorKind, OperationClass, ReplayBody};

/// Retry decision for one failed attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetryDecision {
    RetryAfter(Duration),
    DoNotRetry,
}

/// Dynamic retry state supplied for an attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetryState {
    pub elapsed: Duration,
    pub canceled: bool,
    pub jitter: Duration,
}

/// Retry policy configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
    pub max_jitter: Duration,
    pub total_timeout: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            base_delay: Duration::from_millis(50),
            max_delay: Duration::from_secs(2),
            max_jitter: Duration::ZERO,
            total_timeout: Duration::from_secs(30),
        }
    }
}

impl RetryPolicy {
    /// Decides whether to retry an attempt.
    pub fn decide(
        self,
        attempt: u32,
        operation: OperationClass,
        body: &ReplayBody,
        error: &Error,
        retry_after: Option<Duration>,
    ) -> RetryDecision {
        self.decide_with_state(
            attempt,
            operation,
            body,
            error,
            retry_after,
            RetryState {
                elapsed: Duration::ZERO,
                canceled: false,
                jitter: Duration::ZERO,
            },
        )
    }

    /// Decides whether to retry an attempt with timeout and cancellation state.
    pub fn decide_with_state(
        self,
        attempt: u32,
        operation: OperationClass,
        body: &ReplayBody,
        error: &Error,
        retry_after: Option<Duration>,
        state: RetryState,
    ) -> RetryDecision {
        if state.canceled || state.elapsed >= self.total_timeout {
            return RetryDecision::DoNotRetry;
        }
        if attempt >= self.max_attempts || !is_retriable(operation, error.kind()) {
            return RetryDecision::DoNotRetry;
        }
        if body.replay() == BodyReplay::NonReplayable {
            return RetryDecision::DoNotRetry;
        }
        let delay = retry_after.unwrap_or_else(|| {
            self.delay_for_attempt(attempt)
                .saturating_add(state.jitter.min(self.max_jitter))
        });
        let remaining = self.total_timeout.saturating_sub(state.elapsed);
        if delay > remaining {
            RetryDecision::DoNotRetry
        } else {
            RetryDecision::RetryAfter(delay)
        }
    }

    fn delay_for_attempt(self, attempt: u32) -> Duration {
        let factor = 1_u32
            .checked_shl(attempt.saturating_sub(1))
            .unwrap_or(u32::MAX);
        self.base_delay.saturating_mul(factor).min(self.max_delay)
    }
}

fn is_retriable(operation: OperationClass, kind: ErrorKind) -> bool {
    if operation == OperationClass::PageWrite {
        return false;
    }
    matches!(
        kind,
        ErrorKind::Unavailable | ErrorKind::Timeout | ErrorKind::Transport
    )
}

/// Observation emitted for retry instrumentation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetryObservation {
    pub attempt: u32,
    pub decision: RetryDecision,
    pub error_kind: ErrorKind,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_policy_honors_retry_after_for_replayable_body() {
        let policy = RetryPolicy::default();
        let decision = policy.decide(
            1,
            OperationClass::PageRead,
            &ReplayBody::BorrowedStatic(b"abc"),
            &Error::new(ErrorKind::Unavailable, "busy"),
            Some(Duration::from_secs(3)),
        );

        assert_eq!(decision, RetryDecision::RetryAfter(Duration::from_secs(3)));
    }

    #[test]
    fn retry_policy_rejects_non_replayable_body() {
        let policy = RetryPolicy::default();
        let decision = policy.decide(
            1,
            OperationClass::List,
            &ReplayBody::NonReplayable { len: Some(512) },
            &Error::new(ErrorKind::Unavailable, "busy"),
            None,
        );

        assert_eq!(decision, RetryDecision::DoNotRetry);
    }

    #[test]
    fn retry_policy_retries_transport_failures_for_replayable_body() {
        let policy = RetryPolicy::default();
        let decision = policy.decide(
            1,
            OperationClass::PageRead,
            &ReplayBody::Empty,
            &Error::new(ErrorKind::Transport, "connection reset"),
            None,
        );

        assert!(matches!(decision, RetryDecision::RetryAfter(_)));
    }

    #[test]
    fn retry_policy_does_not_directly_retry_ambiguous_page_writes() {
        let policy = RetryPolicy::default();
        let decision = policy.decide(
            1,
            OperationClass::PageWrite,
            &ReplayBody::BorrowedStatic(b"abc"),
            &Error::new(ErrorKind::Unavailable, "ambiguous write"),
            None,
        );

        assert_eq!(decision, RetryDecision::DoNotRetry);
    }

    #[test]
    fn retry_policy_stops_at_max_attempts() {
        let policy = RetryPolicy {
            max_attempts: 1,
            ..RetryPolicy::default()
        };

        assert_eq!(
            policy.decide(
                1,
                OperationClass::PageRead,
                &ReplayBody::Empty,
                &Error::new(ErrorKind::Timeout, "timeout"),
                None,
            ),
            RetryDecision::DoNotRetry
        );
    }

    #[test]
    fn retry_policy_stops_when_cancelled_or_budget_exhausted() {
        let policy = RetryPolicy::default();
        let error = Error::new(ErrorKind::Unavailable, "busy");

        assert_eq!(
            policy.decide_with_state(
                1,
                OperationClass::PageRead,
                &ReplayBody::Empty,
                &error,
                None,
                RetryState {
                    elapsed: Duration::ZERO,
                    canceled: true,
                    jitter: Duration::ZERO,
                },
            ),
            RetryDecision::DoNotRetry
        );
        assert_eq!(
            policy.decide_with_state(
                1,
                OperationClass::PageRead,
                &ReplayBody::Empty,
                &error,
                Some(Duration::from_secs(60)),
                RetryState {
                    elapsed: Duration::from_secs(1),
                    canceled: false,
                    jitter: Duration::ZERO,
                },
            ),
            RetryDecision::DoNotRetry
        );
    }

    #[test]
    fn retry_policy_applies_caller_supplied_jitter_budget() {
        let policy = RetryPolicy {
            max_jitter: Duration::from_millis(10),
            ..RetryPolicy::default()
        };
        let decision = policy.decide_with_state(
            1,
            OperationClass::PageRead,
            &ReplayBody::Empty,
            &Error::new(ErrorKind::Unavailable, "busy"),
            None,
            RetryState {
                elapsed: Duration::ZERO,
                canceled: false,
                jitter: Duration::from_millis(25),
            },
        );

        assert_eq!(
            decision,
            RetryDecision::RetryAfter(Duration::from_millis(60))
        );
    }
}
