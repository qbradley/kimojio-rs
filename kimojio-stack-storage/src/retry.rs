// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Explicit retry policy for storage attempts.
//!
//! The policy is deliberately a pure decision function: it does not sleep,
//! mutate transport state, or resubmit requests. Operation loops call
//! [`RetryPolicy::decide_with_state`], sleep through the stack runtime if a retry
//! is allowed, and then execute another attempt themselves.
//!
//! ```
//! use std::time::Duration;
//! use kimojio_stack_storage::{
//!     Error, ErrorKind, OperationClass, ReplayBody, RetryDecision, RetryPolicy,
//! };
//!
//! let policy = RetryPolicy::default();
//! let decision = policy.decide(
//!     1,
//!     OperationClass::PageRead,
//!     &ReplayBody::Empty,
//!     &Error::new(ErrorKind::Unavailable, "busy"),
//!     None,
//! );
//! assert!(matches!(decision, RetryDecision::RetryAfter(_)));
//! ```

use std::time::Duration;

use crate::{BodyReplay, Error, ErrorKind, OperationClass, ReplayBody};

/// Retry decision for one failed attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetryDecision {
    /// Retry after the supplied delay.
    RetryAfter(Duration),
    /// Do not retry this failure.
    DoNotRetry,
}

/// Dynamic retry state supplied for an attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetryState {
    /// Total elapsed time since the operation started.
    pub elapsed: Duration,
    /// Whether the caller has cancelled the operation.
    pub canceled: bool,
    /// Caller-supplied jitter for this decision.
    pub jitter: Duration,
}

/// Retry policy configuration.
///
/// `max_attempts` includes the first attempt. `max_jitter` caps the jitter value
/// supplied through [`RetryState`]; the policy does not generate randomness.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetryPolicy {
    /// Maximum total attempts, including attempt 1.
    pub max_attempts: u32,
    /// Base exponential backoff delay.
    pub base_delay: Duration,
    /// Maximum computed backoff delay before jitter.
    pub max_delay: Duration,
    /// Maximum caller-supplied jitter that can be added.
    pub max_jitter: Duration,
    /// Total operation retry budget.
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
    /// Decides whether to retry an attempt with default elapsed/cancel/jitter state.
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
    ///
    /// Page writes are not retried directly because completion can be ambiguous
    /// after a transport failure. Callers should refresh properties or sequence
    /// number state before deciding whether another write is safe.
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
    /// Attempt number that produced this observation.
    pub attempt: u32,
    /// Retry decision returned by the policy.
    pub decision: RetryDecision,
    /// Stable error kind used for the decision.
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
