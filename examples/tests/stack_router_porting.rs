// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use http::StatusCode;

#[test]
fn stack_router_porting_routes_example_matches_expected_behavior() {
    let health = examples::stack_router_ported_routes::run("/health");
    assert_eq!(health.status(), StatusCode::OK);
    assert_eq!(health.body().as_bytes(), b"ok");

    let path = examples::stack_router_ported_routes::run("/api/users/42");
    assert_eq!(path.body().as_bytes(), b"user 42");

    let query = examples::stack_router_ported_routes::run("/api/query?q=hello");
    assert_eq!(query.body().as_bytes(), b"hello");

    let state = examples::stack_router_ported_routes::run("/api/state");
    assert_eq!(state.body().as_bytes(), b"routes-state");
}

#[test]
fn stack_router_porting_middleware_example_covers_security_stack() {
    let denied = examples::stack_router_ported_middleware::run(false);
    assert_eq!(denied.0, StatusCode::UNAUTHORIZED);

    let allowed = examples::stack_router_ported_middleware::run(true);
    assert_eq!(allowed.0, StatusCode::OK);
    assert!(allowed.1);
    assert_eq!(allowed.2, 2);
}

#[test]
fn stack_router_porting_stateful_example_covers_session_cache_rate_limit() {
    let report = examples::stack_router_ported_stateful::run_report();
    assert_eq!(report.first_body, report.reused_body);
    assert!(report.first_body.starts_with("session-"));
    assert_eq!(report.cached_body, "cache=1");
    assert_eq!(report.rate_limited_status, StatusCode::TOO_MANY_REQUESTS);
}

#[test]
fn stack_router_porting_full_stack_example_covers_sc_002() {
    assert_eq!(
        examples::stack_router_ported_full_stack::run(false),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        examples::stack_router_ported_full_stack::run(true),
        StatusCode::OK
    );
    let report = examples::stack_router_ported_full_stack::run_report();
    assert_eq!(report.unauthorized_status, StatusCode::UNAUTHORIZED);
    assert_eq!(report.first_status, StatusCode::OK);
    assert_eq!(report.cached_status, StatusCode::OK);
    assert_eq!(report.rate_limited_status, StatusCode::TOO_MANY_REQUESTS);
    assert!(report.has_request_id);
    assert!(report.has_stack_header);
    assert!(report.has_session_cookie);
    assert!(report.trace_events >= 6);
}

#[test]
fn stack_router_porting_compatibility_matrix_has_required_schema_and_features() {
    let matrix = include_str!("../../docs/stack-router-tower-compatibility.md");
    for heading in [
        "| Ecosystem | Feature | Classification | Stackful equivalent | Test reference / rationale | Missing or deferred behavior |",
        "Porting taxonomy",
    ] {
        assert!(
            matrix.contains(heading),
            "missing matrix schema section: {heading}"
        );
    }
    for feature in [
        "axum | Router",
        "tower | buffer",
        "tower | spawn-ready",
        "tower-http | compression",
        "tower-http | trace",
        "tower-governor | keyed rate limit",
        "tower-sessions | sessions",
        "tower-http-cache | HTTP cache",
    ] {
        assert!(matrix.contains(feature), "missing feature row: {feature}");
    }
    for classification in [
        "supported",
        "partial",
        "intentionally-different",
        "deferred",
        "unsupported",
    ] {
        assert!(
            matrix.contains(classification),
            "missing classification: {classification}"
        );
    }
    for line in matrix
        .split("Porting taxonomy:")
        .next()
        .unwrap()
        .lines()
        .filter(|line| line.starts_with("| ") && !line.contains("---"))
    {
        if line.starts_with("| Ecosystem ") {
            continue;
        }
        let columns = line
            .trim_matches('|')
            .split('|')
            .map(str::trim)
            .collect::<Vec<_>>();
        assert_eq!(columns.len(), 6, "bad matrix row: {line}");
        for column in &columns {
            assert!(!column.is_empty(), "empty column in row: {line}");
        }
        match columns[2] {
            "supported" | "partial" | "intentionally-different" => {
                assert_ne!(columns[4], "none", "missing test reference: {line}");
            }
            "deferred" | "unsupported" => {
                assert_ne!(columns[5], "none", "missing rationale: {line}");
            }
            other => panic!("unknown classification {other}: {line}"),
        }
    }
}
