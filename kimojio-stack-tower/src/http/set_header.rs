// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use http::{HeaderName, HeaderValue, Request, Response};
use kimojio_stack_http::Body;

use crate::{BoxError, Layer, Readiness, Service, ServiceError};

/// Header mutation target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SetHeaderTarget {
    /// Set request header before calling inner service.
    Request,
    /// Set response header after calling inner service.
    Response,
}

/// Layer that sets a request or response header.
#[derive(Clone, Debug)]
pub struct SetHeaderLayer {
    target: SetHeaderTarget,
    name: HeaderName,
    value: HeaderValue,
}

impl SetHeaderLayer {
    /// Sets a request header.
    pub fn request(name: HeaderName, value: HeaderValue) -> Self {
        Self {
            target: SetHeaderTarget::Request,
            name,
            value,
        }
    }

    /// Sets a response header.
    pub fn response(name: HeaderName, value: HeaderValue) -> Self {
        Self {
            target: SetHeaderTarget::Response,
            name,
            value,
        }
    }
}

impl<S> Layer<S> for SetHeaderLayer {
    type Service = SetHeader<S>;

    fn layer(&self, inner: S) -> Self::Service {
        SetHeader {
            inner,
            target: self.target,
            name: self.name.clone(),
            value: self.value.clone(),
        }
    }
}

/// Set-header middleware.
pub struct SetHeader<S> {
    inner: S,
    target: SetHeaderTarget,
    name: HeaderName,
    value: HeaderValue,
}

impl<Cx, S> Service<Cx, Request<Body>> for SetHeader<S>
where
    S: Service<Cx, Request<Body>, Response = Response<Body>>,
    S::Error: Into<BoxError>,
{
    type Response = Response<Body>;
    type Error = ServiceError;

    fn ready(&mut self, cx: &Cx) -> Result<Readiness, Self::Error> {
        self.inner
            .ready(cx)
            .map_err(|error| ServiceError::Inner(error.into()))
    }

    fn call(&mut self, cx: &Cx, mut request: Request<Body>) -> Result<Self::Response, Self::Error> {
        if self.target == SetHeaderTarget::Request {
            request
                .headers_mut()
                .insert(self.name.clone(), self.value.clone());
        }
        let mut response = self
            .inner
            .call(cx, request)
            .map_err(|error| ServiceError::Inner(error.into()))?;
        if self.target == SetHeaderTarget::Response {
            response
                .headers_mut()
                .insert(self.name.clone(), self.value.clone());
        }
        Ok(response)
    }
}
