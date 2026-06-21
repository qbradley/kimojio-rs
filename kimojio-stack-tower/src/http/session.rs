// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use http::{HeaderName, Request, Response, header};
use kimojio_stack_http::Body;

use crate::{BoxError, Layer, Readiness, Service, ServiceError};

/// Session data made available through request extensions.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Session {
    /// Session ID.
    pub id: String,
    /// Key/value data snapshot.
    pub data: BTreeMap<String, String>,
}

/// In-memory session store.
#[derive(Clone, Default)]
pub struct MemorySessionStore {
    entries: Arc<Mutex<BTreeMap<String, StoredSession>>>,
    next: Arc<AtomicU64>,
    fail: Arc<Mutex<Option<ServiceError>>>,
    ttl: Option<Duration>,
}

#[derive(Clone)]
struct StoredSession {
    session: Session,
    expires_at: Option<Instant>,
    corrupt: bool,
}

/// Pluggable session storage contract.
pub trait SessionStore: Clone {
    /// Loads a session by ID.
    fn load<Cx>(&self, cx: &Cx, id: &str) -> Result<Option<Session>, ServiceError>;
    /// Creates a new session.
    fn create<Cx>(&self, cx: &Cx) -> Result<Session, ServiceError>;
    /// Saves a session snapshot.
    fn save<Cx>(&self, cx: &Cx, session: Session) -> Result<(), ServiceError>;
}

impl MemorySessionStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an in-memory store with expiration.
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            ttl: Some(ttl),
            ..Self::default()
        }
    }

    /// Inserts a corrupt marker for `id`.
    pub fn insert_corrupt(&self, id: impl Into<String>) {
        self.entries.lock().unwrap().insert(
            id.into(),
            StoredSession {
                session: Session {
                    id: "corrupt".to_owned(),
                    data: BTreeMap::new(),
                },
                expires_at: None,
                corrupt: true,
            },
        );
    }

    /// Fails the next store operation.
    pub fn fail_next(&self, error: ServiceError) {
        *self.fail.lock().unwrap() = Some(error);
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

impl SessionStore for MemorySessionStore {
    fn load<Cx>(&self, _cx: &Cx, id: &str) -> Result<Option<Session>, ServiceError> {
        self.take_failure()?;
        let mut entries = self.entries.lock().unwrap();
        let Some(stored) = entries.get(id) else {
            return Ok(None);
        };
        if stored.corrupt {
            return Err(ServiceError::InvalidRequest("corrupt session"));
        }
        if stored
            .expires_at
            .is_some_and(|expires_at| expires_at <= Instant::now())
        {
            entries.remove(id);
            return Ok(None);
        }
        Ok(Some(stored.session.clone()))
    }

    fn create<Cx>(&self, cx: &Cx) -> Result<Session, ServiceError> {
        self.take_failure()?;
        let id = format!("session-{}", self.next.fetch_add(1, Ordering::AcqRel));
        let session = Session {
            id: id.clone(),
            data: BTreeMap::new(),
        };
        self.save(cx, session.clone())?;
        Ok(session)
    }

    fn save<Cx>(&self, _cx: &Cx, session: Session) -> Result<(), ServiceError> {
        self.take_failure()?;
        self.entries.lock().unwrap().insert(
            session.id.clone(),
            StoredSession {
                session,
                expires_at: self.expires_at(),
                corrupt: false,
            },
        );
        Ok(())
    }
}

#[derive(Clone, Debug)]
enum SessionTransport {
    Cookie,
    Header(HeaderName),
}

impl SessionTransport {
    fn read(&self, request: &Request<Body>) -> Option<String> {
        match self {
            Self::Cookie => request
                .headers()
                .get(header::COOKIE)
                .and_then(|value| value.to_str().ok())
                .and_then(|cookie| cookie.strip_prefix("sid="))
                .map(str::to_owned),
            Self::Header(name) => request
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned),
        }
    }

    fn write(&self, response: &mut Response<Body>, id: &str) {
        match self {
            Self::Cookie => {
                response.headers_mut().insert(
                    header::SET_COOKIE,
                    format!("sid={id}").parse().expect("valid session cookie"),
                );
            }
            Self::Header(name) => {
                response
                    .headers_mut()
                    .insert(name.clone(), id.parse().expect("valid session header"));
            }
        }
    }
}

/// Session middleware layer.
#[derive(Clone)]
pub struct SessionLayer<Store = MemorySessionStore> {
    store: Store,
    transport: SessionTransport,
}

impl<Store> SessionLayer<Store>
where
    Store: SessionStore,
{
    /// Creates a session layer.
    pub fn new(store: Store) -> Self {
        Self {
            store,
            transport: SessionTransport::Cookie,
        }
    }

    /// Uses a header instead of cookies to transport session IDs.
    pub fn header_transport(mut self, header: HeaderName) -> Self {
        self.transport = SessionTransport::Header(header);
        self
    }
}

impl<S, Store> Layer<S> for SessionLayer<Store>
where
    Store: SessionStore,
{
    type Service = SessionMiddleware<S, Store>;

    fn layer(&self, inner: S) -> Self::Service {
        SessionMiddleware {
            inner,
            store: self.store.clone(),
            transport: self.transport.clone(),
        }
    }
}

/// Session middleware.
pub struct SessionMiddleware<S, Store = MemorySessionStore> {
    inner: S,
    store: Store,
    transport: SessionTransport,
}

impl<Cx, S, Store> Service<Cx, Request<Body>> for SessionMiddleware<S, Store>
where
    S: Service<Cx, Request<Body>, Response = Response<Body>>,
    S::Error: Into<BoxError>,
    Store: SessionStore,
{
    type Response = Response<Body>;
    type Error = ServiceError;

    fn ready(&mut self, cx: &Cx) -> Result<Readiness, Self::Error> {
        self.inner
            .ready(cx)
            .map_err(|error| ServiceError::Inner(error.into()))
    }

    fn call(&mut self, cx: &Cx, mut request: Request<Body>) -> Result<Self::Response, Self::Error> {
        let id = self.transport.read(&request);
        let session = load_or_create(&self.store, cx, id)?;
        let session_id = session.id.clone();
        self.store.save(cx, session.clone())?;
        request.extensions_mut().insert(session);
        let mut response = self
            .inner
            .call(cx, request)
            .map_err(|error| ServiceError::Inner(error.into()))?;
        self.transport.write(&mut response, &session_id);
        Ok(response)
    }
}

fn load_or_create<Cx, Store>(
    store: &Store,
    cx: &Cx,
    id: Option<String>,
) -> Result<Session, ServiceError>
where
    Store: SessionStore,
{
    if let Some(id) = id
        && let Some(session) = store.load(cx, &id)?
    {
        return Ok(session);
    }
    store.create(cx)
}
