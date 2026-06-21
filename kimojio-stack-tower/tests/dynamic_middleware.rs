// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::thread;
use std::time::Duration;

use kimojio_stack_tower::{
    BalanceLayer, BufferLayer, DynamicDiscover, HedgeLayer, Readiness, ReconnectLayer, Service,
    ServiceError, ServiceExt, SpawnReadyLayer, StaticDiscover, service_fn,
};

#[derive(Clone)]
struct ReadyOffset {
    offset: u64,
    ready: Readiness,
}

impl<Cx> Service<Cx, u64> for ReadyOffset {
    type Response = u64;
    type Error = ServiceError;

    fn ready(&mut self, _cx: &Cx) -> Result<Readiness, Self::Error> {
        Ok(self.ready)
    }

    fn call(&mut self, _cx: &Cx, request: u64) -> Result<Self::Response, Self::Error> {
        Ok(request + self.offset)
    }
}

#[test]
fn dynamic_middleware_buffer_saturates_and_releases() {
    let layer = BufferLayer::new(1);
    let nested = RefCell::new(Some(
        service_fn(|_: &(), request| Ok::<_, ServiceError>(request)).layer(layer.clone()),
    ));
    let mut service =
        service_fn(|cx: &(), request| nested.borrow_mut().as_mut().unwrap().call(cx, request))
            .layer(layer.clone());

    assert!(matches!(service.call(&(), 1), Err(ServiceError::Inner(_))));
    let mut after_release =
        service_fn(|_: &(), request| Ok::<_, ServiceError>(request)).layer(layer);
    assert_eq!(after_release.call(&(), 2).unwrap(), 2);
}

#[test]
fn dynamic_middleware_spawn_ready_drives_readiness_before_call() {
    let layer = SpawnReadyLayer::new();
    let mut service = ReadyOffset {
        offset: 1,
        ready: Readiness::Ready,
    }
    .layer(layer.clone());

    assert_eq!(service.call(&(), 4).unwrap(), 5);
    assert_eq!(layer.drives(), 1);
}

#[test]
fn dynamic_middleware_spawn_ready_sheds_when_not_ready() {
    let mut service = ReadyOffset {
        offset: 1,
        ready: Readiness::NotReady,
    }
    .layer(SpawnReadyLayer::new());

    assert!(matches!(
        service.call(&(), 4),
        Err(ServiceError::Overloaded)
    ));
}

#[test]
fn dynamic_middleware_hedge_reissues_slow_or_failed_request() {
    let attempts = Cell::new(0);
    let mut service = service_fn(|_: &(), request: u64| {
        attempts.set(attempts.get() + 1);
        if attempts.get() == 1 {
            thread::sleep(Duration::from_millis(3));
            Ok::<_, ServiceError>(request + 1)
        } else {
            Ok::<_, ServiceError>(request + 10)
        }
    })
    .layer(HedgeLayer::new(Duration::from_millis(1)));

    assert_eq!(service.call(&(), 5).unwrap(), 15);
    assert_eq!(attempts.get(), 2);

    let failures = Cell::new(0);
    let mut failed_first = service_fn(|_: &(), request: u64| {
        failures.set(failures.get() + 1);
        if failures.get() == 1 {
            Err(ServiceError::InvalidRequest("first"))
        } else {
            Ok(request)
        }
    })
    .layer(HedgeLayer::new(Duration::from_secs(1)));
    assert_eq!(failed_first.call(&(), 8).unwrap(), 8);

    let mut final_failure =
        service_fn(|_: &(), _request: u64| Err::<u64, _>(ServiceError::InvalidRequest("failed")))
            .layer(HedgeLayer::new(Duration::from_secs(1)));
    assert!(matches!(
        final_failure.call(&(), 8),
        Err(ServiceError::Inner(_))
    ));
}

#[test]
fn dynamic_middleware_discover_and_balance_route_to_ready_backends() {
    let discover = StaticDiscover::new(vec![
        ReadyOffset {
            offset: 10,
            ready: Readiness::NotReady,
        },
        ReadyOffset {
            offset: 20,
            ready: Readiness::Ready,
        },
    ]);
    let mut service = service_fn(|_: &(), request: u64| Ok::<_, ServiceError>(request))
        .layer(BalanceLayer::new(discover));

    assert_eq!(service.ready(&()).unwrap(), Readiness::Ready);
    assert_eq!(service.call(&(), 1).unwrap(), 21);

    let empty = StaticDiscover::<ReadyOffset>::new(Vec::new());
    let mut empty_service = service_fn(|_: &(), request: u64| Ok::<_, ServiceError>(request))
        .layer(BalanceLayer::new(empty));
    assert!(matches!(
        empty_service.call(&(), 1),
        Err(ServiceError::Overloaded)
    ));
}

#[test]
fn dynamic_middleware_balance_distributes_and_observes_changing_discovery() {
    let discover = DynamicDiscover::new(vec![
        ReadyOffset {
            offset: 100,
            ready: Readiness::Ready,
        },
        ReadyOffset {
            offset: 200,
            ready: Readiness::Ready,
        },
    ]);
    let mut service = service_fn(|_: &(), request: u64| Ok::<_, ServiceError>(request))
        .layer(BalanceLayer::new(discover.clone()));

    assert_eq!(service.call(&(), 1).unwrap(), 101);
    assert_eq!(service.call(&(), 1).unwrap(), 201);

    discover.replace(vec![ReadyOffset {
        offset: 300,
        ready: Readiness::Ready,
    }]);
    assert_eq!(service.call(&(), 1).unwrap(), 301);
}

#[derive(Clone)]
struct FailsOnce {
    failed: Rc<Cell<bool>>,
}

impl<Cx> Service<Cx, u64> for FailsOnce {
    type Response = u64;
    type Error = ServiceError;

    fn call(&mut self, _cx: &Cx, request: u64) -> Result<Self::Response, Self::Error> {
        if !self.failed.replace(true) {
            Err(ServiceError::InvalidRequest("disconnect"))
        } else {
            Ok(request)
        }
    }
}

#[test]
fn dynamic_middleware_reconnect_recreates_after_failure() {
    let created = Rc::new(Cell::new(0));
    let created_factory = Rc::clone(&created);
    let factory = move || {
        created_factory.set(created_factory.get() + 1);
        FailsOnce {
            failed: Rc::new(Cell::new(true)),
        }
    };
    let mut service = FailsOnce {
        failed: Rc::new(Cell::new(false)),
    }
    .layer(ReconnectLayer::new(factory));

    assert_eq!(service.call(&(), 42).unwrap(), 42);
    assert_eq!(created.get(), 1);
}

#[test]
fn dynamic_middleware_reconnect_reports_failure_after_recreated_service_fails() {
    let factory = || FailsOnce {
        failed: Rc::new(Cell::new(false)),
    };
    let mut service = FailsOnce {
        failed: Rc::new(Cell::new(false)),
    }
    .layer(ReconnectLayer::new(factory));

    assert!(matches!(service.call(&(), 42), Err(ServiceError::Inner(_))));
}

#[test]
fn dynamic_middleware_cleanup_runs_after_buffer_rejection_path() {
    let dropped = Rc::new(Cell::new(false));
    struct DropFlag(Rc<Cell<bool>>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }

    let layer = BufferLayer::new(1);
    let dropped_inner = Rc::clone(&dropped);
    let nested = RefCell::new(Some(
        service_fn(move |_: &(), request| {
            let _flag = DropFlag(Rc::clone(&dropped_inner));
            Ok::<_, ServiceError>(request)
        })
        .layer(layer.clone()),
    ));
    let mut service =
        service_fn(|cx: &(), request| nested.borrow_mut().as_mut().unwrap().call(cx, request))
            .layer(layer);

    assert!(service.call(&(), 1).is_err());
    assert!(!dropped.get());
}
