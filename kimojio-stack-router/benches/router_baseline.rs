// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use http::{Method, Request};
use kimojio_stack_http::Body;
use kimojio_stack_router::{
    BodyBytes, PathParams, Rejection, Router, StreamingBody, StreamingChunk, extractor_fn,
    handler_fn,
};
use kimojio_stack_tower::Service;

fn request(path: &str) -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri(path)
        .body(Body::empty())
        .unwrap()
}

fn post(path: &str, body: &'static [u8]) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(path)
        .body(Body::copy_from_slice(body, Default::default()).unwrap())
        .unwrap()
}

fn router<Cx>() -> Router<Cx> {
    let nested = Router::new()
        .route(
            "/items/:id",
            Method::GET,
            extractor_fn::<PathParams, _>(|_: &Cx, path: PathParams| {
                Ok::<_, Rejection>(format!("item {}", path.get("id").unwrap()))
            }),
        )
        .unwrap()
        .route(
            "/echo",
            Method::POST,
            extractor_fn::<BodyBytes, _>(|_: &Cx, body: BodyBytes| {
                Ok::<_, Rejection>(String::from_utf8_lossy(&body.0).into_owned())
            }),
        )
        .unwrap();
    Router::new()
        .route(
            "/direct",
            Method::GET,
            handler_fn(|_: &Cx, _| Ok::<_, Rejection>("direct")),
        )
        .unwrap()
        .route(
            "/stream",
            Method::GET,
            handler_fn(|_: &Cx, _| {
                Ok::<_, Rejection>(StreamingBody::from_chunks(
                    http::StatusCode::OK,
                    vec![StreamingChunk::data("a"), StreamingChunk::finish()],
                ))
            }),
        )
        .unwrap()
        .nest("/api", nested)
        .unwrap()
        .fallback(handler_fn(|_: &Cx, _| Ok::<_, Rejection>("missing")))
}

fn bench_stack(c: &mut Criterion) {
    c.bench_function("router/stack/direct_handler", |b| {
        let mut runtime = kimojio_stack::Runtime::new();
        runtime.block_on(|cx| {
            b.iter(|| {
                black_box("direct");
                cx.yield_now();
            });
        });
    });

    c.bench_function("router/stack/routed_handler", |b| {
        let mut runtime = kimojio_stack::Runtime::new();
        runtime.block_on(|cx| {
            let mut router = router();
            b.iter(|| black_box(router.call(cx, request("/api/items/42")).unwrap()));
        });
    });

    c.bench_function("router/stack/extractor_heavy_handler", |b| {
        let mut runtime = kimojio_stack::Runtime::new();
        runtime.block_on(|cx| {
            let mut router = router();
            b.iter(|| black_box(router.call(cx, post("/api/echo", b"payload")).unwrap()));
        });
    });

    c.bench_function("router/stack/fallback", |b| {
        let mut runtime = kimojio_stack::Runtime::new();
        runtime.block_on(|cx| {
            let mut router = router();
            b.iter(|| black_box(router.call(cx, request("/missing")).unwrap()));
        });
    });

    c.bench_function("router/stack/streaming_route_buffered", |b| {
        let mut runtime = kimojio_stack::Runtime::new();
        runtime.block_on(|cx| {
            let mut router = router();
            b.iter(|| black_box(router.call(cx, request("/stream")).unwrap()));
        });
    });
}

fn bench_steal(c: &mut Criterion) {
    c.bench_function("router/stack-steal/direct_handler", |b| {
        let mut runtime = kimojio_stack_steal::Runtime::new();
        runtime.block_on(|cx| {
            b.iter(|| {
                black_box("direct");
                cx.yield_now();
            });
        });
    });

    c.bench_function("router/stack-steal/routed_handler", |b| {
        let mut runtime = kimojio_stack_steal::Runtime::new();
        runtime.block_on(|cx| {
            let mut router = router();
            b.iter(|| black_box(router.call(cx, request("/api/items/42")).unwrap()));
        });
    });

    c.bench_function("router/stack-steal/extractor_heavy_handler", |b| {
        let mut runtime = kimojio_stack_steal::Runtime::new();
        runtime.block_on(|cx| {
            let mut router = router();
            b.iter(|| black_box(router.call(cx, post("/api/echo", b"payload")).unwrap()));
        });
    });

    c.bench_function("router/stack-steal/fallback", |b| {
        let mut runtime = kimojio_stack_steal::Runtime::new();
        runtime.block_on(|cx| {
            let mut router = router();
            b.iter(|| black_box(router.call(cx, request("/missing")).unwrap()));
        });
    });

    c.bench_function("router/stack-steal/streaming_route_buffered", |b| {
        let mut runtime = kimojio_stack_steal::Runtime::new();
        runtime.block_on(|cx| {
            let mut router = router();
            b.iter(|| black_box(router.call(cx, request("/stream")).unwrap()));
        });
    });

    c.bench_function("router/stack-steal/middleware_ready", |b| {
        let mut runtime = kimojio_stack_steal::Runtime::new();
        runtime.block_on(|cx| {
            let mut router = router();
            b.iter(|| black_box(router.ready(cx).unwrap()));
        });
    });
}

fn router_baseline(c: &mut Criterion) {
    bench_stack(c);
    bench_steal(c);
}

criterion_group!(benches, router_baseline);
criterion_main!(benches);
