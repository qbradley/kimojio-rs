// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::hint::black_box;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use http::{HeaderName, HeaderValue, Method, Request, Response};
use kimojio_stack_http::Body;
use kimojio_stack_tower::http::{
    AuthLayer, CacheLayer, GovernorLayer, MemoryCacheStore, MemorySessionStore, RequestIdLayer,
    SessionLayer, SetHeaderLayer, TraceLayer,
};
use kimojio_stack_tower::{
    BalanceLayer, BufferLayer, ConcurrencyLimitLayer, FilterLayer, HedgeLayer, LoadShedLayer,
    Readiness, ReconnectLayer, RetryLayer, Service, ServiceError, ServiceExt, SpawnReadyLayer,
    StaticDiscover, TimeoutLayer, service_fn,
};

#[derive(Clone)]
struct BenchReady {
    offset: u64,
}

impl<Cx> Service<Cx, Request<Body>> for BenchReady {
    type Response = Response<Body>;
    type Error = ServiceError;

    fn ready(&mut self, _cx: &Cx) -> Result<Readiness, Self::Error> {
        Ok(Readiness::Ready)
    }

    fn call(&mut self, _cx: &Cx, _request: Request<Body>) -> Result<Self::Response, Self::Error> {
        Ok(Response::new(
            Body::copy_from_slice(self.offset.to_string().as_bytes(), Default::default()).unwrap(),
        ))
    }
}

fn request() -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri("/bench")
        .header("authorization", "token")
        .body(Body::empty())
        .unwrap()
}

fn base_service() -> impl Service<(), Request<Body>, Response = Response<Body>, Error = ServiceError>
{
    service_fn(|_: &(), _request: Request<Body>| {
        Ok::<_, ServiceError>(Response::new(
            Body::copy_from_slice(b"ok", Default::default()).unwrap(),
        ))
    })
}

fn middleware_baseline(c: &mut Criterion) {
    c.bench_function("tower/direct_service", |b| {
        let mut service = base_service();
        b.iter(|| black_box(service.call(&(), request()).unwrap()));
    });

    c.bench_function("tower/shallow_layer_stack", |b| {
        let mut service = base_service()
            .layer(FilterLayer::new(|_: &Request<Body>| true))
            .layer(SetHeaderLayer::response(
                HeaderName::from_static("x-layer"),
                HeaderValue::from_static("yes"),
            ));
        b.iter(|| black_box(service.call(&(), request()).unwrap()));
    });

    c.bench_function("tower/middleware_heavy_stack", |b| {
        let trace = TraceLayer::new();
        let mut service = base_service()
            .layer(SetHeaderLayer::response(
                HeaderName::from_static("x-stack"),
                HeaderValue::from_static("router"),
            ))
            .layer(RequestIdLayer::new(HeaderName::from_static("x-request-id")))
            .layer(SessionLayer::new(MemorySessionStore::new()))
            .layer(CacheLayer::new(MemoryCacheStore::new(64)))
            .layer(GovernorLayer::new(4096, |request: &Request<Body>| {
                request.uri().path().to_owned()
            }))
            .layer(AuthLayer::new(|request: &Request<Body>| {
                request.headers().contains_key("authorization")
            }))
            .layer(trace)
            .layer(RetryLayer::new(1))
            .layer(ConcurrencyLimitLayer::new(1024))
            .layer(TimeoutLayer::new(Duration::from_secs(1)))
            .layer(LoadShedLayer);
        b.iter(|| black_box(service.call(&(), request()).unwrap()));
    });

    c.bench_function("tower/timeout_retry_limit", |b| {
        let mut service = base_service()
            .layer(RetryLayer::new(2))
            .layer(ConcurrencyLimitLayer::new(128))
            .layer(TimeoutLayer::new(Duration::from_secs(1)));
        b.iter(|| black_box(service.call(&(), request()).unwrap()));
    });

    c.bench_function("tower/session_cache_governor", |b| {
        let mut service = base_service()
            .layer(SessionLayer::new(MemorySessionStore::new()))
            .layer(CacheLayer::new(MemoryCacheStore::new(64)))
            .layer(GovernorLayer::new(4096, |request: &Request<Body>| {
                request.uri().path().to_owned()
            }));
        b.iter(|| black_box(service.call(&(), request()).unwrap()));
    });

    c.bench_function("tower/dynamic_background_stack", |b| {
        let mut service = BenchReady { offset: 1 }
            .layer(BufferLayer::new(128))
            .layer(SpawnReadyLayer::new())
            .layer(HedgeLayer::new(Duration::from_secs(1)));
        b.iter(|| black_box(service.call(&(), request()).unwrap()));
    });

    c.bench_function("tower/discover_balance", |b| {
        let discover =
            StaticDiscover::new(vec![BenchReady { offset: 1 }, BenchReady { offset: 2 }]);
        let mut service = base_service().layer(BalanceLayer::new(discover));
        b.iter(|| black_box(service.call(&(), request()).unwrap()));
    });

    c.bench_function("tower/reconnect", |b| {
        let mut service =
            BenchReady { offset: 1 }.layer(ReconnectLayer::new(|| BenchReady { offset: 1 }));
        b.iter(|| black_box(service.call(&(), request()).unwrap()));
    });
}

criterion_group!(benches, middleware_baseline);
criterion_main!(benches);
