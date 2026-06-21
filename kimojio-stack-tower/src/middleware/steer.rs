// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::{BoxError, Readiness, Service, ServiceError};

/// Routes requests to one of several services.
pub struct Steer<S, F> {
    services: Vec<S>,
    selector: F,
}

impl<S, F> Steer<S, F> {
    /// Creates a steering service from services and a selector.
    pub fn new(services: Vec<S>, selector: F) -> Self {
        Self::try_new(services, selector).expect("steer requires at least one service")
    }

    /// Creates a steering service, returning an error when no services exist.
    pub fn try_new(services: Vec<S>, selector: F) -> Result<Self, ServiceError> {
        if services.is_empty() {
            return Err(ServiceError::InvalidRequest(
                "steer requires at least one service",
            ));
        }
        Ok(Self { services, selector })
    }
}

impl<Cx, Request, S, F> Service<Cx, Request> for Steer<S, F>
where
    S: Service<Cx, Request>,
    S::Error: Into<BoxError>,
    F: Fn(&Request, usize) -> usize,
{
    type Response = S::Response;
    type Error = ServiceError;

    fn ready(&mut self, cx: &Cx) -> Result<Readiness, Self::Error> {
        for service in &mut self.services {
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
        let index = (self.selector)(&request, self.services.len()) % self.services.len();
        match self.services[index]
            .ready(cx)
            .map_err(|error| ServiceError::Inner(error.into()))?
        {
            Readiness::Ready => {}
            Readiness::NotReady | Readiness::Overloaded => return Err(ServiceError::Overloaded),
        }
        self.services[index]
            .call(cx, request)
            .map_err(|error| ServiceError::Inner(error.into()))
    }
}
