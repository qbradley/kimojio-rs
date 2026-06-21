// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::{Parser, ValueEnum};
use http::{HeaderName, HeaderValue, Method, Request, Response, StatusCode};
use kimojio_stack_http::Body;
use kimojio_stack_router::{PathParams, Rejection, Router, extractor_fn, handler_fn};
use kimojio_stack_tower::http::{
    AuthLayer, CacheLayer, GovernorLayer, MemoryCacheStore, MemorySessionStore, RequestIdLayer,
    SessionLayer, SetHeaderLayer, TraceLayer,
};
use kimojio_stack_tower::{
    BufferLayer, ConcurrencyLimitLayer, LoadShedLayer, Readiness, RetryLayer, Service,
    ServiceError, ServiceExt, TimeoutLayer, service_fn,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum RuntimeKind {
    Stack,
    StackSteal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum WorkloadKind {
    Routed,
    MiddlewareHeavy,
    Failure,
}

#[derive(Debug, Parser)]
#[command(version, about = "Run stack router workload scenarios")]
struct Args {
    #[arg(long, value_enum, default_value_t = RuntimeKind::Stack)]
    runtime: RuntimeKind,
    #[arg(long, value_enum, default_value_t = WorkloadKind::Routed)]
    workload: WorkloadKind,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let summary = match args.runtime {
        RuntimeKind::Stack => {
            let mut runtime = kimojio_stack::Runtime::new();
            runtime.block_on(|cx| run_workload("stack", args.workload, cx))
        }
        RuntimeKind::StackSteal => {
            let mut runtime = kimojio_stack_steal::Runtime::new();
            runtime.block_on(|cx| run_workload("stack-steal", args.workload, cx))
        }
    };
    println!("{}", summary.line());
    Ok(())
}

fn run_workload<Cx>(runtime: &'static str, workload: WorkloadKind, cx: &Cx) -> Summary {
    let started = Instant::now();
    let mut latencies = Vec::new();
    let mut success = 0usize;
    let mut failure = 0usize;
    let mut timeouts = 0usize;
    let mut load_shed = 0usize;
    let mut retries = 0usize;
    let mut buffer_rejects = 0usize;
    let mut cancellations = 0usize;

    for index in 0..64 {
        let before = Instant::now();
        let status = match workload {
            WorkloadKind::Routed => routed(cx, index),
            WorkloadKind::MiddlewareHeavy => middleware_heavy(index),
            WorkloadKind::Failure => {
                let outcome = failure_case(index);
                timeouts += outcome.timeouts;
                load_shed += outcome.load_shed;
                retries += outcome.retries;
                buffer_rejects += outcome.buffer_rejects;
                cancellations += outcome.cancellations;
                outcome.status
            }
        };
        latencies.push(before.elapsed());
        if status.is_success() {
            success += 1;
        } else {
            failure += 1;
        }
    }
    latencies.sort_unstable();
    Summary {
        runtime,
        workload,
        requests: latencies.len(),
        success,
        failure,
        throughput: latencies.len() as f64 / started.elapsed().as_secs_f64().max(0.000_001),
        p50_us: percentile(&latencies, 50),
        p95_us: percentile(&latencies, 95),
        p99_us: percentile(&latencies, 99),
        timeouts,
        retries,
        load_shed,
        buffer_rejects,
        cancellations,
    }
}

fn routed<Cx>(cx: &Cx, index: usize) -> StatusCode {
    let mut router = Router::new()
        .route(
            "/items/:id",
            Method::GET,
            extractor_fn::<PathParams, _>(|_: &Cx, path: PathParams| {
                Ok::<_, Rejection>(format!("item {}", path.get("id").unwrap()))
            }),
        )
        .unwrap()
        .fallback(handler_fn(|_: &Cx, _| {
            Ok::<_, Rejection>((StatusCode::NOT_FOUND, "missing"))
        }));
    router
        .call(
            cx,
            Request::builder()
                .method(Method::GET)
                .uri(format!("/items/{index}"))
                .body(Body::empty())
                .unwrap(),
        )
        .map(|response| response.status())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}

fn middleware_heavy(index: usize) -> StatusCode {
    let trace = TraceLayer::new();
    let mut service = service_fn(|_: &(), _request: Request<Body>| {
        Ok::<_, ServiceError>(Response::new(Body::empty()))
    })
    .layer(SetHeaderLayer::response(
        HeaderName::from_static("x-stack"),
        HeaderValue::from_static("router"),
    ))
    .layer(RequestIdLayer::new(HeaderName::from_static("x-request-id")))
    .layer(SessionLayer::new(MemorySessionStore::new()))
    .layer(CacheLayer::new(MemoryCacheStore::new(128)))
    .layer(GovernorLayer::new(1024, |request: &Request<Body>| {
        request.uri().path().to_owned()
    }))
    .layer(AuthLayer::new(|request: &Request<Body>| {
        request.headers().contains_key("authorization")
    }))
    .layer(trace)
    .layer(RetryLayer::new(1))
    .layer(ConcurrencyLimitLayer::new(128))
    .layer(TimeoutLayer::new(Duration::from_secs(1)))
    .layer(LoadShedLayer);
    let mut request = Request::builder()
        .method(Method::GET)
        .uri(format!("/heavy/{index}"))
        .body(Body::empty())
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", HeaderValue::from_static("token"));
    service
        .call(&(), request)
        .map(|response| response.status())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}

fn failure_case(index: usize) -> FailureOutcome {
    if index.is_multiple_of(4) {
        let mut service = service_fn(|_: &(), _request: Request<Body>| {
            std::thread::sleep(Duration::from_millis(2));
            Ok::<_, ServiceError>(Response::new(Body::empty()))
        })
        .layer(TimeoutLayer::new(Duration::from_millis(1)));
        let status = match service.call(&(), request_for("/timeout")) {
            Err(ServiceError::Timeout) => StatusCode::REQUEST_TIMEOUT,
            _ => StatusCode::OK,
        };
        return FailureOutcome {
            status,
            timeouts: usize::from(status == StatusCode::REQUEST_TIMEOUT),
            retries: 0,
            load_shed: 0,
            buffer_rejects: 0,
            cancellations: 1,
        };
    }
    if index % 4 == 1 {
        let mut service = NotReadyService.layer(LoadShedLayer);
        let status = match service.call(&(), request_for("/load-shed")) {
            Err(ServiceError::Overloaded) => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::OK,
        };
        return FailureOutcome {
            status,
            timeouts: 0,
            retries: 0,
            load_shed: 1,
            buffer_rejects: 0,
            cancellations: 0,
        };
    }
    if index % 4 == 2 {
        let layer = BufferLayer::new(1);
        let mut competing = service_fn(|_: &(), _request: Request<Body>| {
            Ok::<_, ServiceError>(Response::new(Body::empty()))
        })
        .layer(layer.clone());
        let mut service = service_fn(move |_: &(), _request: Request<Body>| {
            competing.call(&(), request_for("/buffer-competing"))
        })
        .layer(layer);
        let status = if service.call(&(), request_for("/buffer")).is_err() {
            StatusCode::SERVICE_UNAVAILABLE
        } else {
            StatusCode::OK
        };
        return FailureOutcome {
            status,
            timeouts: 0,
            retries: 0,
            load_shed: 0,
            buffer_rejects: 1,
            cancellations: 0,
        };
    }

    struct NotReadyService;

    impl Service<(), Request<Body>> for NotReadyService {
        type Response = Response<Body>;
        type Error = ServiceError;

        fn ready(&mut self, _cx: &()) -> Result<Readiness, Self::Error> {
            Ok(Readiness::NotReady)
        }

        fn call(
            &mut self,
            _cx: &(),
            _request: Request<Body>,
        ) -> Result<Self::Response, Self::Error> {
            Ok(Response::new(Body::empty()))
        }
    }

    let attempts = Rc::new(Cell::new(0usize));
    let attempts_inner = Rc::clone(&attempts);
    let mut service = service_fn(move |_: &(), _request: Request<Body>| {
        attempts_inner.set(attempts_inner.get() + 1);
        if attempts_inner.get() == 1 {
            Err(ServiceError::InvalidRequest("retry"))
        } else {
            Ok(Response::new(Body::empty()))
        }
    })
    .layer(RetryLayer::new(2));
    let status = match service.call(&(), request_for("/retry")) {
        Ok(_) => StatusCode::OK,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    FailureOutcome {
        status,
        timeouts: 0,
        retries: attempts.get().saturating_sub(1),
        load_shed: 0,
        buffer_rejects: 0,
        cancellations: 0,
    }
}

fn request_for(path: &str) -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri(path)
        .body(Body::empty())
        .unwrap()
}

struct FailureOutcome {
    status: StatusCode,
    timeouts: usize,
    retries: usize,
    load_shed: usize,
    buffer_rejects: usize,
    cancellations: usize,
}

fn percentile(latencies: &[Duration], percentile: usize) -> u128 {
    let index = ((latencies.len() - 1) * percentile) / 100;
    latencies[index].as_micros()
}

struct Summary {
    runtime: &'static str,
    workload: WorkloadKind,
    requests: usize,
    success: usize,
    failure: usize,
    throughput: f64,
    p50_us: u128,
    p95_us: u128,
    p99_us: u128,
    timeouts: usize,
    retries: usize,
    load_shed: usize,
    buffer_rejects: usize,
    cancellations: usize,
}

impl Summary {
    fn line(&self) -> String {
        format!(
            "stack-router-workload runtime={} workload={:?} requests={} success={} failure={} throughput_per_sec={:.3} p50_us={} p95_us={} p99_us={} timeout_count={} retry_count={} load_shed_count={} buffer_reject_count={} cancellation_cleanup_count={}",
            self.runtime,
            self.workload,
            self.requests,
            self.success,
            self.failure,
            self.throughput,
            self.p50_us,
            self.p95_us,
            self.p99_us,
            self.timeouts,
            self.retries,
            self.load_shed,
            self.buffer_rejects,
            self.cancellations
        )
    }
}
