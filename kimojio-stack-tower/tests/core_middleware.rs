// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::thread;
use std::time::Duration;

use kimojio_stack_tower::{
    ConcurrencyLimitLayer, FilterLayer, LoadLayer, LoadShedLayer, RateLimitLayer, Readiness,
    RetryLayer, RetryNone, Service, ServiceError, ServiceExt, Steer, TimeoutLayer, service_fn,
};

struct FixedReady {
    readiness: Readiness,
}

impl<Cx> Service<Cx, u64> for FixedReady {
    type Response = u64;
    type Error = ServiceError;

    fn ready(&mut self, _cx: &Cx) -> Result<Readiness, Self::Error> {
        Ok(self.readiness)
    }

    fn call(&mut self, _cx: &Cx, request: u64) -> Result<Self::Response, Self::Error> {
        Ok(request)
    }
}

#[derive(Clone)]
struct OffsetService {
    offset: u64,
}

impl<Cx> Service<Cx, u64> for OffsetService {
    type Response = u64;
    type Error = ServiceError;

    fn call(&mut self, _cx: &Cx, request: u64) -> Result<Self::Response, Self::Error> {
        Ok(request + self.offset)
    }
}

#[derive(Clone)]
struct SelectableReady {
    offset: u64,
    readiness: Readiness,
}

impl<Cx> Service<Cx, u64> for SelectableReady {
    type Response = u64;
    type Error = ServiceError;

    fn ready(&mut self, _cx: &Cx) -> Result<Readiness, Self::Error> {
        Ok(self.readiness)
    }

    fn call(&mut self, _cx: &Cx, request: u64) -> Result<Self::Response, Self::Error> {
        Ok(request + self.offset)
    }
}

#[test]
fn core_middleware_filter_accepts_and_rejects() {
    let mut service = service_fn(|_: &(), request| Ok::<_, ServiceError>(request))
        .layer(FilterLayer::new(|request: &u64| *request > 10));

    assert_eq!(service.call(&(), 11).unwrap(), 11);
    assert!(matches!(
        service.call(&(), 10),
        Err(ServiceError::InvalidRequest(_))
    ));
}

#[test]
fn core_middleware_concurrency_limit_reports_overload_and_releases_capacity() {
    let layer = ConcurrencyLimitLayer::new(1);
    let nested = RefCell::new(Some(
        service_fn(|_: &(), request| Ok::<_, ServiceError>(request)).layer(layer.clone()),
    ));
    let mut first =
        service_fn(|cx: &(), request| nested.borrow_mut().as_mut().unwrap().call(cx, request))
            .layer(layer.clone());
    let mut after_release = FixedReady {
        readiness: Readiness::Ready,
    }
    .layer(layer);

    assert!(matches!(first.call(&(), 7), Err(ServiceError::Inner(_))));
    assert_eq!(after_release.ready(&()).unwrap(), Readiness::Ready);
}

#[test]
fn core_middleware_rate_limit_rejects_after_budget() {
    let mut service =
        service_fn(|_: &(), request| Ok::<_, ServiceError>(request)).layer(RateLimitLayer::new(1));

    assert_eq!(service.call(&(), 1).unwrap(), 1);
    assert!(matches!(service.ready(&()), Ok(Readiness::Overloaded)));
    assert!(matches!(
        service.call(&(), 2),
        Err(ServiceError::Overloaded)
    ));
}

#[test]
fn core_middleware_load_records_success_and_failure() {
    let layer = LoadLayer::new();
    let fail = Cell::new(false);
    let mut service = service_fn(|_: &(), request| {
        if fail.get() {
            Err(ServiceError::InvalidRequest("fail"))
        } else {
            Ok(request)
        }
    })
    .layer(layer.clone());

    assert_eq!(service.call(&(), 1).unwrap(), 1);
    fail.set(true);
    assert!(service.call(&(), 2).is_err());
    let snapshot = layer.snapshot();
    assert_eq!(snapshot.in_flight, 0);
    assert_eq!(snapshot.completed, 1);
    assert_eq!(snapshot.failed, 1);
}

#[test]
fn core_middleware_load_shed_short_circuits_when_not_ready() {
    let mut service = FixedReady {
        readiness: Readiness::NotReady,
    }
    .layer(LoadShedLayer);

    assert_eq!(service.ready(&()).unwrap(), Readiness::NotReady);
    assert!(matches!(
        service.call(&(), 1),
        Err(ServiceError::Overloaded)
    ));
}

#[test]
fn core_middleware_load_shed_forwards_when_ready() {
    let mut service = FixedReady {
        readiness: Readiness::Ready,
    }
    .layer(LoadShedLayer);

    assert_eq!(service.call(&(), 11).unwrap(), 11);
}

#[test]
fn core_middleware_retry_retries_until_success_or_budget_exhausts() {
    let attempts = Cell::new(0);
    let layer = RetryLayer::new(3);
    let mut service = service_fn(|_: &(), request: u64| {
        attempts.set(attempts.get() + 1);
        if attempts.get() < 3 {
            Err(ServiceError::InvalidRequest("retry"))
        } else {
            Ok(request)
        }
    })
    .layer(layer.clone());

    assert_eq!(service.call(&(), 9).unwrap(), 9);
    assert_eq!(attempts.get(), 3);
    assert_eq!(layer.attempts(), 3);

    let mut exhausted =
        service_fn(|_: &(), _request: u64| Err::<u64, _>(ServiceError::InvalidRequest("retry")))
            .layer(RetryLayer::new(2));
    assert!(matches!(
        exhausted.call(&(), 1),
        Err(ServiceError::Inner(_))
    ));

    let no_retry_attempts = Cell::new(0);
    let no_retry_layer = RetryLayer::with_policy(3, RetryNone);
    let mut no_retry = service_fn(|_: &(), _request: u64| {
        no_retry_attempts.set(no_retry_attempts.get() + 1);
        Err::<u64, _>(ServiceError::InvalidRequest("no retry"))
    })
    .layer(no_retry_layer.clone());
    assert!(matches!(no_retry.call(&(), 1), Err(ServiceError::Inner(_))));
    assert_eq!(no_retry_attempts.get(), 1);
    assert_eq!(no_retry_layer.attempts(), 1);
}

#[test]
fn core_middleware_steer_routes_to_selected_service() {
    let services = vec![OffsetService { offset: 10 }, OffsetService { offset: 20 }];
    let mut service = Steer::new(services, |request: &u64, _len| (*request % 2) as usize);

    assert_eq!(service.call(&(), 2).unwrap(), 12);
    assert_eq!(service.call(&(), 3).unwrap(), 23);
}

#[test]
fn core_middleware_steer_rechecks_selected_service_readiness() {
    let services = vec![
        SelectableReady {
            offset: 10,
            readiness: Readiness::Ready,
        },
        SelectableReady {
            offset: 20,
            readiness: Readiness::NotReady,
        },
    ];
    let mut service = Steer::new(services, |request: &u64, _len| (*request % 2) as usize);

    assert_eq!(service.ready(&()).unwrap(), Readiness::Ready);
    assert!(matches!(
        service.call(&(), 1),
        Err(ServiceError::Overloaded)
    ));
    assert_eq!(service.call(&(), 2).unwrap(), 12);

    let empty: Result<Steer<OffsetService, _>, _> =
        Steer::try_new(Vec::new(), |_: &u64, _: usize| 0);
    assert!(matches!(empty, Err(ServiceError::InvalidRequest(_))));
}

#[test]
fn core_middleware_timeout_reports_slow_call() {
    let dropped = Rc::new(Cell::new(false));
    struct DropMarker(Rc<Cell<bool>>);
    impl Drop for DropMarker {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }
    let marker = Rc::clone(&dropped);
    let mut service = service_fn(|_: &(), request| {
        let _marker = DropMarker(Rc::clone(&marker));
        thread::sleep(Duration::from_millis(5));
        Ok::<_, ServiceError>(request)
    })
    .layer(TimeoutLayer::new(Duration::from_millis(1)));

    assert!(matches!(service.call(&(), 1), Err(ServiceError::Timeout)));
    assert!(dropped.get());
}

#[test]
fn core_middleware_timeout_allows_fast_call() {
    let mut service = service_fn(|_: &(), request| Ok::<_, ServiceError>(request))
        .layer(TimeoutLayer::new(Duration::from_secs(1)));

    assert_eq!(service.call(&(), 5).unwrap(), 5);
}
