// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::{BoxError, Layer, Readiness, Service, ServiceError};

/// Snapshot of measured service load.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LoadSnapshot {
    /// Current in-flight requests.
    pub in_flight: usize,
    /// Completed successful requests.
    pub completed: usize,
    /// Failed requests.
    pub failed: usize,
}

#[derive(Default)]
struct LoadState {
    in_flight: AtomicUsize,
    completed: AtomicUsize,
    failed: AtomicUsize,
}

/// Layer that records service load.
#[derive(Clone, Default)]
pub struct LoadLayer {
    state: Arc<LoadState>,
}

impl LoadLayer {
    /// Creates a load measurement layer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a current load snapshot.
    pub fn snapshot(&self) -> LoadSnapshot {
        LoadSnapshot {
            in_flight: self.state.in_flight.load(Ordering::Acquire),
            completed: self.state.completed.load(Ordering::Acquire),
            failed: self.state.failed.load(Ordering::Acquire),
        }
    }
}

impl<S> Layer<S> for LoadLayer {
    type Service = Load<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Load {
            inner,
            state: Arc::clone(&self.state),
        }
    }
}

/// Load measurement middleware.
pub struct Load<S> {
    inner: S,
    state: Arc<LoadState>,
}

impl<Cx, Request, S> Service<Cx, Request> for Load<S>
where
    S: Service<Cx, Request>,
    S::Error: Into<BoxError>,
{
    type Response = S::Response;
    type Error = ServiceError;

    fn ready(&mut self, cx: &Cx) -> Result<Readiness, Self::Error> {
        self.inner
            .ready(cx)
            .map_err(|error| ServiceError::Inner(error.into()))
    }

    fn call(&mut self, cx: &Cx, request: Request) -> Result<Self::Response, Self::Error> {
        self.state.in_flight.fetch_add(1, Ordering::AcqRel);
        let result = self.inner.call(cx, request);
        self.state.in_flight.fetch_sub(1, Ordering::AcqRel);
        match result {
            Ok(response) => {
                self.state.completed.fetch_add(1, Ordering::AcqRel);
                Ok(response)
            }
            Err(error) => {
                self.state.failed.fetch_add(1, Ordering::AcqRel);
                Err(ServiceError::Inner(error.into()))
            }
        }
    }
}
