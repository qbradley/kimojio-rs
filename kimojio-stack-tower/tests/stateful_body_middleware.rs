// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::cell::Cell;
#[cfg(feature = "compression-gzip")]
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

#[cfg(feature = "compression-gzip")]
use flate2::Compression as FlateCompression;
#[cfg(feature = "compression-gzip")]
use flate2::read::GzDecoder;
#[cfg(feature = "compression-gzip")]
use flate2::write::{GzEncoder, ZlibEncoder};
use http::{HeaderName, Method, Request, Response, StatusCode, header};
use kimojio_stack_http::Body;
#[cfg(feature = "compression-gzip")]
use kimojio_stack_http::BodyLimits;
use kimojio_stack_tower::http::{
    CacheLayer, CacheStore, CompressionLayer, DecompressionLayer, Encoding, FollowRedirectLayer,
    GovernorLayer, MemoryCacheStore, MemorySessionStore, Session, SessionLayer, SessionStore,
};
use kimojio_stack_tower::{Service, ServiceError, ServiceExt, TimeoutLayer, service_fn};

fn request(method: Method, uri: &str, body: impl AsRef<[u8]>) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::copy_from_slice(body.as_ref(), Default::default()).unwrap())
        .unwrap()
}

fn text(response: &Response<Body>) -> String {
    String::from_utf8_lossy(response.body().as_bytes()).into_owned()
}

#[cfg(feature = "compression-gzip")]
fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), FlateCompression::default());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

#[cfg(feature = "compression-gzip")]
fn zlib(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), FlateCompression::default());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

#[cfg(feature = "compression-gzip")]
fn gunzip(bytes: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    GzDecoder::new(bytes).read_to_end(&mut output).unwrap();
    output
}

#[cfg(all(
    feature = "compression-br",
    feature = "compression-gzip",
    feature = "compression-zstd"
))]
#[test]
fn stateful_body_middleware_compresses_and_decompresses_gzip() {
    let mut compress = service_fn(|_: &(), _request: Request<Body>| {
        Ok::<_, ServiceError>(Response::new(
            Body::copy_from_slice(b"compress me", Default::default()).unwrap(),
        ))
    })
    .layer(CompressionLayer::new(vec![Encoding::Gzip]));
    let mut compress_request = request(Method::GET, "/", []);
    compress_request
        .headers_mut()
        .insert(header::ACCEPT_ENCODING, "gzip".parse().unwrap());
    let compressed = compress.call(&(), compress_request).unwrap();
    assert_eq!(
        compressed.headers().get(header::CONTENT_ENCODING).unwrap(),
        "gzip"
    );
    assert_ne!(compressed.body().as_bytes(), b"compress me");
    assert_eq!(gunzip(compressed.body().as_bytes()), b"compress me");

    let mut decompress = service_fn(|_: &(), request: Request<Body>| {
        Ok::<_, ServiceError>(Response::new(request.into_body()))
    })
    .layer(DecompressionLayer::new());

    for (encoding, header_value, encoded) in [
        (Encoding::Deflate, "deflate", zlib(b"hello")),
        (Encoding::Brotli, "br", {
            let mut out = Vec::new();
            {
                let mut writer = brotli::CompressorWriter::new(&mut out, 4096, 5, 22);
                writer.write_all(b"hello").unwrap();
            }
            out
        }),
        (
            Encoding::Zstd,
            "zstd",
            zstd::stream::encode_all(b"hello".as_slice(), 0).unwrap(),
        ),
    ] {
        let mut compress = service_fn(move |_: &(), _request: Request<Body>| {
            Ok::<_, ServiceError>(Response::new(
                Body::copy_from_slice(b"hello", Default::default()).unwrap(),
            ))
        })
        .layer(CompressionLayer::new(vec![encoding]));
        let mut accept_request = request(Method::GET, "/", []);
        accept_request
            .headers_mut()
            .insert(header::ACCEPT_ENCODING, header_value.parse().unwrap());
        assert_eq!(
            compress
                .call(&(), accept_request)
                .unwrap()
                .headers()
                .get(header::CONTENT_ENCODING)
                .unwrap(),
            header_value
        );

        let mut encoded_request = request(Method::POST, "/", encoded);
        encoded_request
            .headers_mut()
            .insert(header::CONTENT_ENCODING, header_value.parse().unwrap());
        let response = decompress.call(&(), encoded_request).unwrap();
        assert_eq!(text(&response), "hello");
    }

    let mut compressed_request = request(Method::POST, "/", gzip(b"hello"));
    compressed_request
        .headers_mut()
        .insert(header::CONTENT_ENCODING, "gzip".parse().unwrap());
    let decompressed = decompress.call(&(), compressed_request).unwrap();
    assert_eq!(text(&decompressed), "hello");
    assert!(!decompressed.headers().contains_key(header::CONTENT_LENGTH));

    let mut unsupported = request(Method::POST, "/", b"hello");
    unsupported
        .headers_mut()
        .insert(header::CONTENT_ENCODING, "unknown".parse().unwrap());
    assert!(matches!(
        decompress.call(&(), unsupported),
        Err(ServiceError::InvalidRequest(_))
    ));
}

#[cfg(feature = "compression-gzip")]
#[test]
fn stateful_body_middleware_decompression_rejects_oversized_decoded_bodies() {
    let mut decompress = service_fn(|_: &(), request: Request<Body>| {
        Ok::<_, ServiceError>(Response::new(request.into_body()))
    })
    .layer(DecompressionLayer::with_limits(BodyLimits::new(5)));
    let mut too_large = request(Method::POST, "/", gzip(b"too large"));
    too_large
        .headers_mut()
        .insert(header::CONTENT_ENCODING, "gzip".parse().unwrap());
    assert!(matches!(
        decompress.call(&(), too_large),
        Err(ServiceError::InvalidRequest(
            "decompressed body exceeded limit"
        ))
    ));
}

#[cfg(feature = "compression-gzip")]
#[test]
fn stateful_body_middleware_compression_updates_entity_headers_and_vary() {
    let mut compress = service_fn(|_: &(), _request: Request<Body>| {
        Ok::<_, ServiceError>(
            Response::builder()
                .header(header::CONTENT_LENGTH, "11")
                .header(header::ETAG, "\"plain\"")
                .body(Body::copy_from_slice(b"compress me", Default::default()).unwrap())
                .unwrap(),
        )
    })
    .layer(CompressionLayer::new(vec![Encoding::Gzip]));
    let mut compress_request = request(Method::GET, "/", []);
    compress_request
        .headers_mut()
        .insert(header::ACCEPT_ENCODING, "gzip".parse().unwrap());
    let compressed = compress.call(&(), compress_request).unwrap();
    assert!(!compressed.headers().contains_key(header::CONTENT_LENGTH));
    assert!(!compressed.headers().contains_key(header::ETAG));
    assert_eq!(
        compressed.headers().get(header::VARY).unwrap(),
        "Accept-Encoding"
    );

    let mut no_q_zero = request(Method::GET, "/", []);
    no_q_zero
        .headers_mut()
        .insert(header::ACCEPT_ENCODING, "gzip;q=0".parse().unwrap());
    let uncompressed = compress.call(&(), no_q_zero).unwrap();
    assert!(
        !uncompressed
            .headers()
            .contains_key(header::CONTENT_ENCODING)
    );
}

#[cfg(not(feature = "compression-gzip"))]
#[test]
fn stateful_body_middleware_compression_codecs_are_feature_gated() {
    let mut compress = service_fn(|_: &(), _request: Request<Body>| {
        Ok::<_, ServiceError>(Response::new(
            Body::copy_from_slice(b"compress me", Default::default()).unwrap(),
        ))
    })
    .layer(CompressionLayer::new(vec![Encoding::Gzip]));
    let mut compress_request = request(Method::GET, "/", []);
    compress_request
        .headers_mut()
        .insert(header::ACCEPT_ENCODING, "gzip".parse().unwrap());
    assert!(matches!(
        compress.call(&(), compress_request),
        Err(ServiceError::InvalidRequest(
            "gzip compression feature disabled"
        ))
    ));

    let mut decompress = service_fn(|_: &(), request: Request<Body>| {
        Ok::<_, ServiceError>(Response::new(request.into_body()))
    })
    .layer(DecompressionLayer::new());
    let mut request = request(Method::POST, "/", b"not really gzip");
    request
        .headers_mut()
        .insert(header::CONTENT_ENCODING, "gzip".parse().unwrap());
    assert!(matches!(
        decompress.call(&(), request),
        Err(ServiceError::InvalidRequest("unsupported content encoding"))
    ));
}

#[test]
fn stateful_body_middleware_follow_redirect_obeys_bounds() {
    let seen = Cell::new(0);
    let mut service = service_fn(|_: &(), request: Request<Body>| {
        seen.set(seen.get() + 1);
        if request.uri().path() == "/start" {
            Ok::<_, ServiceError>(
                Response::builder()
                    .status(StatusCode::FOUND)
                    .header(header::LOCATION, "/final")
                    .body(Body::empty())
                    .unwrap(),
            )
        } else {
            Ok(Response::new(
                Body::copy_from_slice(b"final", Default::default()).unwrap(),
            ))
        }
    })
    .layer(FollowRedirectLayer::new(2));

    let response = service
        .call(&(), request(Method::GET, "/start", []))
        .unwrap();
    assert_eq!(text(&response), "final");
    assert_eq!(seen.get(), 2);

    let mut bounded = service_fn(|_: &(), _request: Request<Body>| {
        Ok::<_, ServiceError>(
            Response::builder()
                .status(StatusCode::FOUND)
                .header(header::LOCATION, "/loop")
                .body(Body::empty())
                .unwrap(),
        )
    })
    .layer(FollowRedirectLayer::new(1));
    assert!(matches!(
        bounded.call(&(), request(Method::GET, "/loop", [])),
        Err(ServiceError::Overloaded)
    ));
}

#[test]
fn stateful_body_middleware_governor_limits_by_key() {
    let mut service = service_fn(|_: &(), _request: Request<Body>| {
        Ok::<_, ServiceError>(Response::new(Body::empty()))
    })
    .layer(
        GovernorLayer::new(1, |request: &Request<Body>| {
            request
                .headers()
                .get("x-key")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("anon")
                .to_owned()
        })
        .max_keys(1),
    );

    let mut first = request(Method::GET, "/", []);
    first.headers_mut().insert("x-key", "a".parse().unwrap());
    assert_eq!(service.call(&(), first).unwrap().status(), StatusCode::OK);
    let mut second = request(Method::GET, "/", []);
    second.headers_mut().insert("x-key", "a".parse().unwrap());
    assert_eq!(
        service.call(&(), second).unwrap().status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    let mut third = request(Method::GET, "/", []);
    third.headers_mut().insert("x-key", "b".parse().unwrap());
    assert_eq!(service.call(&(), third).unwrap().status(), StatusCode::OK);
    let mut fourth = request(Method::GET, "/", []);
    fourth.headers_mut().insert("x-key", "a".parse().unwrap());
    assert_eq!(service.call(&(), fourth).unwrap().status(), StatusCode::OK);
}

#[test]
fn stateful_body_middleware_sessions_create_reuse_corrupt_and_fail() {
    let store = MemorySessionStore::new();
    let mut service = service_fn(|_: &(), request: Request<Body>| {
        let session = request.extensions().get::<Session>().unwrap();
        Ok::<_, ServiceError>(Response::new(
            Body::copy_from_slice(session.id.as_bytes(), Default::default()).unwrap(),
        ))
    })
    .layer(SessionLayer::new(store.clone()));

    let first = service.call(&(), request(Method::GET, "/", [])).unwrap();
    let cookie = first.headers().get(header::SET_COOKIE).unwrap().clone();
    let cookie_text = cookie.to_str().unwrap();
    assert!(cookie_text.contains("HttpOnly"));
    assert!(cookie_text.contains("SameSite=Lax"));
    assert!(cookie_text.contains("Path=/"));
    let mut second_request = request(Method::GET, "/", []);
    second_request.headers_mut().insert(header::COOKIE, cookie);
    let second = service.call(&(), second_request).unwrap();
    assert_eq!(text(&first), text(&second));

    let mut multi_cookie = request(Method::GET, "/", []);
    multi_cookie.headers_mut().insert(
        header::COOKIE,
        format!("theme=dark; sid={}; other=yes", text(&first))
            .parse()
            .unwrap(),
    );
    let multi = service.call(&(), multi_cookie).unwrap();
    assert_eq!(text(&first), text(&multi));

    store.insert_corrupt("bad");
    let mut corrupt = request(Method::GET, "/", []);
    corrupt
        .headers_mut()
        .insert(header::COOKIE, "sid=bad".parse().unwrap());
    assert!(matches!(
        service.call(&(), corrupt),
        Err(ServiceError::InvalidRequest("corrupt session"))
    ));

    store.fail_next(ServiceError::InvalidRequest("store failed"));
    assert!(matches!(
        service.call(&(), request(Method::GET, "/", [])),
        Err(ServiceError::InvalidRequest("store failed"))
    ));

    let header_store = MemorySessionStore::with_ttl(Duration::from_millis(1));
    let header_name = HeaderName::from_static("x-session-id");
    let mut header_service = service_fn(|_: &(), request: Request<Body>| {
        let session = request.extensions().get::<Session>().unwrap();
        Ok::<_, ServiceError>(Response::new(
            Body::copy_from_slice(session.id.as_bytes(), Default::default()).unwrap(),
        ))
    })
    .layer(SessionLayer::new(header_store).header_transport(header_name.clone()));
    let first = header_service
        .call(&(), request(Method::GET, "/", []))
        .unwrap();
    let id = first.headers().get(&header_name).unwrap().clone();
    thread::sleep(Duration::from_millis(2));
    let mut expired = request(Method::GET, "/", []);
    expired.headers_mut().insert(header_name, id);
    let second = header_service.call(&(), expired).unwrap();
    assert_ne!(text(&first), text(&second));
}

#[test]
fn stateful_body_middleware_memory_session_store_is_bounded() {
    let store = MemorySessionStore::with_capacity(2);
    for _ in 0..5 {
        store.create(&()).unwrap();
    }
    assert_eq!(store.len(), 2);
}

#[derive(Clone)]
struct CountingSessionStore {
    inner: MemorySessionStore,
    loads: Arc<AtomicUsize>,
}

impl SessionStore for CountingSessionStore {
    fn load<Cx>(&self, cx: &Cx, id: &str) -> Result<Option<Session>, ServiceError> {
        self.loads.fetch_add(1, Ordering::AcqRel);
        self.inner.load(cx, id)
    }

    fn create<Cx>(&self, cx: &Cx) -> Result<Session, ServiceError> {
        self.inner.create(cx)
    }

    fn save<Cx>(&self, cx: &Cx, session: Session) -> Result<(), ServiceError> {
        self.inner.save(cx, session)
    }
}

#[test]
fn stateful_body_middleware_sessions_accept_custom_store_and_cleanup_on_timeout() {
    let loads = Arc::new(AtomicUsize::new(0));
    let store = CountingSessionStore {
        inner: MemorySessionStore::new(),
        loads: Arc::clone(&loads),
    };
    let dropped = Arc::new(AtomicUsize::new(0));
    struct DropFlag(Arc<AtomicUsize>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::AcqRel);
        }
    }
    let dropped_inner = Arc::clone(&dropped);
    let mut service = service_fn(move |_: &(), request: Request<Body>| {
        let _flag = DropFlag(Arc::clone(&dropped_inner));
        assert!(request.extensions().get::<Session>().is_some());
        thread::sleep(Duration::from_millis(3));
        Ok::<_, ServiceError>(Response::new(Body::empty()))
    })
    .layer(SessionLayer::new(store))
    .layer(TimeoutLayer::new(Duration::from_millis(1)));

    assert!(matches!(
        service.call(&(), request(Method::GET, "/", [])),
        Err(ServiceError::Timeout)
    ));
    assert_eq!(loads.load(Ordering::Acquire), 0);
    assert_eq!(dropped.load(Ordering::Acquire), 1);
}

#[test]
fn stateful_body_middleware_sessions_support_concurrent_store_access() {
    let store = MemorySessionStore::new();
    let session = store.create(&()).unwrap();
    let id = session.id.clone();
    thread::scope(|scope| {
        let mut handles = Vec::new();
        for _ in 0..4 {
            let store = store.clone();
            let id = id.clone();
            handles.push(scope.spawn(move || store.load(&(), &id).unwrap().unwrap().id));
        }
        for handle in handles {
            assert_eq!(handle.join().unwrap(), id);
        }
    });
}

#[test]
fn stateful_body_middleware_cache_hits_evicts_and_reports_store_errors() {
    let store = MemoryCacheStore::new(1);
    let calls = Cell::new(0);
    let mut service = service_fn(|_: &(), request: Request<Body>| {
        calls.set(calls.get() + 1);
        Ok::<_, ServiceError>(Response::new(
            Body::copy_from_slice(
                format!("{}={}", request.uri(), calls.get()).as_bytes(),
                Default::default(),
            )
            .unwrap(),
        ))
    })
    .layer(CacheLayer::new(store.clone()));

    let first = service.call(&(), request(Method::GET, "/a", [])).unwrap();
    let second = service.call(&(), request(Method::GET, "/a", [])).unwrap();
    assert_eq!(text(&first), text(&second));
    assert_eq!(calls.get(), 1);

    let _ = service.call(&(), request(Method::GET, "/b", [])).unwrap();
    let third = service.call(&(), request(Method::GET, "/a", [])).unwrap();
    assert_ne!(text(&first), text(&third));

    store.insert_corrupt("/corrupt");
    assert!(matches!(
        service.call(&(), request(Method::GET, "/corrupt", [])),
        Err(ServiceError::InvalidRequest("corrupt cache entry"))
    ));

    let ttl_store = MemoryCacheStore::with_ttl(4, Duration::from_millis(1));
    let ttl_calls = Cell::new(0);
    let mut ttl_service = service_fn(|_: &(), _request: Request<Body>| {
        ttl_calls.set(ttl_calls.get() + 1);
        Ok::<_, ServiceError>(Response::new(
            Body::copy_from_slice(ttl_calls.get().to_string().as_bytes(), Default::default())
                .unwrap(),
        ))
    })
    .layer(CacheLayer::new(ttl_store).with_key_prefix("tenant-a"));
    let first = ttl_service
        .call(&(), request(Method::GET, "/ttl", []))
        .unwrap();
    thread::sleep(Duration::from_millis(2));
    let second = ttl_service
        .call(&(), request(Method::GET, "/ttl", []))
        .unwrap();
    assert_ne!(text(&first), text(&second));

    let mut no_store = request(Method::GET, "/ttl", []);
    no_store
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    let _ = ttl_service.call(&(), no_store).unwrap();

    store.fail_next(ServiceError::InvalidRequest("cache failed"));
    assert!(matches!(
        service.call(&(), request(Method::GET, "/c", [])),
        Err(ServiceError::InvalidRequest("cache failed"))
    ));
}

#[test]
fn stateful_body_middleware_cache_bypasses_private_or_varying_responses() {
    let calls = Cell::new(0);
    let mut auth_service = service_fn(|_: &(), _request: Request<Body>| {
        calls.set(calls.get() + 1);
        Ok::<_, ServiceError>(Response::new(
            Body::copy_from_slice(calls.get().to_string().as_bytes(), Default::default()).unwrap(),
        ))
    })
    .layer(CacheLayer::new(MemoryCacheStore::new(8)));
    let mut authorized = request(Method::GET, "/auth", []);
    authorized
        .headers_mut()
        .insert(header::AUTHORIZATION, "token".parse().unwrap());
    let first = auth_service.call(&(), authorized).unwrap();
    let mut authorized = request(Method::GET, "/auth", []);
    authorized
        .headers_mut()
        .insert(header::AUTHORIZATION, "token".parse().unwrap());
    let second = auth_service.call(&(), authorized).unwrap();
    assert_ne!(text(&first), text(&second));

    let response_calls = Cell::new(0);
    let mut response_service = service_fn(|_: &(), _request: Request<Body>| {
        response_calls.set(response_calls.get() + 1);
        Ok::<_, ServiceError>(
            Response::builder()
                .header(header::SET_COOKIE, "sid=private")
                .body(
                    Body::copy_from_slice(
                        response_calls.get().to_string().as_bytes(),
                        Default::default(),
                    )
                    .unwrap(),
                )
                .unwrap(),
        )
    })
    .layer(CacheLayer::new(MemoryCacheStore::new(8)));
    let first = response_service
        .call(&(), request(Method::GET, "/cookie", []))
        .unwrap();
    let second = response_service
        .call(&(), request(Method::GET, "/cookie", []))
        .unwrap();
    assert_ne!(text(&first), text(&second));

    let private_calls = Cell::new(0);
    let mut private_service = service_fn(|_: &(), _request: Request<Body>| {
        private_calls.set(private_calls.get() + 1);
        Ok::<_, ServiceError>(
            Response::builder()
                .header(header::CACHE_CONTROL, "Private")
                .body(
                    Body::copy_from_slice(
                        private_calls.get().to_string().as_bytes(),
                        Default::default(),
                    )
                    .unwrap(),
                )
                .unwrap(),
        )
    })
    .layer(CacheLayer::new(MemoryCacheStore::new(8)));
    let first = private_service
        .call(&(), request(Method::GET, "/private", []))
        .unwrap();
    let second = private_service
        .call(&(), request(Method::GET, "/private", []))
        .unwrap();
    assert_ne!(text(&first), text(&second));
}

#[derive(Clone)]
struct CountingCacheStore {
    inner: MemoryCacheStore,
    gets: Arc<AtomicUsize>,
}

impl CacheStore for CountingCacheStore {
    fn get<Cx>(&self, cx: &Cx, key: &str) -> Result<Option<Response<Body>>, ServiceError> {
        self.gets.fetch_add(1, Ordering::AcqRel);
        self.inner.get(cx, key)
    }

    fn insert<Cx>(
        &self,
        cx: &Cx,
        key: String,
        response: Response<Body>,
    ) -> Result<(), ServiceError> {
        self.inner.insert(cx, key, response)
    }
}

#[test]
fn stateful_body_middleware_cache_accepts_custom_store_and_retains_clones_after_eviction() {
    let gets = Arc::new(AtomicUsize::new(0));
    let store = CountingCacheStore {
        inner: MemoryCacheStore::new(1),
        gets: Arc::clone(&gets),
    };
    let calls = Cell::new(0);
    let mut service = service_fn(|_: &(), request: Request<Body>| {
        calls.set(calls.get() + 1);
        Ok::<_, ServiceError>(Response::new(
            Body::copy_from_slice(
                format!("{}={}", request.uri(), calls.get()).as_bytes(),
                Default::default(),
            )
            .unwrap(),
        ))
    })
    .layer(CacheLayer::new(store));

    let first = service.call(&(), request(Method::GET, "/a", [])).unwrap();
    let first_body = text(&first);
    let _ = service.call(&(), request(Method::GET, "/b", [])).unwrap();
    assert_eq!(text(&first), first_body);
    assert!(gets.load(Ordering::Acquire) >= 2);
}

#[test]
fn stateful_body_middleware_cache_supports_concurrent_reads() {
    let store = MemoryCacheStore::new(4);
    store
        .insert(
            &(),
            "/cached".to_owned(),
            Response::new(Body::copy_from_slice(b"cached", Default::default()).unwrap()),
        )
        .unwrap();
    thread::scope(|scope| {
        let mut handles = Vec::new();
        for _ in 0..4 {
            let store = store.clone();
            handles.push(scope.spawn(move || text(&store.get(&(), "/cached").unwrap().unwrap())));
        }
        for handle in handles {
            assert_eq!(handle.join().unwrap(), "cached");
        }
    });
}
