// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Stackful service, readiness, layer, and type-erasure abstractions.

use std::convert::Infallible;

/// Readiness state returned by a stackful [`Service`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Readiness {
    /// The service can accept a request now.
    Ready,
    /// The service is temporarily not ready.
    NotReady,
    /// The service is overloaded and work should be shed or retried elsewhere.
    Overloaded,
}

impl Readiness {
    /// Returns `true` when the service can accept work immediately.
    pub const fn is_ready(self) -> bool {
        matches!(self, Self::Ready)
    }
}

/// Stackful request/response processing abstraction.
///
/// `Cx` is the explicit runtime context type supplied by the caller. Services
/// can use it to yield, sleep, park, spawn, or perform I/O according to that
/// runtime's capabilities.
pub trait Service<Cx, Request> {
    /// Response type returned for successful requests.
    type Response;
    /// Error type returned by readiness or request processing.
    type Error;

    /// Reports whether the service can accept a request.
    fn ready(&mut self, _cx: &Cx) -> Result<Readiness, Self::Error> {
        Ok(Readiness::Ready)
    }

    /// Processes one request.
    fn call(&mut self, cx: &Cx, request: Request) -> Result<Self::Response, Self::Error>;
}

impl<Cx, Request, S> Service<Cx, Request> for &mut S
where
    S: Service<Cx, Request> + ?Sized,
{
    type Response = S::Response;
    type Error = S::Error;

    fn ready(&mut self, cx: &Cx) -> Result<Readiness, Self::Error> {
        (**self).ready(cx)
    }

    fn call(&mut self, cx: &Cx, request: Request) -> Result<Self::Response, Self::Error> {
        (**self).call(cx, request)
    }
}

impl<Cx, Request, S> Service<Cx, Request> for Box<S>
where
    S: Service<Cx, Request> + ?Sized,
{
    type Response = S::Response;
    type Error = S::Error;

    fn ready(&mut self, cx: &Cx) -> Result<Readiness, Self::Error> {
        (**self).ready(cx)
    }

    fn call(&mut self, cx: &Cx, request: Request) -> Result<Self::Response, Self::Error> {
        (**self).call(cx, request)
    }
}

/// Type-erased service helper.
pub type BoxService<Cx, Request, Response, Error> =
    Box<dyn Service<Cx, Request, Response = Response, Error = Error> + Send + 'static>;

/// Cloneable type-erased service helper.
pub type BoxCloneService<Cx, Request, Response, Error> =
    Box<dyn CloneService<Cx, Request, Response = Response, Error = Error> + Send + 'static>;

/// Object-safe cloneable service trait used by [`BoxCloneService`].
pub trait CloneService<Cx, Request>: Service<Cx, Request> {
    /// Clones this service into a boxed cloneable service.
    fn clone_box(
        &self,
    ) -> Box<dyn CloneService<Cx, Request, Response = Self::Response, Error = Self::Error> + Send>;
}

impl<Cx, Request, S> CloneService<Cx, Request> for S
where
    S: Service<Cx, Request> + Clone + Send + 'static,
    S::Response: 'static,
    S::Error: 'static,
{
    fn clone_box(
        &self,
    ) -> Box<dyn CloneService<Cx, Request, Response = Self::Response, Error = Self::Error> + Send>
    {
        Box::new(self.clone())
    }
}

impl<Cx, Request, Response, Error> Clone for BoxCloneService<Cx, Request, Response, Error>
where
    Cx: 'static,
    Request: 'static,
    Response: 'static,
    Error: 'static,
{
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

/// Constructs a [`Service`] from a closure.
pub fn service_fn<F>(f: F) -> ServiceFn<F> {
    ServiceFn { f }
}

/// Service created by [`service_fn`].
#[derive(Clone, Debug)]
pub struct ServiceFn<F> {
    f: F,
}

impl<Cx, Request, Response, Error, F> Service<Cx, Request> for ServiceFn<F>
where
    F: FnMut(&Cx, Request) -> Result<Response, Error>,
{
    type Response = Response;
    type Error = Error;

    fn call(&mut self, cx: &Cx, request: Request) -> Result<Self::Response, Self::Error> {
        (self.f)(cx, request)
    }
}

/// Middleware constructor that wraps one service with another.
pub trait Layer<S> {
    /// Service type produced by this layer.
    type Service;

    /// Wraps `inner` with this layer.
    fn layer(&self, inner: S) -> Self::Service;
}

impl<S, F, T> Layer<S> for F
where
    F: Fn(S) -> T,
{
    type Service = T;

    fn layer(&self, inner: S) -> Self::Service {
        self(inner)
    }
}

/// Convenience extension methods for services.
pub trait ServiceExt: Sized {
    /// Wraps this service with `layer`.
    fn layer<L>(self, layer: L) -> L::Service
    where
        L: Layer<Self>,
    {
        Layer::layer(&layer, self)
    }
}

impl<S> ServiceExt for S {}

impl<Cx, Request> Service<Cx, Request> for Infallible {
    type Response = Infallible;
    type Error = Infallible;

    fn call(&mut self, _cx: &Cx, _request: Request) -> Result<Self::Response, Self::Error> {
        match *self {}
    }
}
