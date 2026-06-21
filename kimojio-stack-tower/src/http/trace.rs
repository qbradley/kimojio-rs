// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use http::{Method, Request, Response, StatusCode, Uri};
use kimojio_stack_http::Body;

use crate::{BoxError, Layer, Readiness, Service, ServiceError};

/// Layer that records request/response trace event counts.
#[derive(Clone, Default)]
pub struct TraceLayer {
    events: Arc<AtomicUsize>,
    records: Arc<Mutex<Vec<TraceRecord>>>,
}

/// Structured trace event kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TraceEvent {
    /// Request was received.
    Request,
    /// Response was produced.
    Response,
    /// Inner service returned an error.
    Error,
}

/// Backend-neutral trace record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TraceRecord {
    /// Event kind.
    pub event: TraceEvent,
    /// Request method.
    pub method: Method,
    /// Request URI.
    pub uri: Uri,
    /// Response status for response events.
    pub status: Option<StatusCode>,
}

impl TraceLayer {
    /// Creates a trace layer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns recorded event count.
    pub fn events(&self) -> usize {
        self.events.load(Ordering::Acquire)
    }

    /// Returns recorded structured events.
    pub fn records(&self) -> Vec<TraceRecord> {
        self.records
            .lock()
            .expect("trace records mutex poisoned")
            .clone()
    }
}

impl<S> Layer<S> for TraceLayer {
    type Service = Trace<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Trace {
            inner,
            events: Arc::clone(&self.events),
            records: Arc::clone(&self.records),
        }
    }
}

/// Trace middleware.
pub struct Trace<S> {
    inner: S,
    events: Arc<AtomicUsize>,
    records: Arc<Mutex<Vec<TraceRecord>>>,
}

impl<Cx, S> Service<Cx, Request<Body>> for Trace<S>
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

    fn call(&mut self, cx: &Cx, request: Request<Body>) -> Result<Self::Response, Self::Error> {
        let method = request.method().clone();
        let uri = request.uri().clone();
        self.events.fetch_add(1, Ordering::AcqRel);
        self.record(TraceRecord {
            event: TraceEvent::Request,
            method: method.clone(),
            uri: uri.clone(),
            status: None,
        });
        let response = match self.inner.call(cx, request) {
            Ok(response) => response,
            Err(error) => {
                self.events.fetch_add(1, Ordering::AcqRel);
                self.record(TraceRecord {
                    event: TraceEvent::Error,
                    method,
                    uri,
                    status: None,
                });
                return Err(ServiceError::Inner(error.into()));
            }
        };
        self.events.fetch_add(1, Ordering::AcqRel);
        self.record(TraceRecord {
            event: TraceEvent::Response,
            method,
            uri,
            status: Some(response.status()),
        });
        Ok(response)
    }
}

impl<S> Trace<S> {
    fn record(&self, record: TraceRecord) {
        self.records
            .lock()
            .expect("trace records mutex poisoned")
            .push(record);
    }
}
