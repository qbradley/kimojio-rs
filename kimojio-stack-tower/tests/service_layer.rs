// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::cell::RefCell;
use std::error::Error;
use std::fmt;
use std::time::Duration;

use kimojio_stack_tower::{Layer, Readiness, Service, ServiceError, ServiceExt, service_fn};

#[derive(Debug)]
struct StaticError(&'static str);

impl fmt::Display for StaticError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl Error for StaticError {}

#[derive(Clone)]
struct AddLayer {
    value: u64,
}

impl<S> Layer<S> for AddLayer {
    type Service = AddService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AddService {
            inner,
            value: self.value,
        }
    }
}

struct AddService<S> {
    inner: S,
    value: u64,
}

impl<Cx, S> Service<Cx, u64> for AddService<S>
where
    S: Service<Cx, u64, Response = u64>,
{
    type Response = u64;
    type Error = S::Error;

    fn ready(&mut self, cx: &Cx) -> Result<Readiness, Self::Error> {
        self.inner.ready(cx)
    }

    fn call(&mut self, cx: &Cx, request: u64) -> Result<Self::Response, Self::Error> {
        self.inner
            .call(cx, request)
            .map(|response| response + self.value)
    }
}

struct RecordingLayer<'a> {
    name: &'static str,
    events: &'a RefCell<Vec<&'static str>>,
}

impl<'a, S> Layer<S> for RecordingLayer<'a> {
    type Service = RecordingService<'a, S>;

    fn layer(&self, inner: S) -> Self::Service {
        RecordingService {
            inner,
            before: self.name,
            events: self.events,
        }
    }
}

struct RecordingService<'a, S> {
    inner: S,
    before: &'static str,
    events: &'a RefCell<Vec<&'static str>>,
}

impl<Cx, S> Service<Cx, u64> for RecordingService<'_, S>
where
    S: Service<Cx, u64, Response = u64>,
{
    type Response = u64;
    type Error = S::Error;

    fn ready(&mut self, cx: &Cx) -> Result<Readiness, Self::Error> {
        self.inner.ready(cx)
    }

    fn call(&mut self, cx: &Cx, request: u64) -> Result<Self::Response, Self::Error> {
        self.events.borrow_mut().push(self.before);
        let response = self.inner.call(cx, request)?;
        self.events.borrow_mut().push("after");
        Ok(response)
    }
}

struct FixedReadiness {
    readiness: Readiness,
}

impl<Cx> Service<Cx, ()> for FixedReadiness {
    type Response = ();
    type Error = ServiceError;

    fn ready(&mut self, _cx: &Cx) -> Result<Readiness, Self::Error> {
        Ok(self.readiness)
    }

    fn call(&mut self, _cx: &Cx, _request: ()) -> Result<Self::Response, Self::Error> {
        Ok(())
    }
}

#[test]
fn service_fn_and_layers_compose_in_order() {
    let events = RefCell::new(Vec::new());
    let mut service = service_fn(|_: &(), request| Ok::<_, ServiceError>(request * 2))
        .layer(AddLayer { value: 1 })
        .layer(RecordingLayer {
            name: "outer",
            events: &events,
        });

    assert!(service.ready(&()).unwrap().is_ready());
    assert_eq!(service.call(&(), 3).unwrap(), 7);
    assert_eq!(events.into_inner(), vec!["outer", "after"]);
}

#[test]
fn readiness_reports_immediate_not_ready_and_overloaded_states() {
    let mut ready = FixedReadiness {
        readiness: Readiness::Ready,
    };
    let mut not_ready = FixedReadiness {
        readiness: Readiness::NotReady,
    };
    let mut overloaded = FixedReadiness {
        readiness: Readiness::Overloaded,
    };

    assert_eq!(ready.ready(&()).unwrap(), Readiness::Ready);
    assert_eq!(not_ready.ready(&()).unwrap(), Readiness::NotReady);
    assert_eq!(overloaded.ready(&()).unwrap(), Readiness::Overloaded);
}

struct ShortCircuit;

impl<Cx> Service<Cx, u64> for ShortCircuit {
    type Response = u64;
    type Error = ServiceError;

    fn ready(&mut self, _cx: &Cx) -> Result<Readiness, Self::Error> {
        Err(ServiceError::Overloaded)
    }

    fn call(&mut self, _cx: &Cx, _request: u64) -> Result<Self::Response, Self::Error> {
        Err(ServiceError::InvalidRequest("short circuit"))
    }
}

#[test]
fn short_circuit_service_reports_structured_errors() {
    let mut service = ShortCircuit;
    assert!(matches!(service.ready(&()), Err(ServiceError::Overloaded)));
    assert!(matches!(
        service.call(&(), 1),
        Err(ServiceError::InvalidRequest("short circuit"))
    ));
}

struct ShortCircuitLayer;

impl<S> Layer<S> for ShortCircuitLayer {
    type Service = ShortCircuitLayerService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ShortCircuitLayerService { inner }
    }
}

struct ShortCircuitLayerService<S> {
    inner: S,
}

impl<Cx, S> Service<Cx, u64> for ShortCircuitLayerService<S>
where
    S: Service<Cx, u64, Response = u64, Error = ServiceError>,
{
    type Response = u64;
    type Error = ServiceError;

    fn ready(&mut self, cx: &Cx) -> Result<Readiness, Self::Error> {
        self.inner.ready(cx)
    }

    fn call(&mut self, cx: &Cx, request: u64) -> Result<Self::Response, Self::Error> {
        if request == 0 {
            return Err(ServiceError::InvalidRequest("short circuit"));
        }
        self.inner.call(cx, request)
    }
}

#[test]
fn short_circuit_layer_does_not_call_inner_service() {
    let calls = RefCell::new(0);
    let mut service = service_fn(|_: &(), request| {
        *calls.borrow_mut() += 1;
        Ok::<_, ServiceError>(request)
    })
    .layer(ShortCircuitLayer);

    assert!(matches!(
        service.call(&(), 0),
        Err(ServiceError::InvalidRequest("short circuit"))
    ));
    assert_eq!(*calls.borrow(), 0);
    assert_eq!(service.call(&(), 7).unwrap(), 7);
    assert_eq!(*calls.borrow(), 1);
}

#[test]
fn layer_error_preserves_source_context() {
    let source = StaticError("inner failed");
    let error = ServiceError::layer("trace", source);
    assert_eq!(
        error.to_string(),
        "service layer trace failed: inner failed"
    );
    assert_eq!(error.source().unwrap().to_string(), "inner failed");
}

#[test]
fn nested_layer_errors_preserve_source_chain() {
    let inner = ServiceError::layer("auth", StaticError("denied"));
    let outer = ServiceError::layer("trace", inner);

    assert_eq!(
        outer.to_string(),
        "service layer trace failed: service layer auth failed: denied"
    );
    let source = outer.source().unwrap();
    assert_eq!(source.to_string(), "service layer auth failed: denied");
    assert_eq!(source.source().unwrap().to_string(), "denied");
}

#[test]
fn boxed_service_erases_concrete_service_type() {
    let mut service: kimojio_stack_tower::BoxService<(), u64, u64, ServiceError> =
        Box::new(service_fn(|_: &(), request| {
            Ok::<_, ServiceError>(request + 1)
        }));
    assert_eq!(service.call(&(), 41).unwrap(), 42);
}

struct StackSleepReady;

impl<'cx> Service<kimojio_stack::RuntimeContext<'cx>, ()> for StackSleepReady {
    type Response = &'static str;
    type Error = kimojio_stack::Errno;

    fn ready(&mut self, cx: &kimojio_stack::RuntimeContext<'cx>) -> Result<Readiness, Self::Error> {
        cx.sleep(Duration::from_millis(0))?;
        Ok(Readiness::Ready)
    }

    fn call(
        &mut self,
        _cx: &kimojio_stack::RuntimeContext<'cx>,
        _request: (),
    ) -> Result<Self::Response, Self::Error> {
        Ok("stack")
    }
}

struct StealSleepReady;

impl<'cx> Service<kimojio_stack_steal::RuntimeContext<'cx>, ()> for StealSleepReady {
    type Response = &'static str;
    type Error = kimojio_stack::Errno;

    fn ready(
        &mut self,
        cx: &kimojio_stack_steal::RuntimeContext<'cx>,
    ) -> Result<Readiness, Self::Error> {
        cx.sleep(Duration::from_millis(0))?;
        Ok(Readiness::Ready)
    }

    fn call(
        &mut self,
        _cx: &kimojio_stack_steal::RuntimeContext<'cx>,
        _request: (),
    ) -> Result<Self::Response, Self::Error> {
        Ok("steal")
    }
}

#[test]
fn readiness_can_wait_on_stack_runtime() {
    let mut runtime = kimojio_stack::Runtime::new();
    runtime.block_on(|cx| {
        let mut service = StackSleepReady;
        assert_eq!(service.ready(cx).unwrap(), Readiness::Ready);
        assert_eq!(service.call(cx, ()).unwrap(), "stack");
    });
}

#[test]
fn readiness_can_wait_on_steal_runtime() {
    let mut runtime = kimojio_stack_steal::Runtime::new();
    runtime.block_on(|cx| {
        let mut service = StealSleepReady;
        assert_eq!(service.ready(cx).unwrap(), Readiness::Ready);
        assert_eq!(service.call(cx, ()).unwrap(), "steal");
    });
}
