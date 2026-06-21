// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use http::{Method, Request, Response, StatusCode, header};
use kimojio_stack_http::Body;

use crate::{BoxError, Layer, Readiness, Service, ServiceError};

/// In-memory bounded cache store.
#[derive(Clone)]
pub struct MemoryCacheStore {
    max_entries: usize,
    entries: Arc<Mutex<BTreeMap<String, CacheEntry>>>,
    fail: Arc<Mutex<Option<ServiceError>>>,
    ttl: Option<Duration>,
}

#[derive(Clone)]
struct CacheEntry {
    response: Response<Body>,
    expires_at: Option<Instant>,
    corrupt: bool,
}

/// Pluggable cache store contract.
pub trait CacheStore: Clone {
    /// Gets a response by key.
    fn get<Cx>(&self, cx: &Cx, key: &str) -> Result<Option<Response<Body>>, ServiceError>;
    /// Inserts a response by key.
    fn insert<Cx>(
        &self,
        cx: &Cx,
        key: String,
        response: Response<Body>,
    ) -> Result<(), ServiceError>;
}

impl MemoryCacheStore {
    /// Creates a bounded cache store.
    pub fn new(max_entries: usize) -> Self {
        Self {
            max_entries: max_entries.max(1),
            entries: Arc::new(Mutex::new(BTreeMap::new())),
            fail: Arc::new(Mutex::new(None)),
            ttl: None,
        }
    }

    /// Creates a bounded cache store with expiration.
    pub fn with_ttl(max_entries: usize, ttl: Duration) -> Self {
        Self {
            ttl: Some(ttl),
            ..Self::new(max_entries)
        }
    }

    /// Fails the next store operation.
    pub fn fail_next(&self, error: ServiceError) {
        *self.fail.lock().unwrap() = Some(error);
    }

    /// Inserts a corrupt cache entry marker.
    pub fn insert_corrupt(&self, key: impl Into<String>) {
        self.entries.lock().unwrap().insert(
            key.into(),
            CacheEntry {
                response: Response::new(Body::empty()),
                expires_at: None,
                corrupt: true,
            },
        );
    }

    fn take_failure(&self) -> Result<(), ServiceError> {
        if let Some(error) = self.fail.lock().unwrap().take() {
            return Err(error);
        }
        Ok(())
    }

    fn expires_at(&self) -> Option<Instant> {
        self.ttl.map(|ttl| Instant::now() + ttl)
    }
}

impl CacheStore for MemoryCacheStore {
    fn get<Cx>(&self, _cx: &Cx, key: &str) -> Result<Option<Response<Body>>, ServiceError> {
        self.take_failure()?;
        let mut entries = self.entries.lock().unwrap();
        let Some(entry) = entries.get(key) else {
            return Ok(None);
        };
        if entry.corrupt {
            return Err(ServiceError::InvalidRequest("corrupt cache entry"));
        }
        if entry
            .expires_at
            .is_some_and(|expires_at| expires_at <= Instant::now())
        {
            entries.remove(key);
            return Ok(None);
        }
        Ok(Some(entry.response.clone()))
    }

    fn insert<Cx>(
        &self,
        _cx: &Cx,
        key: String,
        response: Response<Body>,
    ) -> Result<(), ServiceError> {
        self.take_failure()?;
        let mut entries = self.entries.lock().unwrap();
        if entries.len() >= self.max_entries
            && let Some(first) = entries.keys().next().cloned()
        {
            entries.remove(&first);
        }
        entries.insert(
            key,
            CacheEntry {
                response,
                expires_at: self.expires_at(),
                corrupt: false,
            },
        );
        Ok(())
    }
}

/// HTTP cache layer.
#[derive(Clone)]
pub struct CacheLayer<Store = MemoryCacheStore> {
    store: Store,
    key_prefix: Option<String>,
}

impl<Store> CacheLayer<Store>
where
    Store: CacheStore,
{
    /// Creates a cache layer.
    pub fn new(store: Store) -> Self {
        Self {
            store,
            key_prefix: None,
        }
    }

    /// Adds a simple key prefix to distinguish cache domains.
    pub fn with_key_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.key_prefix = Some(prefix.into());
        self
    }
}

impl<S, Store> Layer<S> for CacheLayer<Store>
where
    Store: CacheStore,
{
    type Service = Cache<S, Store>;

    fn layer(&self, inner: S) -> Self::Service {
        Cache {
            inner,
            store: self.store.clone(),
            key_prefix: self.key_prefix.clone(),
        }
    }
}

/// Cache middleware.
pub struct Cache<S, Store = MemoryCacheStore> {
    inner: S,
    store: Store,
    key_prefix: Option<String>,
}

impl<Cx, S, Store> Service<Cx, Request<Body>> for Cache<S, Store>
where
    S: Service<Cx, Request<Body>, Response = Response<Body>>,
    S::Error: Into<BoxError>,
    Store: CacheStore,
{
    type Response = Response<Body>;
    type Error = ServiceError;

    fn ready(&mut self, cx: &Cx) -> Result<Readiness, Self::Error> {
        self.inner
            .ready(cx)
            .map_err(|error| ServiceError::Inner(error.into()))
    }

    fn call(&mut self, cx: &Cx, request: Request<Body>) -> Result<Self::Response, Self::Error> {
        if request.method() != Method::GET {
            return self
                .inner
                .call(cx, request)
                .map_err(|error| ServiceError::Inner(error.into()));
        }
        if request
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("no-store"))
        {
            return self
                .inner
                .call(cx, request)
                .map_err(|error| ServiceError::Inner(error.into()));
        }
        let key = self.cache_key(&request);
        if let Some(response) = self.store.get(cx, &key)? {
            return Ok(response);
        }
        let response = self
            .inner
            .call(cx, request)
            .map_err(|error| ServiceError::Inner(error.into()))?;
        let response_no_store = response
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("no-store"));
        if response.status() == StatusCode::OK && !response_no_store {
            self.store.insert(cx, key, response.clone())?;
        }
        Ok(response)
    }
}

impl<S, Store> Cache<S, Store> {
    fn cache_key(&self, request: &Request<Body>) -> String {
        match &self.key_prefix {
            Some(prefix) => format!("{prefix}:{}", request.uri()),
            None => request.uri().to_string(),
        }
    }
}
