# Stack Router/Tower Compatibility Matrix

This matrix tracks the stackful equivalents for axum, tower, tower-http,
tower-governor, tower-sessions, and tower-http-cache concepts.

Classification values:

- `supported`: implemented with positive and negative tests.
- `partial`: implemented with documented missing behavior and at least one positive test.
- `intentionally-different`: stackful behavior differs by design and has replacement tests.
- `deferred`: planned future work with rationale.
- `unsupported`: not planned for the current scope with rationale.

| Ecosystem | Feature | Classification | Stackful equivalent | Test reference / rationale | Missing or deferred behavior |
|-----------|---------|----------------|---------------------|----------------------------|------------------------------|
| axum | Router | supported | `kimojio_stack_router::Router` | `kimojio-stack-router/tests/router_core.rs` | none |
| axum | method routing | supported | `Router::route`, `MethodRouter` | `router_core_method_router_adds_multiple_methods_for_one_path` | none |
| axum | nested routers | supported | `Router::nest` | `router_core_nested_routers_merge_and_fallbacks_work` | none |
| axum | router merging | supported | `Router::merge` | `router_core_nested_routers_merge_and_fallbacks_work` | none |
| axum | path params | supported | `PathParams` extractor | `router_core_path_query_header_body_state_and_extension_extractors_work` | tuple/serde extraction deferred |
| axum | query extraction | supported | `QueryParams` extractor | `router_core_path_query_header_body_state_and_extension_extractors_work` | typed serde extraction deferred |
| axum | header extraction | supported | `HeaderMap` extractor | `router_core_path_query_header_body_state_and_extension_extractors_work` | typed header extractors deferred |
| axum | body extraction | supported | `BodyBytes` extractor | `router_core_path_query_header_body_state_and_extension_extractors_work` | streaming body extraction is HTTP/2-specific |
| axum | state extraction | supported | `State<T>` extractor | `router_core_path_query_header_body_state_and_extension_extractors_work` | none |
| axum | extensions | supported | `Extension<T>` extractor | `router_core_path_query_header_body_state_and_extension_extractors_work` | none |
| axum | handler conversion | supported | `handler_fn`, `extractor_fn` | `router_core_method_and_path_routes_dispatch_to_handlers`, `router_core_path_query_header_body_state_and_extension_extractors_work` | multi-extractor tuple handlers deferred |
| axum | typed responses | supported | `IntoResponse` | `router_core_method_and_path_routes_dispatch_to_handlers` | broad tuple/status/header response forms deferred |
| axum | rejection/error responses | supported | `Rejection` | `router_core_method_not_allowed_and_extractor_failures_are_rejections` | custom rejection mappers deferred |
| axum | fallback | supported | `Router::fallback` | `router_core_nested_routers_merge_and_fallbacks_work` | none |
| axum | extract/response macros | deferred | manual extractor and handler functions | no proc-macro support in first compatibility version | derive/route macros deferred |
| tower | Service | supported | `kimojio_stack_tower::Service` | `kimojio-stack-tower/tests/service_layer.rs` | async `poll_ready` intentionally different |
| tower | Layer | supported | `kimojio_stack_tower::Layer` | `service_fn_and_layers_compose_in_order` | none |
| tower | filter | supported | `FilterLayer` | `core_middleware_filter_accepts_and_rejects` | none |
| tower | limit | supported | `ConcurrencyLimitLayer`, `RateLimitLayer` | `core_middleware_concurrency_limit_reports_overload_and_releases_capacity`, `core_middleware_rate_limit_rejects_after_budget` | time-window rate limiting deferred to governor-style middleware |
| tower | load | supported | `LoadLayer` | `core_middleware_load_records_success_and_failure` | none |
| tower | load-shed | supported | `LoadShedLayer` | `core_middleware_load_shed_short_circuits_when_not_ready` | none |
| tower | retry | supported | `RetryLayer`, `RetryPolicy` | `core_middleware_retry_retries_until_success_or_budget_exhausts` | backoff policies deferred |
| tower | steer | supported | `Steer` | `core_middleware_steer_routes_to_selected_service` | none |
| tower | timeout | intentionally-different | `TimeoutLayer` cooperative/post-call timeout | `core_middleware_timeout_reports_slow_call` | preemptive cancellation deferred |
| tower | buffer | partial | `BufferLayer` cooperative bounded in-flight budget | `dynamic_middleware_buffer_saturates_and_releases` | true background queue worker deferred |
| tower | discover | supported | `Discover`, `StaticDiscover`, `DynamicDiscover` | `dynamic_middleware_balance_distributes_and_observes_changing_discovery` | external discovery backends deferred |
| tower | balance | supported | `BalanceLayer` | `dynamic_middleware_discover_and_balance_route_to_ready_backends` | advanced load algorithms deferred |
| tower | hedge | intentionally-different | `HedgeLayer` cooperative bounded follow-up | `dynamic_middleware_hedge_reissues_slow_or_failed_request` | concurrent backup attempt deferred |
| tower | reconnect | supported | `ReconnectLayer` | `dynamic_middleware_reconnect_recreates_after_failure` | endpoint-specific connectors deferred |
| tower | spawn-ready | intentionally-different | `SpawnReadyLayer` cooperative readiness drive | `dynamic_middleware_spawn_ready_drives_readiness_before_call` | detached background readiness loop deferred |
| tower-http | add-extension | supported | `AddExtensionLayer` | `http_middleware_add_extension_and_validate_request` | none |
| tower-http | auth | supported | `AuthLayer` | `http_middleware_auth_cors_and_csrf_cover_default_and_relaxed_paths` | app-specific auth backends left to users |
| tower-http | catch-panic | supported | `CatchPanicLayer` | `http_middleware_sensitive_headers_and_catch_panic` | fatal process errors are not caught |
| tower-http | compression | supported | `CompressionLayer` | `stateful_body_middleware_compresses_and_decompresses_gzip` | streaming compression deferred |
| tower-http | cors | supported | `CorsLayer` | `http_middleware_auth_cors_and_csrf_cover_default_and_relaxed_paths` | advanced vary/private-network handling deferred |
| tower-http | csrf | supported | `CsrfLayer` | `http_middleware_auth_cors_and_csrf_cover_default_and_relaxed_paths` | app-specific token stores deferred |
| tower-http | decompression | supported | `DecompressionLayer` | `stateful_body_middleware_compresses_and_decompresses_gzip` | streaming decompression deferred |
| tower-http | follow-redirect | supported | `FollowRedirectLayer` | `stateful_body_middleware_follow_redirect_obeys_bounds` | redirect policy customization deferred |
| tower-http | normalize-path | supported | `NormalizePathLayer` | `http_middleware_headers_status_request_id_trace_and_normalize_path` | advanced slash policies deferred |
| tower-http | propagate-header | supported | `PropagateHeaderLayer` | `http_middleware_headers_status_request_id_trace_and_normalize_path` | none |
| tower-http | request-id | supported | `RequestIdLayer` | `http_middleware_headers_status_request_id_trace_and_normalize_path` | custom ID generators deferred |
| tower-http | sensitive-headers | supported | `SensitiveHeadersLayer` | `http_middleware_sensitive_headers_and_catch_panic` | none |
| tower-http | set-header | supported | `SetHeaderLayer` | `http_middleware_headers_status_request_id_trace_and_normalize_path` | append/conditional modes deferred |
| tower-http | set-status | supported | `SetStatusLayer` | `http_middleware_headers_status_request_id_trace_and_normalize_path` | none |
| tower-http | timeout | intentionally-different | `HttpTimeoutLayer` alias to cooperative timeout | `http_middleware_timeout_alias_reports_slow_call` | preemptive cancellation deferred |
| tower-http | trace | supported | `TraceLayer`, `TraceRecord` | `http_middleware_headers_status_request_id_trace_and_normalize_path` | external logging integration deferred |
| tower-http | validate-request | supported | `ValidateRequestLayer` | `http_middleware_add_extension_and_validate_request` | schema validators left to users |
| tower-governor | keyed rate limit | supported | `GovernorLayer` | `stateful_body_middleware_governor_limits_by_key` | time-window refill policies deferred |
| tower-sessions | sessions | supported | `SessionLayer`, `SessionStore`, `MemorySessionStore` | `stateful_body_middleware_sessions_create_reuse_corrupt_and_fail` | external stores deferred |
| tower-http-cache | HTTP cache | supported | `CacheLayer`, `CacheStore`, `MemoryCacheStore` | `stateful_body_middleware_cache_hits_evicts_and_reports_store_errors` | conditional validation hooks are partial |

Porting taxonomy:

| Example | Concept | Status |
|---------|---------|--------|
| `stack_router_ported_routes` | route tree | adapted |
| `stack_router_ported_routes` | path/query/state extraction | adapted |
| `stack_router_ported_middleware` | security/header middleware stack | adapted |
| `stack_router_ported_stateful` | session/cache/rate-limit state | adapted |
| `stack_router_ported_full_stack` | SC-002 middleware composition | adapted |
| all examples | async handlers/futures | unsupported |
