// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::{BoxError, Layer, Readiness, Service, ServiceError};

use super::discover::Discover;

/// Round-robin balance layer over a discovery source.
#[derive(Clone)]
pub struct BalanceLayer<D> {
    discover: D,
    cursor: Arc<AtomicUsize>,
}

impl<D> BalanceLayer<D> {
    /// Creates a balance layer.
    pub fn new(discover: D) -> Self {
        Self {
            discover,
            cursor: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl<S, D> Layer<S> for BalanceLayer<D>
where
    D: Clone,
{
    type Service = Balance<D>;

    fn layer(&self, _inner: S) -> Self::Service {
        Balance {
            discover: self.discover.clone(),
            cursor: Arc::clone(&self.cursor),
        }
    }
}

/// Round-robin balance service.
pub struct Balance<D> {
    discover: D,
    cursor: Arc<AtomicUsize>,
}

impl<Cx, Request, D> Service<Cx, Request> for Balance<D>
where
    D: Discover,
    D::Service: Service<Cx, Request>,
    <D::Service as Service<Cx, Request>>::Error: Into<BoxError>,
{
    type Response = <D::Service as Service<Cx, Request>>::Response;
    type Error = ServiceError;

    fn ready(&mut self, cx: &Cx) -> Result<Readiness, Self::Error> {
        for mut service in self.discover.services() {
            if service
                .ready(cx)
                .map_err(|error| ServiceError::Inner(error.into()))?
                == Readiness::Ready
            {
                return Ok(Readiness::Ready);
            }
        }
        Ok(Readiness::NotReady)
    }

    fn call(&mut self, cx: &Cx, request: Request) -> Result<Self::Response, Self::Error> {
        let services = self.discover.services();
        if services.is_empty() {
            return Err(ServiceError::Overloaded);
        }
        let start = self.cursor.fetch_add(1, Ordering::AcqRel);
        let mut last_not_ready = false;
        for offset in 0..services.len() {
            let index = (start + offset) % services.len();
            let mut service = services[index].clone();
            if service
                .ready(cx)
                .map_err(|error| ServiceError::Inner(error.into()))?
                == Readiness::Ready
            {
                return service
                    .call(cx, request)
                    .map_err(|error| ServiceError::Inner(error.into()));
            }
            last_not_ready = true;
        }
        if last_not_ready {
            Err(ServiceError::Overloaded)
        } else {
            Err(ServiceError::NotReady)
        }
    }
}
