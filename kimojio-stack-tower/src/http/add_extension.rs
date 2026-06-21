// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use http::Request;
use kimojio_stack_http::Body;

use crate::{BoxError, Layer, Readiness, Service, ServiceError};

/// Layer that clones a value into each request's extensions.
#[derive(Clone)]
pub struct AddExtensionLayer<T> {
    value: T,
}

impl<T> AddExtensionLayer<T> {
    /// Creates an add-extension layer.
    pub fn new(value: T) -> Self {
        Self { value }
    }
}

impl<S, T> Layer<S> for AddExtensionLayer<T>
where
    T: Clone,
{
    type Service = AddExtension<S, T>;

    fn layer(&self, inner: S) -> Self::Service {
        AddExtension {
            inner,
            value: self.value.clone(),
        }
    }
}

/// Add-extension middleware.
pub struct AddExtension<S, T> {
    inner: S,
    value: T,
}

impl<Cx, S, T> Service<Cx, Request<Body>> for AddExtension<S, T>
where
    S: Service<Cx, Request<Body>>,
    S::Error: Into<BoxError>,
    T: Clone + Send + Sync + 'static,
{
    type Response = S::Response;
    type Error = ServiceError;

    fn ready(&mut self, cx: &Cx) -> Result<Readiness, Self::Error> {
        self.inner
            .ready(cx)
            .map_err(|error| ServiceError::Inner(error.into()))
    }

    fn call(&mut self, cx: &Cx, mut request: Request<Body>) -> Result<Self::Response, Self::Error> {
        request.extensions_mut().insert(self.value.clone());
        self.inner
            .call(cx, request)
            .map_err(|error| ServiceError::Inner(error.into()))
    }
}
