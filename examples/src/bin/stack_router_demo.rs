// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::Result;
use clap::Parser;
use http::{Method, Request, StatusCode};
use kimojio_stack_http::Body;
use kimojio_stack_router::{
    PathParams, QueryParams, Rejection, Router, State, extractor_fn, handler_fn,
};
use kimojio_stack_tower::Service;

#[derive(Debug, Parser)]
#[command(version, about = "Run the stack router demo request")]
struct Args {
    #[arg(long, default_value = "/api/users/42?verbose=true")]
    uri: String,
}

#[derive(Debug)]
struct DemoState {
    service_name: &'static str,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let mut router = demo_router();
    let request = Request::builder()
        .method(Method::GET)
        .uri(args.uri)
        .body(Body::empty())?;
    let response = router
        .call(&(), request)
        .unwrap_or_else(Rejection::into_response);
    println!(
        "stack-router-demo status={} body={}",
        response.status().as_u16(),
        String::from_utf8_lossy(response.body().as_bytes())
    );
    Ok(())
}

fn demo_router() -> Router<()> {
    let api = Router::new()
        .route(
            "/users/:id",
            Method::GET,
            extractor_fn::<PathParams, _>(|_: &(), path: PathParams| {
                Ok::<_, Rejection>(format!("user {}", path.get("id").unwrap()))
            }),
        )
        .unwrap()
        .route(
            "/state",
            Method::GET,
            extractor_fn::<State<DemoState>, _>(|_: &(), state: State<DemoState>| {
                Ok::<_, Rejection>(format!("state {}", state.0.service_name))
            }),
        )
        .unwrap()
        .route(
            "/query",
            Method::GET,
            extractor_fn::<QueryParams, _>(|_: &(), query: QueryParams| {
                Ok::<_, Rejection>(query.get("q").unwrap_or("missing").to_owned())
            }),
        )
        .unwrap();

    Router::new()
        .route(
            "/health",
            Method::GET,
            handler_fn(|_: &(), _| Ok::<_, Rejection>("ok")),
        )
        .unwrap()
        .nest("/api", api)
        .unwrap()
        .fallback(handler_fn(|_: &(), _| {
            Ok::<_, Rejection>((StatusCode::NOT_FOUND, "missing"))
        }))
        .with_state(DemoState {
            service_name: "stack-router",
        })
}
