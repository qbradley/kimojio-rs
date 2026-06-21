// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::{BoxError, Layer, Readiness, Service, ServiceError};

/// Retry policy for boxed inner-service errors.
pub trait RetryPolicy: Clone {
    /// Returns whether `error` should be retried.
    fn should_retry(&self, error: &(dyn std::error::Error + Send + Sync + 'static)) -> bool;
}

/// Retry policy that retries every error until the attempt budget is exhausted.
#[derive(Clone, Copy, Debug, Default)]
pub struct RetryAll;

impl RetryPolicy for RetryAll {
    fn should_retry(&self, _error: &(dyn std::error::Error + Send + Sync + 'static)) -> bool {
        true
    }
}

/// Retry policy that never retries.
#[derive(Clone, Copy, Debug, Default)]
pub struct RetryNone;

impl RetryPolicy for RetryNone {
    fn should_retry(&self, _error: &(dyn std::error::Error + Send + Sync + 'static)) -> bool {
        false
    }
}

/// Layer that retries failed requests with a fixed attempt budget.
#[derive(Clone, Debug)]
pub struct RetryLayer<P = RetryAll> {
    max_attempts: usize,
    policy: P,
    attempts: Arc<AtomicUsize>,
}

impl RetryLayer {
    /// Creates a retry layer with at least one attempt.
    pub fn new(max_attempts: usize) -> Self {
        Self {
            max_attempts: max_attempts.max(1),
            policy: RetryAll,
            attempts: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl<P> RetryLayer<P>
where
    P: RetryPolicy,
{
    /// Creates a retry layer with a custom retry policy.
    pub fn with_policy(max_attempts: usize, policy: P) -> Self {
        Self {
            max_attempts: max_attempts.max(1),
            policy,
            attempts: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Returns the number of attempts observed through services from this layer.
    pub fn attempts(&self) -> usize {
        self.attempts.load(Ordering::Acquire)
    }
}

impl<S, P> Layer<S> for RetryLayer<P>
where
    P: RetryPolicy,
{
    type Service = Retry<S, P>;

    fn layer(&self, inner: S) -> Self::Service {
        Retry {
            inner,
            max_attempts: self.max_attempts,
            policy: self.policy.clone(),
            attempts: Arc::clone(&self.attempts),
        }
    }
}

/// Retry middleware.
pub struct Retry<S, P = RetryAll> {
    inner: S,
    max_attempts: usize,
    policy: P,
    attempts: Arc<AtomicUsize>,
}

impl<Cx, Request, S, P> Service<Cx, Request> for Retry<S, P>
where
    Request: Clone,
    S: Service<Cx, Request>,
    S::Error: Into<BoxError>,
    P: RetryPolicy,
{
    type Response = S::Response;
    type Error = ServiceError;

    fn ready(&mut self, cx: &Cx) -> Result<Readiness, Self::Error> {
        self.inner
            .ready(cx)
            .map_err(|error| ServiceError::Inner(error.into()))
    }

    fn call(&mut self, cx: &Cx, request: Request) -> Result<Self::Response, Self::Error> {
        let mut last_error = None;
        for attempt in 0..self.max_attempts {
            self.attempts.fetch_add(1, Ordering::AcqRel);
            let request = request.clone();
            match self.inner.call(cx, request) {
                Ok(response) => return Ok(response),
                Err(error) => {
                    let error = error.into();
                    let should_retry =
                        attempt + 1 < self.max_attempts && self.policy.should_retry(error.as_ref());
                    last_error = Some(error);
                    if !should_retry {
                        break;
                    }
                }
            }
        }
        Err(ServiceError::Inner(
            last_error.expect("retry has at least one attempt"),
        ))
    }
}
