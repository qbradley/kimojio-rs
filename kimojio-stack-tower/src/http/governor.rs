// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use http::{Request, Response, StatusCode};
use kimojio_stack_http::Body;

use crate::{BoxError, Layer, Readiness, Service, ServiceError};

use super::empty_response;

/// Governor-style keyed rate limiting layer.
#[derive(Clone)]
pub struct GovernorLayer<K> {
    key: K,
    limit: usize,
    max_keys: usize,
    counts: Arc<Mutex<BTreeMap<String, usize>>>,
}

impl<K> GovernorLayer<K> {
    /// Creates a keyed rate limiter.
    pub fn new(limit: usize, key: K) -> Self {
        Self {
            key,
            limit,
            max_keys: 1024,
            counts: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Sets the maximum number of tracked keys.
    pub fn max_keys(mut self, max_keys: usize) -> Self {
        self.max_keys = max_keys.max(1);
        self
    }
}

impl<S, K> Layer<S> for GovernorLayer<K>
where
    K: Clone,
{
    type Service = Governor<S, K>;

    fn layer(&self, inner: S) -> Self::Service {
        Governor {
            inner,
            key: self.key.clone(),
            limit: self.limit,
            max_keys: self.max_keys,
            counts: Arc::clone(&self.counts),
        }
    }
}

/// Keyed rate-limiting middleware.
pub struct Governor<S, K> {
    inner: S,
    key: K,
    limit: usize,
    max_keys: usize,
    counts: Arc<Mutex<BTreeMap<String, usize>>>,
}

impl<Cx, S, K> Service<Cx, Request<Body>> for Governor<S, K>
where
    S: Service<Cx, Request<Body>, Response = Response<Body>>,
    S::Error: Into<BoxError>,
    K: Fn(&Request<Body>) -> String,
{
    type Response = Response<Body>;
    type Error = ServiceError;

    fn ready(&mut self, cx: &Cx) -> Result<Readiness, Self::Error> {
        self.inner
            .ready(cx)
            .map_err(|error| ServiceError::Inner(error.into()))
    }

    fn call(&mut self, cx: &Cx, request: Request<Body>) -> Result<Self::Response, Self::Error> {
        let key = (self.key)(&request);
        {
            let mut counts = self.counts.lock().expect("governor mutex poisoned");
            if !counts.contains_key(&key)
                && counts.len() >= self.max_keys
                && let Some(first) = counts.keys().next().cloned()
            {
                counts.remove(&first);
            }
            let count = counts.entry(key).or_default();
            if *count >= self.limit {
                return Ok(empty_response(StatusCode::TOO_MANY_REQUESTS));
            }
            *count += 1;
        }
        self.inner
            .call(cx, request)
            .map_err(|error| ServiceError::Inner(error.into()))
    }
}
