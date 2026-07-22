// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//! HTTP client and request builder.
//!
//! [`Client`] reuses idle HTTP/1.1 connections sequentially and multiplexes
//! concurrent requests over shared HTTP/2 sessions. Cleartext requests default
//! to HTTP/1.1 and can select HTTP/2 prior knowledge with [`Version::HTTP_2`].
//! HTTPS requests select HTTP/1.1 or HTTP/2 with ALPN.
//! HTTP/1.1 requests carrying `Expect: 100-continue` wait for the interim
//! response before sending their body, then proceed after the configured
//! bounded wait if no interim response arrives. Request bodies may be buffered
//! or streamed; response bodies are buffered unless the caller opts into
//! incremental streaming.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::io::{self, IoSlice};
use std::rc::{Rc, Weak};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use ::http::header::{
    CONNECTION, CONTENT_LENGTH, HOST, HeaderName, HeaderValue, TRANSFER_ENCODING,
};
use futures::stream;
use futures::{FutureExt, pin_mut, select_biased};
use kimojio_fsm_http::{
    ClientConnection, ClientEvent as DriverClientEvent, ClientIdleStatus, ClientRequest,
    ExchangeId, H2ErrorCode, HeaderBlock, HeaderRef, HttpProtocol, HttpVersion,
    ServerError as FsmServerError, Step, connection_header_value_has_token,
};

use super::{
    Body, BodyInner, BodyStreamCursor, ClientConfig, Error, HeaderMap, InboundTrailers,
    IntoHttpResult, Limits, Method, Protocol, ProtocolError, ProtocolErrorKind, ReadBuffer,
    Request, Response, Result, StatusCode, Uri, UriTarget, Version, connect, transport::Transport,
};
use crate::{AsyncStreamWrite, CancellationToken, operations};

/// An HTTP client with optional TLS and ALPN support.
///
/// The client is inexpensive to clone. Clones share pooled HTTP/1.1
/// connections and multiplexed HTTP/2 sessions, while separately constructed
/// clients have independent pools. Responses are buffered unless
/// [`Client::execute_streaming`] or
/// [`RequestBuilder::send_streaming`] is used. A `Client` and its clones are
/// the isolation boundary for connection-bound state, such as
/// connection-oriented authentication; that state is never shared with a
/// separately constructed client.
///
/// # Name resolution
///
/// Host names are resolved asynchronously. Kimojio prefers systemd-resolved's
/// local Varlink service and falls back to the operating system resolver on a
/// process-wide helper thread, so lookups do not block the runtime thread.
#[derive(Clone, Debug)]
pub struct Client {
    config: ClientConfig,
    pool: ConnectionPool,
}

/// A nonfatal event observed while a [`Client`] continues a request.
///
/// Use [`Client::execute_with_event_handler`] or
/// [`RequestBuilder::send_with_event_handler`] to route these events into
/// application logging or telemetry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ClientEvent {
    /// Checkout validation found a pooled connection unsafe to reuse.
    DirtyPooledConnectionDiscarded,
    /// A failed exchange on a reused connection is being retried once.
    RequestRetried,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResponseMode {
    Buffered,
    Streaming,
}

impl Client {
    /// Creates a client with default limits and HTTP/1.1 request selection.
    pub fn new() -> Self {
        Self {
            config: ClientConfig::new(),
            pool: ConnectionPool::new(),
        }
    }

    /// Creates a client from validated configuration.
    pub fn with_config(config: ClientConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            pool: ConnectionPool::new(),
        })
    }

    /// Returns the client configuration.
    pub const fn config(&self) -> &ClientConfig {
        &self.config
    }

    /// Starts a `GET` request.
    pub fn get(&self, uri: impl ToString) -> RequestBuilder {
        self.request(Method::GET, uri)
    }

    /// Starts a `POST` request.
    pub fn post(&self, uri: impl ToString) -> RequestBuilder {
        self.request(Method::POST, uri)
    }

    /// Starts a request with an arbitrary standard HTTP method.
    pub fn request(&self, method: Method, uri: impl ToString) -> RequestBuilder {
        RequestBuilder {
            client: self.clone(),
            method,
            uri: uri.to_string().parse().map_err(Error::from),
            version: None,
            headers: HeaderMap::new(),
            body: Body::empty(),
            error: None,
        }
    }

    /// Executes an already-built request and buffers its complete response.
    ///
    /// This call resolves host-name authorities asynchronously. See the
    /// [`Client`] name-resolution section.
    pub async fn execute(&self, request: Request<Body>) -> Result<Response<Body>> {
        let mut ignore_event = |_: ClientEvent| {};
        self.execute_inner(request, ResponseMode::Buffered, &mut ignore_event)
            .await
    }

    /// Executes a request and returns after the response head is received.
    ///
    /// Dropping an HTTP/1.1 body before completion retires its checked-out
    /// connection. Dropping an HTTP/2 body resets only that response stream, so
    /// sibling exchanges can continue on the shared connection.
    pub async fn execute_streaming(&self, request: Request<Body>) -> Result<Response<Body>> {
        let mut ignore_event = |_: ClientEvent| {};
        self.execute_inner(request, ResponseMode::Streaming, &mut ignore_event)
            .await
    }

    /// Executes a request and reports nonfatal pool events.
    ///
    /// The handler is called synchronously when a dirty pooled connection is
    /// discarded or a request is retried on a fresh connection.
    pub async fn execute_with_event_handler<E>(
        &self,
        request: Request<Body>,
        mut on_event: E,
    ) -> Result<Response<Body>>
    where
        E: FnMut(ClientEvent),
    {
        self.execute_inner(request, ResponseMode::Buffered, &mut on_event)
            .await
    }

    /// Executes a request with a streaming response and reports pool events.
    pub async fn execute_streaming_with_event_handler<E>(
        &self,
        request: Request<Body>,
        mut on_event: E,
    ) -> Result<Response<Body>>
    where
        E: FnMut(ClientEvent),
    {
        self.execute_inner(request, ResponseMode::Streaming, &mut on_event)
            .await
    }

    async fn execute_inner(
        &self,
        mut request: Request<Body>,
        response_mode: ResponseMode,
        on_event: &mut impl FnMut(ClientEvent),
    ) -> Result<Response<Body>> {
        self.config.validate()?;
        let target = UriTarget::parse(request.uri())?;
        let requested_protocol = Protocol::from_version(request.version())?;
        let original_headers = request.headers().clone();
        if !target.tls {
            prepare_request(&mut request, self.config.limits())?;
        }

        if requested_protocol == Protocol::Http2PriorKnowledge {
            if target.tls {
                prepare_request_for_wire(
                    &mut request,
                    &original_headers,
                    Protocol::Http2PriorKnowledge,
                    self.config.limits(),
                )?;
            }
            return self
                .execute_http2(
                    request,
                    target,
                    requested_protocol,
                    response_mode,
                    on_event,
                    None,
                )
                .await;
        }

        let (session, evicted) =
            self.pool
                .reserve_http2(&target, requested_protocol, self.config.pool_idle_timeout());
        close_pool_evictions(evicted).await;
        if let Some(session) = session {
            prepare_request_for_wire(
                &mut request,
                &original_headers,
                Protocol::Http2PriorKnowledge,
                self.config.limits(),
            )?;
            return self
                .execute_http2(
                    request,
                    target,
                    requested_protocol,
                    response_mode,
                    on_event,
                    Some(session),
                )
                .await;
        }

        let (pooled, expired) =
            self.pool
                .checkout_http1(&target, requested_protocol, self.config.pool_idle_timeout());
        close_pool_evictions(expired).await;

        let pooled = if let Some((key, mut connection)) = pooled {
            if connection.is_clean_for_reuse(key.wire_protocol).await {
                Some((key, connection))
            } else {
                on_event(ClientEvent::DirtyPooledConnectionDiscarded);
                close_connection(connection).await;
                None
            }
        } else {
            None
        };
        let reused = pooled.is_some();
        let (mut key, mut connection) = match pooled {
            Some(connection) => connection,
            None => self.open_connection(&target, requested_protocol).await?,
        };

        if target.tls
            && let Err(error) = prepare_request_for_wire(
                &mut request,
                &original_headers,
                key.wire_protocol,
                self.config.limits(),
            )
        {
            close_connection(connection).await;
            return Err(error);
        }

        if key.wire_protocol == Protocol::Http2PriorKnowledge {
            let session =
                self.pool
                    .create_http2(key, target.clone(), self.config.clone(), Some(connection));
            return self
                .execute_http2(
                    request,
                    target,
                    requested_protocol,
                    response_mode,
                    on_event,
                    Some(session),
                )
                .await;
        }

        let mut may_reuse_request_connection = request_allows_reuse(&request, key.wire_protocol);
        let request_is_replayable = !request.body().is_streaming();

        let response = match send(
            &mut connection,
            &request,
            &target,
            key.wire_protocol,
            self.config.expect_continue_timeout(),
            response_mode,
        )
        .await
        {
            Ok(response) => response,
            Err(failure)
                if reused
                    && request_is_replayable
                    && failure.can_retry_on_fresh_connection(request.method()) =>
            {
                on_event(ClientEvent::RequestRetried);
                close_connection(connection).await;
                (key, connection) = self.open_connection(&target, requested_protocol).await?;
                if target.tls
                    && let Err(error) = prepare_request_for_wire(
                        &mut request,
                        &original_headers,
                        key.wire_protocol,
                        self.config.limits(),
                    )
                {
                    close_connection(connection).await;
                    return Err(error);
                }
                if key.wire_protocol == Protocol::Http2PriorKnowledge {
                    let session = self.pool.create_http2(
                        key,
                        target.clone(),
                        self.config.clone(),
                        Some(connection),
                    );
                    return self
                        .execute_http2(
                            request,
                            target,
                            requested_protocol,
                            response_mode,
                            on_event,
                            Some(session),
                        )
                        .await;
                }
                may_reuse_request_connection = request_allows_reuse(&request, key.wire_protocol);
                match send(
                    &mut connection,
                    &request,
                    &target,
                    key.wire_protocol,
                    self.config.expect_continue_timeout(),
                    response_mode,
                )
                .await
                {
                    Ok(response) => response,
                    Err(failure) => {
                        close_connection(connection).await;
                        return Err(failure.error);
                    }
                }
            }
            Err(failure) => {
                close_connection(connection).await;
                return Err(failure.error);
            }
        };

        match response {
            SentResponse::Buffered(response) => {
                finish_response_connection(self, key, connection, may_reuse_request_connection)
                    .await?;
                Ok(response)
            }
            SentResponse::Streaming(head) => {
                let state = StreamingResponseState {
                    client: self.clone(),
                    key,
                    connection: Some(connection),
                    may_reuse_request_connection,
                    io_timeout: self.config.connection_io_timeout(),
                    trailers: InboundTrailers::streaming(),
                };
                finish_streaming_response(head, state)
            }
        }
    }

    async fn execute_http2(
        &self,
        mut request: Request<Body>,
        target: UriTarget,
        requested_protocol: Protocol,
        response_mode: ResponseMode,
        on_event: &mut impl FnMut(ClientEvent),
        mut slot: Option<H2SessionSlot>,
    ) -> Result<Response<Body>> {
        let method = request.method().clone();
        let replayable = !request.body().is_streaming();
        let mut force_new = false;
        let mut retried = false;

        loop {
            let selected = if let Some(slot) = slot.take() {
                slot
            } else if force_new {
                self.pool.create_http2(
                    PoolKey::new(&target, requested_protocol, Protocol::Http2PriorKnowledge),
                    target.clone(),
                    self.config.clone(),
                    None,
                )
            } else {
                let (slot, evicted) = self.pool.reserve_http2(
                    &target,
                    requested_protocol,
                    self.config.pool_idle_timeout(),
                );
                close_pool_evictions(evicted).await;
                match slot {
                    Some(slot) => slot,
                    None => self.pool.create_http2(
                        PoolKey::new(&target, requested_protocol, Protocol::Http2PriorKnowledge),
                        target.clone(),
                        self.config.clone(),
                        None,
                    ),
                }
            };
            let attempt_reused = selected.reused;
            let retry_request = (!retried && replayable).then(|| clone_request_for_retry(&request));
            match send_http2_submission(selected, request, target.clone(), response_mode).await {
                H2AttemptResult::Response(response) => return Ok(response),
                H2AttemptResult::Rejected(returned) => {
                    request = returned;
                    force_new = true;
                }
                H2AttemptResult::Failure(failure)
                    if retry_request.is_some()
                        && (attempt_reused || failure.guaranteed_unprocessed)
                        && failure.can_retry_on_fresh_connection(&method) =>
                {
                    on_event(ClientEvent::RequestRetried);
                    request = retry_request.expect("the retry request was checked");
                    force_new = true;
                    retried = true;
                }
                H2AttemptResult::Failure(failure) => return Err(failure.error),
            }
        }
    }

    async fn open_connection(
        &self,
        target: &UriTarget,
        requested_protocol: Protocol,
    ) -> Result<(PoolKey, Box<PooledConnection>)> {
        open_client_connection(&self.config, target, requested_protocol).await
    }
}

async fn open_client_connection(
    config: &ClientConfig,
    target: &UriTarget,
    requested_protocol: Protocol,
) -> Result<(PoolKey, Box<PooledConnection>)> {
    if !target.tls {
        let wire_protocol = requested_protocol;
        let transport = Transport::Plain(connect(target).await?);
        let connection = PooledConnection::new(transport, wire_protocol, config.limits())?;
        return Ok((
            PoolKey::new(target, requested_protocol, wire_protocol),
            connection,
        ));
    }

    #[cfg(not(feature = "tls"))]
    {
        Err(Error::TlsNotConfigured)
    }
    #[cfg(feature = "tls")]
    {
        let tls = config.tls().ok_or(Error::TlsNotConfigured)?;
        let stream = connect(target).await?;
        let socket = stream
            .into_inner()
            .expect("a newly connected stream owns its socket");
        let stream = tls
            .context()
            .client(config.limits().read_buffer_bytes(), socket, None)
            .await?;
        let negotiated =
            super::tls::negotiated_protocol(stream.get_ssl().selected_alpn_protocol())?;
        let wire_protocol = Protocol::from(negotiated);
        if requested_protocol == Protocol::Http2PriorKnowledge
            && wire_protocol != Protocol::Http2PriorKnowledge
        {
            let connection =
                PooledConnection::new(Transport::Tls(stream), wire_protocol, config.limits())?;
            close_connection(connection).await;
            return Err(Error::AlpnProtocolMismatch {
                requested: requested_protocol.version(),
                negotiated: wire_protocol.version(),
            });
        }
        let transport = Transport::Tls(stream);
        let connection = PooledConnection::new(transport, wire_protocol, config.limits())?;
        Ok((
            PoolKey::new(target, requested_protocol, wire_protocol),
            connection,
        ))
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

type CheckedOutConnection = (PoolKey, Box<PooledConnection>);
type ConnectionBatch = Vec<Box<PooledConnection>>;

#[derive(Clone, Debug)]
struct ConnectionPool {
    state: Rc<RefCell<PoolState>>,
}

impl ConnectionPool {
    fn new() -> Self {
        Self {
            state: Rc::new(RefCell::new(PoolState::default())),
        }
    }

    fn checkout_http1(
        &self,
        target: &UriTarget,
        requested_protocol: Protocol,
        idle_timeout: Duration,
    ) -> (Option<CheckedOutConnection>, PoolEvictions) {
        let now = Instant::now();
        let mut state = self.state.borrow_mut();
        let expired = state.evict_expired(now, idle_timeout);
        let position = state.idle.iter().rposition(|entry| {
            entry.key.wire_protocol == Protocol::Http1
                && entry.key.matches(target, requested_protocol)
        });
        let connection = position.map(|position| {
            let entry = state.idle.remove(position);
            (entry.key, entry.connection)
        });
        (connection, expired)
    }

    fn reserve_http2(
        &self,
        target: &UriTarget,
        requested_protocol: Protocol,
        idle_timeout: Duration,
    ) -> (Option<H2SessionSlot>, PoolEvictions) {
        let now = Instant::now();
        let mut state = self.state.borrow_mut();
        let expired = state.evict_expired(now, idle_timeout);
        let slot = state
            .sessions
            .iter()
            .rev()
            .filter(|session| session.key.matches(target, requested_protocol))
            .find_map(|session| session.try_reserve(self.clone()));
        (slot, expired)
    }

    fn create_http2(
        &self,
        key: PoolKey,
        target: UriTarget,
        config: ClientConfig,
        initial: Option<Box<PooledConnection>>,
    ) -> H2SessionSlot {
        debug_assert_eq!(key.wire_protocol, Protocol::Http2PriorKnowledge);
        let connected = initial.is_some();
        let completed = initial
            .as_ref()
            .map_or(0, |connection| connection.completed_exchanges);
        let (session, commands) = {
            let mut state = self.state.borrow_mut();
            let id = state.next_session_id;
            state.next_session_id = state.next_session_id.wrapping_add(1);
            let (session, commands) =
                H2Session::new(id, key.clone(), config.limits(), connected, completed);
            state.sessions.push(Rc::clone(&session));
            (session, commands)
        };
        let slot = session
            .try_reserve(self.clone())
            .expect("a new HTTP/2 session has request capacity");
        let pool = Rc::downgrade(&self.state);
        let pump_session = Rc::clone(&session);
        drop(operations::spawn_task(async move {
            run_http2_session(pump_session, pool, commands, target, config, initial).await;
        }));
        slot
    }

    fn insert_http1(
        &self,
        key: PoolKey,
        connection: Box<PooledConnection>,
        idle_timeout: Duration,
        max_idle_per_key: usize,
        max_idle_total: usize,
    ) -> PoolEvictions {
        let now = Instant::now();
        let mut state = self.state.borrow_mut();
        let mut evicted = state.evict_expired(now, idle_timeout);
        state.idle.push(IdleConnection {
            key: key.clone(),
            connection,
            idle_since: now,
        });
        evicted.extend(state.enforce_idle_caps(&key, max_idle_per_key, max_idle_total));
        evicted
    }
}

#[derive(Debug, Default)]
struct PoolState {
    idle: Vec<IdleConnection>,
    sessions: Vec<Rc<H2Session>>,
    next_session_id: u64,
}

impl PoolState {
    fn evict_expired(&mut self, now: Instant, idle_timeout: Duration) -> PoolEvictions {
        let mut expired = PoolEvictions::default();
        let mut position = 0;
        while position < self.idle.len() {
            if now.saturating_duration_since(self.idle[position].idle_since) >= idle_timeout {
                expired
                    .connections
                    .push(self.idle.remove(position).connection);
            } else {
                position += 1;
            }
        }
        let mut position = 0;
        while position < self.sessions.len() {
            let session = &self.sessions[position];
            let state = session.state.borrow();
            let expired_idle = state.active == 0
                && state.idle_since.is_some_and(|idle_since| {
                    now.saturating_duration_since(idle_since) >= idle_timeout
                });
            let unusable = !state.usable;
            drop(state);
            if expired_idle || unusable {
                expired.sessions.push(self.sessions.remove(position));
            } else {
                position += 1;
            }
        }
        expired
    }

    fn enforce_idle_caps(
        &mut self,
        key: &PoolKey,
        max_idle_per_key: usize,
        max_idle_total: usize,
    ) -> PoolEvictions {
        let mut evicted = PoolEvictions::default();
        while self.idle_count(Some(key)) > max_idle_per_key {
            self.evict_oldest_idle(Some(key), &mut evicted);
        }
        while self.idle_count(None) > max_idle_total {
            self.evict_oldest_idle(None, &mut evicted);
        }
        evicted
    }

    fn idle_count(&self, key: Option<&PoolKey>) -> usize {
        let http1 = self
            .idle
            .iter()
            .filter(|entry| key.is_none_or(|key| entry.key == *key))
            .count();
        let http2 = self
            .sessions
            .iter()
            .filter(|session| {
                key.is_none_or(|key| session.key == *key)
                    && session.state.borrow().active == 0
                    && session.state.borrow().idle_since.is_some()
            })
            .count();
        http1 + http2
    }

    fn evict_oldest_idle(&mut self, key: Option<&PoolKey>, evicted: &mut PoolEvictions) {
        #[derive(Clone, Copy)]
        enum Candidate {
            Http1(usize, Instant),
            Http2(usize, Instant),
        }

        let http1 = self
            .idle
            .iter()
            .enumerate()
            .filter(|(_, entry)| key.is_none_or(|key| entry.key == *key))
            .map(|(index, entry)| Candidate::Http1(index, entry.idle_since))
            .min_by_key(|candidate| match candidate {
                Candidate::Http1(_, idle_since) | Candidate::Http2(_, idle_since) => *idle_since,
            });
        let http2 = self
            .sessions
            .iter()
            .enumerate()
            .filter_map(|(index, session)| {
                if key.is_some_and(|key| session.key != *key) {
                    return None;
                }
                let state = session.state.borrow();
                (state.active == 0)
                    .then_some(state.idle_since)
                    .flatten()
                    .map(|idle_since| Candidate::Http2(index, idle_since))
            })
            .min_by_key(|candidate| match candidate {
                Candidate::Http1(_, idle_since) | Candidate::Http2(_, idle_since) => *idle_since,
            });
        let oldest = match (http1, http2) {
            (Some(left), Some(right)) => {
                let left_since = match left {
                    Candidate::Http1(_, idle_since) | Candidate::Http2(_, idle_since) => idle_since,
                };
                let right_since = match right {
                    Candidate::Http1(_, idle_since) | Candidate::Http2(_, idle_since) => idle_since,
                };
                if left_since <= right_since {
                    left
                } else {
                    right
                }
            }
            (Some(candidate), None) | (None, Some(candidate)) => candidate,
            (None, None) => return,
        };
        match oldest {
            Candidate::Http1(index, _) => {
                evicted.connections.push(self.idle.remove(index).connection);
            }
            Candidate::Http2(index, _) => {
                evicted.sessions.push(self.sessions.remove(index));
            }
        }
    }
}

impl Drop for PoolState {
    fn drop(&mut self) {
        for session in &self.sessions {
            session.shutdown();
        }
    }
}

#[derive(Default)]
struct PoolEvictions {
    connections: ConnectionBatch,
    sessions: Vec<Rc<H2Session>>,
}

impl PoolEvictions {
    fn extend(&mut self, other: Self) {
        self.connections.extend(other.connections);
        self.sessions.extend(other.sessions);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PoolKey {
    tls: bool,
    host: String,
    port: u16,
    requested_protocol: Protocol,
    wire_protocol: Protocol,
}

impl PoolKey {
    fn new(target: &UriTarget, requested_protocol: Protocol, wire_protocol: Protocol) -> Self {
        Self {
            tls: target.tls,
            host: target.host.to_ascii_lowercase(),
            port: target.port,
            requested_protocol,
            wire_protocol,
        }
    }

    fn matches(&self, target: &UriTarget, requested_protocol: Protocol) -> bool {
        self.tls == target.tls
            && self.host.eq_ignore_ascii_case(&target.host)
            && self.port == target.port
            && self.requested_protocol == requested_protocol
    }
}

struct H2Session {
    id: u64,
    key: PoolKey,
    state: Rc<RefCell<H2SessionState>>,
    commands: Rc<crate::SenderUnbounded<H2Command>>,
    shutdown: Rc<CancellationToken>,
    shutdown_fd: RefCell<Option<crate::OwnedFd>>,
}

impl H2Session {
    fn new(
        id: u64,
        key: PoolKey,
        limits: Limits,
        connected: bool,
        completed: usize,
    ) -> (Rc<Self>, crate::ReceiverUnbounded<H2Command>) {
        let (commands, receiver) = crate::async_channel_unbounded();
        (
            Rc::new(Self {
                id,
                key,
                state: Rc::new(RefCell::new(H2SessionState {
                    usable: true,
                    connected,
                    capacity: limits.max_active_streams(),
                    active: 0,
                    completed,
                    max_requests: limits.max_requests_per_connection(),
                    idle_since: None,
                    next_request_id: 1,
                })),
                commands: Rc::new(commands),
                shutdown: Rc::new(CancellationToken::new()),
                shutdown_fd: RefCell::new(None),
            }),
            receiver,
        )
    }

    fn try_reserve(self: &Rc<Self>, pool: ConnectionPool) -> Option<H2SessionSlot> {
        let mut state = self.state.borrow_mut();
        if !state.usable
            || state.active >= state.capacity
            || state.completed.saturating_add(state.active) >= state.max_requests
        {
            return None;
        }
        let reused = state.connected && (state.completed != 0 || state.active != 0);
        let request_id = state.next_request_id;
        state.next_request_id = state.next_request_id.wrapping_add(1);
        state.active = state.active.saturating_add(1);
        state.idle_since = None;
        Some(H2SessionSlot {
            session: Rc::clone(self),
            pool,
            request_id,
            reused,
            armed: true,
        })
    }

    fn release_unstarted(&self) {
        let mut state = self.state.borrow_mut();
        state.active = state.active.saturating_sub(1);
    }

    fn shutdown(&self) {
        self.state.borrow_mut().usable = false;
        self.shutdown.cancel();
        if let Some(fd) = self.shutdown_fd.borrow().as_ref() {
            let _ = rustix::net::shutdown(fd, rustix::net::Shutdown::Both);
        }
    }

    fn install_shutdown_fd(&self, fd: crate::OwnedFd) {
        if self.shutdown.is_cancelled() {
            let _ = rustix::net::shutdown(&fd, rustix::net::Shutdown::Both);
        }
        *self.shutdown_fd.borrow_mut() = Some(fd);
    }
}

impl fmt::Debug for H2Session {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("H2Session")
            .field("id", &self.id)
            .field("key", &self.key)
            .field("state", &self.state.borrow())
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct H2SessionState {
    usable: bool,
    connected: bool,
    capacity: usize,
    active: usize,
    completed: usize,
    max_requests: usize,
    idle_since: Option<Instant>,
    next_request_id: u64,
}

struct H2SessionSlot {
    session: Rc<H2Session>,
    pool: ConnectionPool,
    request_id: u64,
    reused: bool,
    armed: bool,
}

impl H2SessionSlot {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for H2SessionSlot {
    fn drop(&mut self) {
        // A slot spans pool-eviction awaits before submission, so cancellation
        // must return its capacity. `dropping_unstarted_http2_slot_releases_reservation`
        // pins this guard.
        if self.armed {
            self.session.release_unstarted();
        }
    }
}

enum H2Command {
    Submit(Box<H2Submission>),
    Cancel(u64),
}

struct H2Submission {
    request_id: u64,
    request: Request<Body>,
    target: UriTarget,
    response_mode: ResponseMode,
    reply: crate::Sender<H2SessionReply>,
}

enum H2SessionReply {
    Response(H2SessionResponse),
    Failure(SendFailure),
    Rejected(Request<Body>),
}

enum H2SessionResponse {
    Buffered(Response<Body>),
    Streaming {
        head: ReceivedResponseHead,
        body: H2ResponseBody,
    },
}

struct H2ResponseBody {
    receiver: crate::Receiver<Vec<u8>>,
    terminal_error: Rc<RefCell<Option<Error>>>,
    trailers: InboundTrailers,
    io_timeout: Duration,
}

struct H2CancellationLease {
    commands: Rc<crate::SenderUnbounded<H2Command>>,
    _pool: ConnectionPool,
    request_id: u64,
    armed: bool,
}

impl H2CancellationLease {
    fn new(session: &H2Session, pool: ConnectionPool, request_id: u64) -> Self {
        Self {
            commands: Rc::clone(&session.commands),
            _pool: pool,
            request_id,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for H2CancellationLease {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.commands.send(H2Command::Cancel(self.request_id));
        }
    }
}

struct PooledConnection {
    transport: Transport,
    driver: ClientConnection,
    input: ReadBuffer,
    current_exchange: Option<ExchangeId>,
    completed_exchanges: usize,
}

impl PooledConnection {
    fn new(transport: Transport, protocol: Protocol, limits: Limits) -> Result<Box<Self>> {
        Ok(Box::new(Self {
            transport,
            driver: ClientConnection::new(fsm_protocol(protocol), limits.protocol()),
            input: ReadBuffer::new(limits.read_buffer_bytes())?,
            current_exchange: None,
            completed_exchanges: 0,
        }))
    }

    /// Performs the checkout half of the reuse-safety gate.
    ///
    /// `ClientConnection::begin_next_exchange` validates state when a
    /// connection enters the pool. This probe is also required because
    /// unsolicited peer bytes can arrive while the connection remains idle;
    /// accepting those bytes as the next response would permit response
    /// smuggling.
    async fn is_clean_for_reuse(&mut self, protocol: Protocol) -> bool {
        if self.current_exchange.is_some() {
            return false;
        }
        if protocol == Protocol::Http1 {
            if !self.input.available().is_empty() {
                return false;
            }
            let mut byte = [0; 1];
            return matches!(self.transport.try_read_for_reuse(&mut byte).await, Ok(None));
        }

        loop {
            while !self.input.available().is_empty() {
                let (status, consumed, output) =
                    match self.driver.process_idle_input(self.input.available()) {
                        Ok(progress) => progress,
                        Err(_) => return false,
                    };
                self.input.consume(consumed);
                if !output.is_empty() && self.transport.write(&output, None).await.is_err() {
                    return false;
                }
                match status {
                    ClientIdleStatus::Reusable => {}
                    ClientIdleStatus::NeedInput => break,
                    ClientIdleStatus::NotReusable => return false,
                    _ => return false,
                }
            }

            let read = {
                let spare = match self.input.spare_mut() {
                    Ok(spare) => spare,
                    Err(_) => return false,
                };
                self.transport.try_read_for_reuse(spare).await
            };
            match read {
                Ok(Some(0)) | Err(_) => return false,
                Ok(Some(amount)) => self.input.commit_read(amount),
                Ok(None) => return self.input.available().is_empty(),
            }
        }
    }
}

impl fmt::Debug for PooledConnection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PooledConnection")
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct IdleConnection {
    key: PoolKey,
    connection: Box<PooledConnection>,
    idle_since: Instant,
}

async fn close_connections(connections: ConnectionBatch) {
    for connection in connections {
        close_connection(connection).await;
    }
}

async fn close_pool_evictions(evictions: PoolEvictions) {
    for session in evictions.sessions {
        session.shutdown();
    }
    close_connections(evictions.connections).await;
}

fn dispatch_pool_evictions(evictions: PoolEvictions) {
    for session in evictions.sessions {
        session.shutdown();
    }
    if !evictions.connections.is_empty() {
        drop(operations::spawn_task(close_connections(
            evictions.connections,
        )));
    }
}

fn enforce_http2_idle_caps(
    pool: &Weak<RefCell<PoolState>>,
    session: &H2Session,
    config: &ClientConfig,
) {
    let Some(pool) = pool.upgrade() else {
        session.shutdown();
        return;
    };
    let evictions = pool.borrow_mut().enforce_idle_caps(
        &session.key,
        config.pool_max_idle_per_key(),
        config.pool_max_idle_total(),
    );
    dispatch_pool_evictions(evictions);
}

fn remove_http2_session(pool: &Weak<RefCell<PoolState>>, session_id: u64) {
    if let Some(pool) = pool.upgrade() {
        pool.borrow_mut()
            .sessions
            .retain(|session| session.id != session_id);
    }
}

async fn close_connection(mut connection: Box<PooledConnection>) {
    let _ = connection.transport.close().await;
}

async fn finish_response_connection(
    client: &Client,
    key: PoolKey,
    mut connection: Box<PooledConnection>,
    may_reuse_request_connection: bool,
) -> Result<()> {
    connection.completed_exchanges = connection.completed_exchanges.saturating_add(1);
    let below_request_limit =
        connection.completed_exchanges < client.config.limits().max_requests_per_connection();
    let exchange_id = connection.current_exchange.ok_or_else(|| {
        protocol(
            ProtocolErrorKind::InvalidState,
            "the connection has no current exchange to retire",
        )
    })?;
    let reusable = match connection
        .driver
        .begin_next_exchange(exchange_id)
        .into_http()
    {
        Ok(reusable) => {
            connection.current_exchange = None;
            reusable && may_reuse_request_connection && below_request_limit
        }
        Err(error) => {
            close_connection(connection).await;
            return Err(error);
        }
    };
    if reusable {
        let evicted = client.pool.insert_http1(
            key,
            connection,
            client.config.pool_idle_timeout(),
            client.config.pool_max_idle_per_key(),
            client.config.pool_max_idle_total(),
        );
        close_pool_evictions(evicted).await;
    } else {
        close_connection(connection).await;
    }
    Ok(())
}

/// A fluent builder for one HTTP request.
#[must_use = "request builders do nothing until `send` or `build` is called"]
pub struct RequestBuilder {
    client: Client,
    method: Method,
    uri: Result<Uri>,
    version: Option<Version>,
    headers: HeaderMap,
    body: Body,
    error: Option<Error>,
}

impl fmt::Debug for RequestBuilder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RequestBuilder")
            .field("method", &self.method)
            .field("uri", &self.uri.as_ref().ok())
            .field("version", &self.version)
            .field("headers", &self.headers)
            .field("body_len", &self.body.known_len())
            .finish_non_exhaustive()
    }
}

impl RequestBuilder {
    /// Appends a header occurrence.
    pub fn header<K, V>(mut self, name: K, value: V) -> Self
    where
        HeaderName: TryFrom<K>,
        <HeaderName as TryFrom<K>>::Error: Into<::http::Error>,
        HeaderValue: TryFrom<V>,
        <HeaderValue as TryFrom<V>>::Error: Into<::http::Error>,
    {
        if self.error.is_none() {
            let result = HeaderName::try_from(name)
                .map_err(|error| Error::InvalidMessage(error.into()))
                .and_then(|name| {
                    HeaderValue::try_from(value)
                        .map_err(|error| Error::InvalidMessage(error.into()))
                        .map(|value| (name, value))
                });
            match result {
                Ok((name, value)) => {
                    self.headers.append(name, value);
                }
                Err(error) => self.error = Some(error),
            }
        }
        self
    }

    /// Merges header occurrences into the request.
    pub fn headers(mut self, headers: HeaderMap) -> Self {
        self.headers.extend(headers);
        self
    }

    /// Selects the HTTP wire version.
    ///
    /// Only [`Version::HTTP_11`] and [`Version::HTTP_2`] are supported.
    pub fn version(mut self, version: Version) -> Self {
        self.version = Some(version);
        self
    }

    /// Sets the buffered or streaming request body.
    pub fn body(mut self, body: impl Into<Body>) -> Self {
        self.body = body.into();
        self
    }

    /// Builds and validates the standard HTTP request without sending it.
    pub fn build(self) -> Result<Request<Body>> {
        if let Some(error) = self.error {
            return Err(error);
        }
        self.client.config.validate()?;
        let uri = self.uri?;
        UriTarget::parse(&uri)?;
        let version = self
            .version
            .unwrap_or_else(|| self.client.config.default_protocol().version());
        Protocol::from_version(version)?;

        let mut request = Request::builder()
            .method(self.method)
            .uri(uri)
            .version(version)
            .body(self.body)?;
        *request.headers_mut() = self.headers;
        prepare_request(&mut request, self.client.config.limits())?;
        Ok(request)
    }

    /// Sends the request and buffers its complete response.
    pub async fn send(self) -> Result<Response<Body>> {
        let client = self.client.clone();
        client.execute(self.build()?).await
    }

    /// Sends the request and returns a pull-based streaming response body.
    pub async fn send_streaming(self) -> Result<Response<Body>> {
        let client = self.client.clone();
        client.execute_streaming(self.build()?).await
    }

    /// Sends the request and reports nonfatal pool events.
    pub async fn send_with_event_handler<E>(self, on_event: E) -> Result<Response<Body>>
    where
        E: FnMut(ClientEvent),
    {
        let client = self.client.clone();
        client
            .execute_with_event_handler(self.build()?, on_event)
            .await
    }

    /// Sends the request with a streaming response and reports pool events.
    pub async fn send_streaming_with_event_handler<E>(self, on_event: E) -> Result<Response<Body>>
    where
        E: FnMut(ClientEvent),
    {
        let client = self.client.clone();
        client
            .execute_streaming_with_event_handler(self.build()?, on_event)
            .await
    }
}

fn prepare_request(request: &mut Request<Body>, limits: Limits) -> Result<()> {
    Protocol::from_version(request.version())?;
    let target = UriTarget::parse(request.uri())?;
    if request.body().has_outbound_trailers() {
        return Err(protocol(
            ProtocolErrorKind::UnsupportedFeature,
            "client request trailers are not supported",
        ));
    }
    let body_len = request.body().known_len();
    match body_len {
        Some(body_len) => {
            request.headers_mut().remove(TRANSFER_ENCODING);
            request.headers_mut().insert(
                CONTENT_LENGTH,
                HeaderValue::from_str(&body_len.to_string())
                    .expect("a decimal usize is a valid header value"),
            );
        }
        None => {
            request.headers_mut().remove(CONTENT_LENGTH);
            request.headers_mut().remove(TRANSFER_ENCODING);
        }
    }

    match Protocol::from_version(request.version())? {
        Protocol::Http1 => {
            if !request.headers().contains_key(HOST) {
                request.headers_mut().insert(
                    HOST,
                    HeaderValue::from_str(&target.authority).map_err(Error::InvalidHeaderValue)?,
                );
            }
        }
        Protocol::Http2PriorKnowledge => {}
    }

    validate_request_with_fsm(request, &target, limits)
}

fn prepare_request_for_wire(
    request: &mut Request<Body>,
    original_headers: &HeaderMap,
    wire_protocol: Protocol,
    limits: Limits,
) -> Result<()> {
    *request.version_mut() = wire_protocol.version();
    *request.headers_mut() = original_headers.clone();
    if wire_protocol == Protocol::Http2PriorKnowledge {
        request.headers_mut().remove(CONNECTION);
    }
    prepare_request(request, limits)
}

fn validate_request_with_fsm(
    request: &Request<Body>,
    target: &UriTarget,
    limits: Limits,
) -> Result<()> {
    let protocol = Protocol::from_version(request.version())?;
    let mut connection = ClientConnection::new(fsm_protocol(protocol), limits.protocol());
    let headers = request_header_refs(request);
    connection
        .prepare_request(ClientRequest {
            method: request.method().as_str(),
            scheme: target.scheme(),
            authority: &target.authority,
            target: &target.request_target,
            headers: &headers,
            body_len: request.body().known_len(),
        })
        .into_http()?;
    Ok(())
}

fn fsm_protocol(protocol: Protocol) -> HttpProtocol {
    match protocol {
        Protocol::Http1 => HttpProtocol::Http1,
        Protocol::Http2PriorKnowledge => HttpProtocol::Http2,
    }
}

fn request_header_refs(request: &Request<Body>) -> Vec<HeaderRef<'_>> {
    request
        .headers()
        .iter()
        .map(|(name, value)| {
            HeaderRef::new(name.as_str().as_bytes(), value.as_bytes())
                .with_sensitive(matches!(name.as_str(), "authorization" | "cookie"))
        })
        .collect()
}

fn request_allows_reuse(request: &Request<Body>, protocol: Protocol) -> bool {
    protocol != Protocol::Http1
        || !request
            .headers()
            .get_all(CONNECTION)
            .iter()
            .any(|value| connection_header_value_has_token(value.as_bytes(), b"close"))
}

fn clone_request_for_retry(request: &Request<Body>) -> Request<Body> {
    let mut cloned = Request::new(request.body().clone());
    *cloned.method_mut() = request.method().clone();
    *cloned.uri_mut() = request.uri().clone();
    *cloned.version_mut() = request.version();
    *cloned.headers_mut() = request.headers().clone();
    cloned
}

enum H2AttemptResult {
    Response(Response<Body>),
    Failure(SendFailure),
    Rejected(Request<Body>),
}

async fn send_http2_submission(
    mut slot: H2SessionSlot,
    request: Request<Body>,
    target: UriTarget,
    response_mode: ResponseMode,
) -> H2AttemptResult {
    let (reply, replies) = crate::async_channel();
    let mut lease = H2CancellationLease::new(&slot.session, slot.pool.clone(), slot.request_id);
    let command = H2Command::Submit(Box::new(H2Submission {
        request_id: slot.request_id,
        request,
        target,
        response_mode,
        reply,
    }));
    if let Err(command) = slot.session.commands.send(command) {
        lease.disarm();
        let H2Command::Submit(submission) = command else {
            unreachable!("only a submission was sent");
        };
        return H2AttemptResult::Rejected(submission.request);
    }
    slot.disarm();
    drop(slot);

    let reply = match replies.recv().await {
        Ok(reply) => reply,
        Err(_) => {
            lease.disarm();
            return H2AttemptResult::Failure(SendFailure::connection(
                Error::Canceled,
                false,
                false,
            ));
        }
    };
    match reply {
        H2SessionReply::Response(H2SessionResponse::Buffered(response)) => {
            lease.disarm();
            H2AttemptResult::Response(response)
        }
        H2SessionReply::Response(H2SessionResponse::Streaming { head, body }) => {
            H2AttemptResult::Response(match finish_http2_streaming_response(head, body, lease) {
                Ok(response) => response,
                Err(error) => {
                    return H2AttemptResult::Failure(SendFailure::other(error, true, true));
                }
            })
        }
        H2SessionReply::Failure(failure) => {
            lease.disarm();
            H2AttemptResult::Failure(failure)
        }
        H2SessionReply::Rejected(request) => {
            lease.disarm();
            H2AttemptResult::Rejected(request)
        }
    }
}

struct H2StreamingBodyState {
    body: H2ResponseBody,
    _lease: H2CancellationLease,
}

fn finish_http2_streaming_response(
    head: ReceivedResponseHead,
    body: H2ResponseBody,
    lease: H2CancellationLease,
) -> Result<Response<Body>> {
    let trailers = body.trailers.clone();
    let state = H2StreamingBodyState {
        body,
        _lease: lease,
    };
    let body = Body::from_inbound_stream(
        stream::try_unfold(state, |state| async move {
            let deadline = streaming_io_deadline(state.body.io_timeout)?;
            match operations::timeout_at(deadline, state.body.receiver.recv()).await {
                Ok(Ok(chunk)) => Ok(Some((chunk, state))),
                Ok(Err(_)) => match state.body.terminal_error.borrow_mut().take() {
                    Some(error) => Err(error),
                    None => Ok(None),
                },
                Err(crate::TimeoutError::Timeout) => Err(Error::Io(crate::Errno::TIMEDOUT)),
                Err(crate::TimeoutError::Canceled) => Err(Error::Canceled),
            }
        }),
        trailers,
    );
    finish_response(Some(head), body)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PumpAction {
    Progress,
    NeedInput,
    Write,
    ResponseHead,
    Complete,
}

type ReceivedResponseHead = (StatusCode, HttpVersion, HeaderMap);

enum SentResponse {
    Buffered(Response<Body>),
    Streaming(ReceivedResponseHead),
}

enum RequestBodyState<'a> {
    Buffered { bytes: &'a [u8], sent: usize },
    Streaming(BodyStreamCursor),
}

impl<'a> RequestBodyState<'a> {
    fn new(body: &'a Body) -> Self {
        match &body.inner {
            BodyInner::Buffered(bytes) => Self::Buffered { bytes, sent: 0 },
            BodyInner::Streaming(source) => Self::Streaming(BodyStreamCursor::new(source.clone())),
        }
    }

    fn available(&self) -> Option<&[u8]> {
        match self {
            Self::Buffered { bytes, sent } => Some(&bytes[*sent..]),
            Self::Streaming(cursor) => cursor.available(),
        }
    }

    fn needs_chunk(&self) -> bool {
        matches!(self, Self::Streaming(cursor) if cursor.needs_chunk())
    }

    fn poll_chunk(&mut self, context: &mut Context<'_>) -> Poll<Result<()>> {
        match self {
            Self::Buffered { .. } => Poll::Ready(Ok(())),
            Self::Streaming(cursor) => match cursor.poll_chunk(context) {
                Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
                Poll::Ready(Err(source)) => Poll::Ready(Err(Error::BodyStream { source })),
                Poll::Pending => Poll::Pending,
            },
        }
    }

    fn commit(&mut self, payload_len: usize) {
        match self {
            Self::Buffered { bytes, sent } => {
                assert!(payload_len <= bytes.len() - *sent);
                *sent += payload_len;
            }
            Self::Streaming(cursor) => cursor.commit(payload_len),
        }
    }
}

enum WaitAction {
    Input(Result<usize>),
    Body(Result<()>),
}

#[derive(Debug)]
struct SendFailure {
    error: Error,
    response_bytes_received: bool,
    request_bytes_written: bool,
    connection_failure: bool,
    guaranteed_unprocessed: bool,
}

impl SendFailure {
    fn other(error: Error, response_bytes_received: bool, request_bytes_written: bool) -> Self {
        Self {
            error,
            response_bytes_received,
            request_bytes_written,
            connection_failure: false,
            guaranteed_unprocessed: false,
        }
    }

    fn connection(
        error: Error,
        response_bytes_received: bool,
        request_bytes_written: bool,
    ) -> Self {
        Self {
            error,
            response_bytes_received,
            request_bytes_written,
            connection_failure: true,
            guaranteed_unprocessed: false,
        }
    }

    fn can_retry_on_fresh_connection(&self, method: &Method) -> bool {
        // A pooled transport can fail after checkout validation. RFC 9110
        // section 9.2.2 permits automatic replay of idempotent methods. A
        // non-idempotent request is replayed only when no request byte was
        // written, because a fully written request may already have produced
        // its side effect even if no response byte arrived.
        !self.response_bytes_received
            && (self.guaranteed_unprocessed
                || (self.connection_failure
                    && (is_idempotent(method) || !self.request_bytes_written)))
    }
}

fn is_idempotent(method: &Method) -> bool {
    // RFC 9110 §9.2.2 defines these request methods as idempotent.
    matches!(
        method.as_str(),
        "GET" | "HEAD" | "OPTIONS" | "TRACE" | "PUT" | "DELETE"
    )
}

#[derive(Clone, Debug)]
enum H2SharedFailure {
    Io(crate::Errno),
    UnexpectedEof,
    Driver(FsmServerError),
    Protocol(ProtocolErrorKind, String),
    AddressResolution {
        authority: String,
        kind: io::ErrorKind,
        message: String,
    },
    TlsNotConfigured,
    #[cfg(feature = "tls")]
    UnsupportedAlpnProtocol(Vec<u8>),
    AlpnProtocolMismatch {
        requested: Version,
        negotiated: Version,
    },
    Canceled,
}

impl H2SharedFailure {
    fn from_error(error: Error) -> Self {
        match error {
            Error::AddressResolution { authority, source } => Self::AddressResolution {
                authority,
                kind: source.kind(),
                message: source.to_string(),
            },
            Error::TlsNotConfigured => Self::TlsNotConfigured,
            #[cfg(feature = "tls")]
            Error::UnsupportedAlpnProtocol(protocol) => Self::UnsupportedAlpnProtocol(protocol),
            Error::AlpnProtocolMismatch {
                requested,
                negotiated,
            } => Self::AlpnProtocolMismatch {
                requested,
                negotiated,
            },
            Error::Io(error) => Self::Io(error),
            Error::UnexpectedEof => Self::UnexpectedEof,
            Error::Canceled => Self::Canceled,
            Error::Protocol(error) => Self::Protocol(error.kind(), error.to_string()),
            error => Self::Protocol(ProtocolErrorKind::InvalidState, error.to_string()),
        }
    }

    fn error(&self) -> Error {
        match self {
            Self::Io(error) => Error::Io(*error),
            Self::UnexpectedEof => Error::UnexpectedEof,
            Self::Driver(error) => std::result::Result::<(), _>::Err(error.clone())
                .into_http()
                .expect_err("an FSM error remains an HTTP error"),
            Self::Protocol(kind, detail) => protocol(*kind, detail.clone()),
            Self::AddressResolution {
                authority,
                kind,
                message,
            } => Error::AddressResolution {
                authority: authority.clone(),
                source: io::Error::new(*kind, message.clone()),
            },
            Self::TlsNotConfigured => Error::TlsNotConfigured,
            #[cfg(feature = "tls")]
            Self::UnsupportedAlpnProtocol(protocol) => {
                Error::UnsupportedAlpnProtocol(protocol.clone())
            }
            Self::AlpnProtocolMismatch {
                requested,
                negotiated,
            } => Error::AlpnProtocolMismatch {
                requested: *requested,
                negotiated: *negotiated,
            },
            Self::Canceled => Error::Canceled,
        }
    }

    fn is_connection_failure(&self) -> bool {
        matches!(
            self,
            Self::Io(_) | Self::UnexpectedEof | Self::Driver(FsmServerError::PeerGoaway { .. })
        )
    }

    fn guarantees_unprocessed(&self, exchange_id: ExchangeId) -> bool {
        // RFC 9113 §6.8 guarantees that a graceful GOAWAY did not process
        // streams above its last-stream ID. `graceful_http2_goaway_retries_only_unprocessed_stream`
        // pins the per-stream split.
        matches!(
            self,
            Self::Driver(FsmServerError::PeerGoaway {
                last_stream_id,
                error_code,
            }) if *error_code == H2ErrorCode::NoError.as_u32()
                && exchange_id.as_u64() > u64::from(*last_stream_id)
        )
    }
}

enum H2RequestBodyState {
    Buffered { bytes: Vec<u8>, sent: usize },
    Streaming(BodyStreamCursor),
}

impl H2RequestBodyState {
    fn new(body: Body) -> Self {
        match body.inner {
            BodyInner::Buffered(bytes) => Self::Buffered { bytes, sent: 0 },
            BodyInner::Streaming(source) => Self::Streaming(BodyStreamCursor::new(source)),
        }
    }

    fn available(&self) -> Option<&[u8]> {
        match self {
            Self::Buffered { bytes, sent } => Some(&bytes[*sent..]),
            Self::Streaming(cursor) => cursor.available(),
        }
    }

    fn needs_chunk(&self) -> bool {
        matches!(self, Self::Streaming(cursor) if cursor.needs_chunk())
    }

    fn poll_chunk(&mut self, context: &mut Context<'_>) -> Poll<Result<()>> {
        match self {
            Self::Buffered { .. } => Poll::Ready(Ok(())),
            Self::Streaming(cursor) => match cursor.poll_chunk(context) {
                Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
                Poll::Ready(Err(source)) => Poll::Ready(Err(Error::BodyStream { source })),
                Poll::Pending => Poll::Pending,
            },
        }
    }

    fn commit(&mut self, payload_len: usize) {
        match self {
            Self::Buffered { bytes, sent } => {
                assert!(payload_len <= bytes.len() - *sent);
                *sent += payload_len;
            }
            Self::Streaming(cursor) => cursor.commit(payload_len),
        }
    }
}

struct H2Exchange {
    request_id: u64,
    response_mode: ResponseMode,
    reply: Option<crate::Sender<H2SessionReply>>,
    request_body: H2RequestBodyState,
    response_head: Option<ReceivedResponseHead>,
    response_body: Vec<u8>,
    response_trailers: Option<HeaderMap>,
    completed_response: Option<Response<Body>>,
    body_sender: Option<crate::Sender<Vec<u8>>>,
    streaming_trailers: Option<InboundTrailers>,
    terminal_error: Option<Rc<RefCell<Option<Error>>>>,
    io_timeout: Duration,
    request_bytes_written: bool,
    response_bytes_received: bool,
    abandoned: bool,
}

impl H2Exchange {
    fn send_failure(
        &mut self,
        error: Error,
        connection_failure: bool,
        guaranteed_unprocessed: bool,
    ) {
        if let Some(terminal_error) = self.terminal_error.as_ref() {
            *terminal_error.borrow_mut() = Some(error);
            self.body_sender.take();
            return;
        }
        if let Some(reply) = self.reply.take() {
            let failure = SendFailure {
                error,
                response_bytes_received: self.response_bytes_received,
                request_bytes_written: self.request_bytes_written,
                connection_failure,
                guaranteed_unprocessed,
            };
            let _ = reply.try_send(H2SessionReply::Failure(failure));
        }
    }
}

enum H2PumpAction {
    NeedInput,
    Write,
    ResponseHead(ExchangeId, ReceivedResponseHead),
    BufferedResponseBody,
    StreamingResponseBody(ExchangeId, Vec<u8>),
    ResponseTrailers(ExchangeId, HeaderMap),
    ResponseComplete(ExchangeId),
    Done,
}

struct H2RequestBodyFailure {
    exchange_id: ExchangeId,
    error: Error,
}

enum H2PumpWait {
    Command(std::result::Result<H2Command, crate::ChannelError>),
    Input(Result<usize>),
    Body(std::result::Result<(), H2RequestBodyFailure>),
    Shutdown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum H2PumpStopReason {
    SessionShutdown,
    RequestLimitReached,
    IdleTimeout,
    IdlePeerClosed,
}

#[derive(Debug)]
enum H2PumpTermination {
    BeforeStart(H2SharedFailure),
    Stopped(H2PumpStopReason),
    Failed(H2SharedFailure),
}

impl H2PumpTermination {
    fn active_failure(&self) -> H2SharedFailure {
        match self {
            Self::BeforeStart(failure) | Self::Failed(failure) => failure.clone(),
            Self::Stopped(H2PumpStopReason::IdlePeerClosed) => H2SharedFailure::UnexpectedEof,
            Self::Stopped(
                H2PumpStopReason::SessionShutdown
                | H2PumpStopReason::RequestLimitReached
                | H2PumpStopReason::IdleTimeout,
            ) => H2SharedFailure::Canceled,
        }
    }

    fn queued_failure(&self) -> Option<H2SharedFailure> {
        match self {
            Self::BeforeStart(failure) => Some(failure.clone()),
            Self::Stopped(_) | Self::Failed(_) => None,
        }
    }
}

struct H2PumpExit {
    termination: H2PumpTermination,
    connection: Option<Box<PooledConnection>>,
    exchanges: HashMap<ExchangeId, H2Exchange>,
}

struct H2PumpContext<'a> {
    session: &'a H2Session,
    pool: &'a Weak<RefCell<PoolState>>,
    commands: &'a crate::ReceiverUnbounded<H2Command>,
    config: &'a ClientConfig,
}

async fn run_http2_session(
    session: Rc<H2Session>,
    pool: Weak<RefCell<PoolState>>,
    commands: crate::ReceiverUnbounded<H2Command>,
    target: UriTarget,
    config: ClientConfig,
    initial: Option<Box<PooledConnection>>,
) {
    let exit = drive_http2_session(&session, &pool, &commands, &target, &config, initial).await;
    finish_http2_session(&session, &pool, &commands, exit).await;
}

async fn drive_http2_session(
    session: &H2Session,
    pool: &Weak<RefCell<PoolState>>,
    commands: &crate::ReceiverUnbounded<H2Command>,
    target: &UriTarget,
    config: &ClientConfig,
    initial: Option<Box<PooledConnection>>,
) -> H2PumpExit {
    let mut exchanges = HashMap::new();
    let opened = if let Some(connection) = initial {
        Ok((session.key.clone(), connection))
    } else {
        let open = open_client_connection(config, target, session.key.requested_protocol).fuse();
        let shutdown = session.shutdown.cancelled().fuse();
        pin_mut!(open, shutdown);
        select_biased! {
            _ = shutdown => Err(Error::Canceled),
            result = open => result,
        }
    };
    let (key, mut connection) = match opened {
        Ok(opened) => opened,
        Err(error) => {
            return H2PumpExit {
                termination: H2PumpTermination::BeforeStart(H2SharedFailure::from_error(error)),
                connection: None,
                exchanges,
            };
        }
    };
    if key.wire_protocol != Protocol::Http2PriorKnowledge {
        return H2PumpExit {
            termination: H2PumpTermination::BeforeStart(H2SharedFailure::Protocol(
                ProtocolErrorKind::InvalidState,
                "the HTTP/2 session opened a non-HTTP/2 transport".to_owned(),
            )),
            connection: Some(connection),
            exchanges,
        };
    }
    debug_assert_eq!(key, session.key);

    debug_assert!(connection.current_exchange.is_none());
    match connection.transport.try_clone_fd() {
        Ok(fd) => session.install_shutdown_fd(fd),
        Err(error) => {
            return H2PumpExit {
                termination: H2PumpTermination::BeforeStart(H2SharedFailure::Io(error)),
                connection: Some(connection),
                exchanges,
            };
        }
    }
    {
        let mut state = session.state.borrow_mut();
        state.connected = true;
        state.capacity = connection.driver.max_active_exchanges();
    }

    let termination = match drive_http2_connection(
        H2PumpContext {
            session,
            pool,
            commands,
            config,
        },
        &mut connection.transport,
        &mut connection.driver,
        &mut connection.input,
        &mut exchanges,
    )
    .await
    {
        Ok(reason) => H2PumpTermination::Stopped(reason),
        Err(failure) => H2PumpTermination::Failed(failure),
    };
    H2PumpExit {
        termination,
        connection: Some(connection),
        exchanges,
    }
}

/// The sole HTTP/2 client-pump termination funnel.
///
/// Every setup failure, session shutdown, request-limit retirement, idle
/// timeout or EOF, GOAWAY, transport error, and driver error must arrive here
/// as an [`H2PumpTermination`]. Before the pump task completes, this function
/// makes the session unavailable, gives every active exchange and queued
/// submission a terminal result, and closes command admission so a racing
/// sender gets its request back. It then closes the transport and its shutdown
/// descriptor, drops the driver and read buffer, and removes the session from
/// the pool. Established-session submissions are rejected so their callers can
/// select a fresh session; setup failures are reported directly. No reply or
/// response-body sender remains owned when this function returns.
async fn finish_http2_session(
    session: &H2Session,
    pool: &Weak<RefCell<PoolState>>,
    commands: &crate::ReceiverUnbounded<H2Command>,
    mut exit: H2PumpExit,
) {
    session.state.borrow_mut().usable = false;
    session.commands.close();

    let active_failure = exit.termination.active_failure();
    for (exchange_id, exchange) in &mut exit.exchanges {
        exchange.send_failure(
            active_failure.error(),
            active_failure.is_connection_failure(),
            active_failure.guarantees_unprocessed(*exchange_id),
        );
    }
    let active_count = exit.exchanges.len();
    exit.exchanges.clear();
    {
        let mut state = session.state.borrow_mut();
        state.active = state.active.saturating_sub(active_count);
    }

    let queued_failure = exit.termination.queued_failure();
    loop {
        match commands.try_recv() {
            Ok(Some(H2Command::Submit(submission))) => {
                session.release_unstarted();
                if let Some(failure) = queued_failure.as_ref() {
                    let _ = submission
                        .reply
                        .try_send(H2SessionReply::Failure(SendFailure {
                            error: failure.error(),
                            response_bytes_received: false,
                            request_bytes_written: false,
                            connection_failure: failure.is_connection_failure(),
                            guaranteed_unprocessed: false,
                        }));
                } else {
                    let _ = submission
                        .reply
                        .try_send(H2SessionReply::Rejected(submission.request));
                }
            }
            Ok(Some(H2Command::Cancel(_))) => {}
            Ok(None) | Err(_) => break,
        }
    }

    drop(session.shutdown_fd.borrow_mut().take());
    if let Some(connection) = exit.connection.take() {
        close_connection(connection).await;
    }
    remove_http2_session(pool, session.id);
}

async fn drive_http2_connection(
    context: H2PumpContext<'_>,
    transport: &mut Transport,
    driver: &mut ClientConnection,
    input: &mut ReadBuffer,
    exchanges: &mut HashMap<ExchangeId, H2Exchange>,
) -> std::result::Result<H2PumpStopReason, H2SharedFailure> {
    let H2PumpContext {
        session,
        pool,
        commands,
        config,
    } = context;
    let mut prefer_body = false;
    let mut idle_announced = false;

    loop {
        if session.shutdown.is_cancelled() {
            return Ok(H2PumpStopReason::SessionShutdown);
        }

        if !exchanges.is_empty() {
            loop {
                match commands.try_recv() {
                    Ok(Some(command)) => {
                        handle_http2_command(
                            session,
                            driver,
                            exchanges,
                            command,
                            config.connection_io_timeout(),
                        )?;
                        idle_announced = false;
                    }
                    Ok(None) => break,
                    Err(_) => return Ok(H2PumpStopReason::SessionShutdown),
                }
            }
        }

        retire_http2_exchanges(session, pool, config, driver, exchanges)?;
        update_http2_capacity(session, driver);

        if let Some(failure) = poll_http2_request_bodies_once(exchanges).await {
            abandon_http2_exchange(driver, exchanges, failure.exchange_id, failure.error)?;
            prefer_body = false;
            continue;
        }

        if prefer_body && write_http2_request_body(transport, driver, exchanges).await? {
            prefer_body = false;
            continue;
        }

        if exchanges.is_empty() {
            let (reserved, established, request_limit_reached) = {
                let state = session.state.borrow();
                (
                    state.active,
                    state.completed != 0,
                    state.completed.saturating_add(state.active) >= state.max_requests,
                )
            };
            if reserved == 0 && request_limit_reached {
                return Ok(H2PumpStopReason::RequestLimitReached);
            }
            if reserved == 0 && !idle_announced {
                {
                    let mut state = session.state.borrow_mut();
                    state.idle_since = Some(crate::clock_now());
                }
                enforce_http2_idle_caps(pool, session, config);
                idle_announced = true;
                if session.shutdown.is_cancelled() {
                    return Ok(H2PumpStopReason::SessionShutdown);
                }
            }

            while !input.available().is_empty() {
                let (status, consumed, output) = driver
                    .process_idle_input(input.available())
                    .map_err(H2SharedFailure::Driver)?;
                input.consume(consumed);
                if !output.is_empty() {
                    transport
                        .write(&output, None)
                        .await
                        .map_err(H2SharedFailure::Io)?;
                }
                update_http2_capacity(session, driver);
                match status {
                    ClientIdleStatus::Reusable => {}
                    ClientIdleStatus::NeedInput => break,
                    ClientIdleStatus::NotReusable => {
                        return Err(H2SharedFailure::Protocol(
                            ProtocolErrorKind::InvalidState,
                            "the idle HTTP/2 connection is no longer reusable".to_owned(),
                        ));
                    }
                    _ => {
                        return Err(H2SharedFailure::Protocol(
                            ProtocolErrorKind::InvalidState,
                            "the driver returned an unknown idle status".to_owned(),
                        ));
                    }
                }
                if consumed == 0 {
                    break;
                }
            }

            let idle_deadline = if reserved == 0 {
                session
                    .state
                    .borrow()
                    .idle_since
                    .and_then(|idle_since| idle_since.checked_add(config.pool_idle_timeout()))
            } else {
                None
            };
            if reserved == 0 && config.pool_idle_timeout().is_zero() {
                return Ok(H2PumpStopReason::IdleTimeout);
            }
            let event = {
                let command = commands.recv().fuse();
                let read = input
                    .read_from_with_deadline(transport, idle_deadline)
                    .fuse();
                let shutdown = session.shutdown.cancelled().fuse();
                pin_mut!(command, read, shutdown);
                if established {
                    select_biased! {
                        _ = shutdown => H2PumpWait::Shutdown,
                        input = read => H2PumpWait::Input(input),
                        command = command => H2PumpWait::Command(command),
                    }
                } else {
                    select_biased! {
                        _ = shutdown => H2PumpWait::Shutdown,
                        command = command => H2PumpWait::Command(command),
                        input = read => H2PumpWait::Input(input),
                    }
                }
            };
            match event {
                H2PumpWait::Shutdown => return Ok(H2PumpStopReason::SessionShutdown),
                H2PumpWait::Command(Ok(command)) => {
                    handle_http2_command(
                        session,
                        driver,
                        exchanges,
                        command,
                        config.connection_io_timeout(),
                    )?;
                    idle_announced = false;
                }
                H2PumpWait::Command(Err(_)) => return Ok(H2PumpStopReason::SessionShutdown),
                H2PumpWait::Input(Ok(0)) => return Ok(H2PumpStopReason::IdlePeerClosed),
                H2PumpWait::Input(Ok(_)) => {}
                H2PumpWait::Input(Err(Error::Io(error)))
                    if reserved == 0
                        && matches!(error, crate::Errno::TIME | crate::Errno::TIMEDOUT) =>
                {
                    return Ok(H2PumpStopReason::IdleTimeout);
                }
                H2PumpWait::Input(Err(error)) => {
                    return Err(H2SharedFailure::from_error(error));
                }
                H2PumpWait::Body(_) => unreachable!("idle waits do not poll request bodies"),
            }
            continue;
        }

        let step = driver.step(input.available(), |step| -> Result<H2PumpAction> {
            match step {
                Step::NeedInput => Ok(H2PumpAction::NeedInput),
                Step::Write(_) => Ok(H2PumpAction::Write),
                Step::Done => Ok(H2PumpAction::Done),
                Step::Event(DriverClientEvent::ResponseHead {
                    exchange_id,
                    status,
                    version,
                    headers,
                    content_length: _,
                }) => {
                    let status = StatusCode::from_u16(status).map_err(|error| {
                        protocol(
                            ProtocolErrorKind::MalformedMessage,
                            format!("invalid response status: {error}"),
                        )
                    })?;
                    let headers = convert_headers(headers)?;
                    Ok(H2PumpAction::ResponseHead(
                        exchange_id,
                        (status, version, headers),
                    ))
                }
                Step::Event(DriverClientEvent::ResponseBody { exchange_id, chunk }) => {
                    // Buffered DATA stays borrowed until it reaches the
                    // exchange buffer. `buffered_http2_body_avoids_owned_pump_action`
                    // pins the allocation-free pump action.
                    match prepare_http2_response_body(exchanges, exchange_id, chunk)? {
                        Some(chunk) => Ok(H2PumpAction::StreamingResponseBody(exchange_id, chunk)),
                        None => Ok(H2PumpAction::BufferedResponseBody),
                    }
                }
                Step::Event(DriverClientEvent::ResponseTrailers {
                    exchange_id,
                    headers,
                }) => Ok(H2PumpAction::ResponseTrailers(
                    exchange_id,
                    convert_headers(headers)?,
                )),
                Step::Event(DriverClientEvent::ResponseComplete { exchange_id }) => {
                    Ok(H2PumpAction::ResponseComplete(exchange_id))
                }
                _ => Err(protocol(
                    ProtocolErrorKind::UnexpectedEvent,
                    "the connection driver returned an unexpected client step",
                )),
            }
        });
        // The FSM owns RFC 9113 §4.1 framing and can attribute a complete
        // frame header before its payload arrives. `partial_http2_response_frame_prevents_retry`
        // pins this retry-safety signal.
        if let Some(exchange_id) = driver.input_exchange()
            && let Some(exchange) = exchanges.get_mut(&exchange_id)
        {
            exchange.response_bytes_received = true;
        }
        let action = match step {
            Ok(Ok(action)) => action,
            Ok(Err(error)) => {
                let kind = match &error {
                    Error::Protocol(error) => error.kind(),
                    _ => ProtocolErrorKind::InvalidState,
                };
                return Err(H2SharedFailure::Protocol(kind, error.to_string()));
            }
            Err(FsmServerError::PeerReset {
                stream_id,
                error_code,
            }) => {
                let consumed = driver.consumed();
                driver.consume(consumed).map_err(H2SharedFailure::Driver)?;
                input.consume(consumed);
                let Some(exchange_id) = exchanges
                    .keys()
                    .copied()
                    .find(|exchange_id| exchange_id.as_u64() == u64::from(stream_id))
                else {
                    return Err(H2SharedFailure::Driver(FsmServerError::PeerReset {
                        stream_id,
                        error_code,
                    }));
                };
                if let Some(exchange) = exchanges.get_mut(&exchange_id) {
                    let guaranteed_unprocessed = error_code == H2ErrorCode::RefusedStream.as_u32();
                    // RFC 9113 §8.7 guarantees REFUSED_STREAM was not
                    // processed, even for non-idempotent methods.
                    // `refused_stream_retries_non_idempotent_request_on_fresh_connection`
                    // pins this exception.
                    if guaranteed_unprocessed {
                        exchange.response_bytes_received = false;
                    }
                    exchange.send_failure(
                        H2SharedFailure::Driver(FsmServerError::PeerReset {
                            stream_id,
                            error_code,
                        })
                        .error(),
                        false,
                        guaranteed_unprocessed,
                    );
                }
                retire_one_http2_exchange(session, pool, config, driver, exchanges, exchange_id)?;
                prefer_body = true;
                continue;
            }
            Err(error) => return Err(H2SharedFailure::Driver(error)),
        };
        let consumed = driver.consumed();

        match action {
            H2PumpAction::Write => {
                let owner = driver.pending_write_exchange();
                let bytes = driver.pending_write().ok_or_else(|| {
                    H2SharedFailure::Protocol(
                        ProtocolErrorKind::InvalidState,
                        "the connection driver did not retain pending output".to_owned(),
                    )
                })?;
                let mut wrote = false;
                let result = transport.write_with_progress(bytes, None, &mut wrote).await;
                if let Some(exchange_id) = owner
                    && let Some(exchange) = exchanges.get_mut(&exchange_id)
                {
                    exchange.request_bytes_written |= wrote;
                }
                result.map_err(H2SharedFailure::Io)?;
                driver.consume(consumed).map_err(H2SharedFailure::Driver)?;
                input.consume(consumed);
                prefer_body = true;
            }
            H2PumpAction::ResponseHead(exchange_id, head) => {
                driver.consume(consumed).map_err(H2SharedFailure::Driver)?;
                input.consume(consumed);
                handle_http2_response_head(driver, exchanges, exchange_id, head)?;
                prefer_body = true;
            }
            H2PumpAction::BufferedResponseBody => {
                driver.consume(consumed).map_err(H2SharedFailure::Driver)?;
                input.consume(consumed);
                prefer_body = true;
            }
            H2PumpAction::StreamingResponseBody(exchange_id, chunk) => {
                driver.consume(consumed).map_err(H2SharedFailure::Driver)?;
                input.consume(consumed);
                if deliver_http2_streaming_response_body(driver, exchanges, exchange_id, chunk)? {
                    operations::yield_io().await;
                }
                prefer_body = true;
            }
            H2PumpAction::ResponseTrailers(exchange_id, headers) => {
                driver.consume(consumed).map_err(H2SharedFailure::Driver)?;
                input.consume(consumed);
                handle_http2_response_trailers(exchanges, exchange_id, headers)?;
                prefer_body = true;
            }
            H2PumpAction::ResponseComplete(exchange_id) => {
                driver.consume(consumed).map_err(H2SharedFailure::Driver)?;
                input.consume(consumed);
                handle_http2_response_complete(driver, exchanges, exchange_id)?;
                prefer_body = true;
            }
            H2PumpAction::NeedInput => {
                driver.consume(consumed).map_err(H2SharedFailure::Driver)?;
                input.consume(consumed);
                if consumed != 0 {
                    prefer_body = true;
                    continue;
                }

                // Alternating protocol progress with one body chunk clears
                // `prefer_body`, but a sibling exchange may still be holding a
                // chunk the driver would accept. Parking now would strand it:
                // the body future below only wakes for exchanges that still
                // NEED a chunk, never for one that already has bytes waiting to
                // be written. Try once more, so parking means nothing is
                // writable rather than nothing was attempted.
                if write_http2_request_body(transport, driver, exchanges).await? {
                    prefer_body = false;
                    continue;
                }

                let event = {
                    let command = commands.recv().fuse();
                    let read = input.read_from_with_deadline(transport, None).fuse();
                    let body = wait_for_http2_request_body(exchanges).fuse();
                    let shutdown = session.shutdown.cancelled().fuse();
                    pin_mut!(command, read, body, shutdown);
                    select_biased! {
                        _ = shutdown => H2PumpWait::Shutdown,
                        input = read => H2PumpWait::Input(input),
                        command = command => H2PumpWait::Command(command),
                        body = body => H2PumpWait::Body(body),
                    }
                };
                match event {
                    H2PumpWait::Shutdown => return Ok(H2PumpStopReason::SessionShutdown),
                    H2PumpWait::Command(Ok(command)) => {
                        handle_http2_command(
                            session,
                            driver,
                            exchanges,
                            command,
                            config.connection_io_timeout(),
                        )?;
                    }
                    H2PumpWait::Command(Err(_)) => {
                        return Ok(H2PumpStopReason::SessionShutdown);
                    }
                    H2PumpWait::Body(Ok(())) => prefer_body = true,
                    H2PumpWait::Body(Err(failure)) => {
                        abandon_http2_exchange(
                            driver,
                            exchanges,
                            failure.exchange_id,
                            failure.error,
                        )?;
                    }
                    H2PumpWait::Input(Ok(0)) => {
                        return Err(H2SharedFailure::UnexpectedEof);
                    }
                    H2PumpWait::Input(Ok(_)) => {}
                    H2PumpWait::Input(Err(error)) => {
                        return Err(H2SharedFailure::from_error(error));
                    }
                }
            }
            H2PumpAction::Done => {
                retire_http2_exchanges(session, pool, config, driver, exchanges)?;
            }
        }
    }
}

fn handle_http2_command(
    session: &H2Session,
    driver: &mut ClientConnection,
    exchanges: &mut HashMap<ExchangeId, H2Exchange>,
    command: H2Command,
    io_timeout: Duration,
) -> std::result::Result<(), H2SharedFailure> {
    match command {
        H2Command::Submit(submission) => {
            if exchanges.len() >= driver.max_active_exchanges() {
                session.release_unstarted();
                let _ = submission
                    .reply
                    .try_send(H2SessionReply::Rejected(submission.request));
                return Ok(());
            }

            let headers = request_header_refs(&submission.request);
            let request_head = ClientRequest {
                method: submission.request.method().as_str(),
                scheme: submission.target.scheme(),
                authority: &submission.target.authority,
                target: &submission.target.request_target,
                headers: &headers,
                body_len: submission.request.body().known_len(),
            };
            let prepared = match submission.response_mode {
                ResponseMode::Buffered => driver.prepare_request(request_head),
                ResponseMode::Streaming => driver.prepare_request_streaming_response(request_head),
            };
            let exchange_id = match prepared {
                Ok(exchange_id) => exchange_id,
                Err(error) => {
                    session.release_unstarted();
                    let failure = H2SharedFailure::Driver(error.clone());
                    let _ = submission
                        .reply
                        .try_send(H2SessionReply::Failure(SendFailure {
                            error: failure.error(),
                            response_bytes_received: false,
                            request_bytes_written: false,
                            connection_failure: false,
                            guaranteed_unprocessed: false,
                        }));
                    return Err(H2SharedFailure::Driver(error));
                }
            };
            let body = submission.request.into_body();
            if exchanges
                .insert(
                    exchange_id,
                    H2Exchange {
                        request_id: submission.request_id,
                        response_mode: submission.response_mode,
                        reply: Some(submission.reply),
                        request_body: H2RequestBodyState::new(body),
                        response_head: None,
                        response_body: Vec::new(),
                        response_trailers: None,
                        completed_response: None,
                        body_sender: None,
                        streaming_trailers: None,
                        terminal_error: None,
                        io_timeout,
                        request_bytes_written: false,
                        response_bytes_received: false,
                        abandoned: false,
                    },
                )
                .is_some()
            {
                return Err(H2SharedFailure::Protocol(
                    ProtocolErrorKind::InvalidState,
                    "the HTTP/2 driver reused an active exchange identifier".to_owned(),
                ));
            }
            update_http2_capacity(session, driver);
            Ok(())
        }
        H2Command::Cancel(request_id) => {
            let Some(exchange_id) = exchanges.iter().find_map(|(exchange_id, exchange)| {
                (exchange.request_id == request_id).then_some(*exchange_id)
            }) else {
                return Ok(());
            };
            let exchange = exchanges
                .get_mut(&exchange_id)
                .expect("the canceled exchange remains active");
            exchange.abandoned = true;
            exchange.reply.take();
            exchange.body_sender.take();
            if !driver.exchange_is_retireable(exchange_id) {
                driver
                    .abandon_exchange(exchange_id)
                    .map_err(H2SharedFailure::Driver)?;
            }
            Ok(())
        }
    }
}

fn handle_http2_response_head(
    driver: &mut ClientConnection,
    exchanges: &mut HashMap<ExchangeId, H2Exchange>,
    exchange_id: ExchangeId,
    head: ReceivedResponseHead,
) -> std::result::Result<(), H2SharedFailure> {
    let exchange = exchanges.get_mut(&exchange_id).ok_or_else(|| {
        H2SharedFailure::Protocol(
            ProtocolErrorKind::UnexpectedEvent,
            "the response head named an unknown exchange".to_owned(),
        )
    })?;
    exchange.response_bytes_received = true;
    if exchange.abandoned {
        if !driver.exchange_is_retireable(exchange_id) {
            driver
                .abandon_exchange(exchange_id)
                .map_err(H2SharedFailure::Driver)?;
        }
        return Ok(());
    }
    if exchange.response_head.is_some() || exchange.body_sender.is_some() {
        return Err(H2SharedFailure::Protocol(
            ProtocolErrorKind::UnexpectedEvent,
            "the response head repeated for one exchange".to_owned(),
        ));
    }

    match exchange.response_mode {
        ResponseMode::Buffered => exchange.response_head = Some(head),
        ResponseMode::Streaming => {
            let (body_sender, receiver) = crate::async_channel();
            let terminal_error = Rc::new(RefCell::new(None));
            let trailers = InboundTrailers::streaming();
            let response = H2SessionReply::Response(H2SessionResponse::Streaming {
                head,
                body: H2ResponseBody {
                    receiver,
                    terminal_error: Rc::clone(&terminal_error),
                    trailers: trailers.clone(),
                    io_timeout: exchange.io_timeout,
                },
            });
            let Some(reply) = exchange.reply.take() else {
                exchange.abandoned = true;
                driver
                    .abandon_exchange(exchange_id)
                    .map_err(H2SharedFailure::Driver)?;
                return Ok(());
            };
            match reply.try_send(response) {
                Ok(()) => {
                    exchange.body_sender = Some(body_sender);
                    exchange.streaming_trailers = Some(trailers);
                    exchange.terminal_error = Some(terminal_error);
                }
                Err(_) => {
                    exchange.abandoned = true;
                    driver
                        .abandon_exchange(exchange_id)
                        .map_err(H2SharedFailure::Driver)?;
                }
            }
        }
    }
    Ok(())
}

fn prepare_http2_response_body(
    exchanges: &mut HashMap<ExchangeId, H2Exchange>,
    exchange_id: ExchangeId,
    chunk: &[u8],
) -> Result<Option<Vec<u8>>> {
    let exchange = exchanges.get_mut(&exchange_id).ok_or_else(|| {
        protocol(
            ProtocolErrorKind::UnexpectedEvent,
            "the response body named an unknown exchange",
        )
    })?;
    exchange.response_bytes_received = true;
    if exchange.abandoned {
        return Ok(None);
    }
    match exchange.response_mode {
        ResponseMode::Buffered => {
            if exchange.response_head.is_none() {
                return Err(protocol(
                    ProtocolErrorKind::UnexpectedEvent,
                    "the response body preceded its head",
                ));
            }
            exchange.response_body.extend_from_slice(chunk);
            Ok(None)
        }
        ResponseMode::Streaming => {
            if exchange.body_sender.is_none() {
                return Err(protocol(
                    ProtocolErrorKind::UnexpectedEvent,
                    "the streaming response body preceded its head",
                ));
            }
            Ok(Some(chunk.to_vec()))
        }
    }
}

fn deliver_http2_streaming_response_body(
    driver: &mut ClientConnection,
    exchanges: &mut HashMap<ExchangeId, H2Exchange>,
    exchange_id: ExchangeId,
    chunk: Vec<u8>,
) -> std::result::Result<bool, H2SharedFailure> {
    let exchange = exchanges.get_mut(&exchange_id).ok_or_else(|| {
        H2SharedFailure::Protocol(
            ProtocolErrorKind::UnexpectedEvent,
            "the streaming response body named an unknown exchange".to_owned(),
        )
    })?;
    if exchange.abandoned {
        return Ok(false);
    }
    let Some(sender) = exchange.body_sender.as_ref() else {
        return Err(H2SharedFailure::Protocol(
            ProtocolErrorKind::UnexpectedEvent,
            "the streaming response body preceded its head".to_owned(),
        ));
    };
    match sender.try_send(chunk) {
        Ok(()) => Ok(true),
        Err(crate::async_channel::SendError::ChannelClosed(_)) => {
            exchange.abandoned = true;
            exchange.body_sender.take();
            driver
                .abandon_exchange(exchange_id)
                .map_err(H2SharedFailure::Driver)?;
            Ok(false)
        }
        Err(crate::async_channel::SendError::ChannelFull(_)) => {
            if let Some(terminal_error) = exchange.terminal_error.as_ref() {
                *terminal_error.borrow_mut() = Some(protocol(
                    ProtocolErrorKind::InvalidState,
                    "the streaming response body outran its consumer and was truncated",
                ));
            }
            exchange.abandoned = true;
            exchange.body_sender.take();
            driver
                .abandon_exchange(exchange_id)
                .map_err(H2SharedFailure::Driver)?;
            Ok(false)
        }
    }
}

fn handle_http2_response_trailers(
    exchanges: &mut HashMap<ExchangeId, H2Exchange>,
    exchange_id: ExchangeId,
    headers: HeaderMap,
) -> std::result::Result<(), H2SharedFailure> {
    let exchange = exchanges.get_mut(&exchange_id).ok_or_else(|| {
        H2SharedFailure::Protocol(
            ProtocolErrorKind::UnexpectedEvent,
            "the response trailers named an unknown exchange".to_owned(),
        )
    })?;
    exchange.response_bytes_received = true;
    if exchange.abandoned {
        return Ok(());
    }
    match exchange.response_mode {
        ResponseMode::Buffered => {
            if exchange.response_head.is_none() {
                return Err(H2SharedFailure::Protocol(
                    ProtocolErrorKind::UnexpectedEvent,
                    "the response trailers preceded its head".to_owned(),
                ));
            }
            if exchange.response_trailers.replace(headers).is_some() {
                return Err(H2SharedFailure::Protocol(
                    ProtocolErrorKind::UnexpectedEvent,
                    "the response trailers repeated for one exchange".to_owned(),
                ));
            }
        }
        ResponseMode::Streaming => {
            let trailers = exchange.streaming_trailers.as_ref().ok_or_else(|| {
                H2SharedFailure::Protocol(
                    ProtocolErrorKind::UnexpectedEvent,
                    "the streaming response trailers preceded its head".to_owned(),
                )
            })?;
            trailers.set(headers).map_err(|_| {
                H2SharedFailure::Protocol(
                    ProtocolErrorKind::UnexpectedEvent,
                    "the streaming response trailers repeated for one exchange".to_owned(),
                )
            })?;
        }
    }
    Ok(())
}

fn handle_http2_response_complete(
    driver: &mut ClientConnection,
    exchanges: &mut HashMap<ExchangeId, H2Exchange>,
    exchange_id: ExchangeId,
) -> std::result::Result<(), H2SharedFailure> {
    let exchange = exchanges.get_mut(&exchange_id).ok_or_else(|| {
        H2SharedFailure::Protocol(
            ProtocolErrorKind::UnexpectedEvent,
            "the response completion named an unknown exchange".to_owned(),
        )
    })?;
    exchange.response_bytes_received = true;
    if !exchange.abandoned {
        match exchange.response_mode {
            ResponseMode::Buffered => {
                let head = exchange.response_head.take().ok_or_else(|| {
                    H2SharedFailure::Protocol(
                        ProtocolErrorKind::MalformedMessage,
                        "the response completed without headers".to_owned(),
                    )
                })?;
                let body = Body::from_inbound(
                    std::mem::take(&mut exchange.response_body),
                    exchange.response_trailers.take(),
                );
                let response = finish_response(Some(head), body).map_err(|error| {
                    H2SharedFailure::Protocol(ProtocolErrorKind::InvalidState, error.to_string())
                })?;
                exchange.completed_response = Some(response);
            }
            ResponseMode::Streaming => {
                exchange.body_sender.take();
            }
        }
    }
    if !driver.exchange_is_retireable(exchange_id) {
        // RFC 9113 §8.1 permits this final response before the request body
        // finishes. Stop offering body chunks while the queued reset makes the
        // exchange retireable. `early_http2_final_response_spares_sibling_streams`
        // pins both the response delivery and sibling-stream behavior.
        exchange.abandoned = true;
        driver
            .abandon_exchange(exchange_id)
            .map_err(H2SharedFailure::Driver)?;
    }
    Ok(())
}

fn abandon_http2_exchange(
    driver: &mut ClientConnection,
    exchanges: &mut HashMap<ExchangeId, H2Exchange>,
    exchange_id: ExchangeId,
    error: Error,
) -> std::result::Result<(), H2SharedFailure> {
    let exchange = exchanges.get_mut(&exchange_id).ok_or_else(|| {
        H2SharedFailure::Protocol(
            ProtocolErrorKind::InvalidState,
            "the failed request body named an unknown exchange".to_owned(),
        )
    })?;
    exchange.send_failure(error, false, false);
    exchange.abandoned = true;
    if !driver.exchange_is_retireable(exchange_id) {
        driver
            .abandon_exchange(exchange_id)
            .map_err(H2SharedFailure::Driver)?;
    }
    Ok(())
}

fn update_http2_capacity(session: &H2Session, driver: &ClientConnection) {
    session.state.borrow_mut().capacity = driver.max_active_exchanges();
}

fn retire_http2_exchanges(
    session: &H2Session,
    pool: &Weak<RefCell<PoolState>>,
    config: &ClientConfig,
    driver: &mut ClientConnection,
    exchanges: &mut HashMap<ExchangeId, H2Exchange>,
) -> std::result::Result<(), H2SharedFailure> {
    let ready = exchanges
        .keys()
        .copied()
        .filter(|exchange_id| driver.exchange_is_retireable(*exchange_id))
        .collect::<Vec<_>>();
    for exchange_id in ready {
        retire_one_http2_exchange(session, pool, config, driver, exchanges, exchange_id)?;
    }
    Ok(())
}

fn retire_one_http2_exchange(
    session: &H2Session,
    pool: &Weak<RefCell<PoolState>>,
    config: &ClientConfig,
    driver: &mut ClientConnection,
    exchanges: &mut HashMap<ExchangeId, H2Exchange>,
    exchange_id: ExchangeId,
) -> std::result::Result<(), H2SharedFailure> {
    driver
        .begin_next_exchange(exchange_id)
        .map_err(H2SharedFailure::Driver)?;
    let mut exchange = exchanges.remove(&exchange_id).ok_or_else(|| {
        H2SharedFailure::Protocol(
            ProtocolErrorKind::InvalidState,
            "the retired exchange was not active".to_owned(),
        )
    })?;
    let mut state = session.state.borrow_mut();
    state.active = state.active.saturating_sub(1);
    state.completed = state.completed.saturating_add(1);
    state.capacity = driver.max_active_exchanges();
    let became_idle = state.active == 0 && state.completed < state.max_requests;
    if became_idle {
        state.idle_since = Some(crate::clock_now());
    }
    drop(state);
    if became_idle {
        enforce_http2_idle_caps(pool, session, config);
    }
    if let Some(response) = exchange.completed_response.take()
        && let Some(reply) = exchange.reply.take()
    {
        let _ = reply.try_send(H2SessionReply::Response(H2SessionResponse::Buffered(
            response,
        )));
    }
    Ok(())
}

async fn poll_http2_request_bodies_once(
    exchanges: &mut HashMap<ExchangeId, H2Exchange>,
) -> Option<H2RequestBodyFailure> {
    std::future::poll_fn(|context| {
        for (exchange_id, exchange) in exchanges.iter_mut() {
            if exchange.abandoned || !exchange.request_body.needs_chunk() {
                continue;
            }
            if let Poll::Ready(Err(error)) = exchange.request_body.poll_chunk(context) {
                return Poll::Ready(Some(H2RequestBodyFailure {
                    exchange_id: *exchange_id,
                    error,
                }));
            }
        }
        Poll::Ready(None)
    })
    .await
}

async fn wait_for_http2_request_body(
    exchanges: &mut HashMap<ExchangeId, H2Exchange>,
) -> std::result::Result<(), H2RequestBodyFailure> {
    std::future::poll_fn(|context| {
        for (exchange_id, exchange) in exchanges.iter_mut() {
            if exchange.abandoned || !exchange.request_body.needs_chunk() {
                continue;
            }
            match exchange.request_body.poll_chunk(context) {
                Poll::Ready(Ok(())) => return Poll::Ready(Ok(())),
                Poll::Ready(Err(error)) => {
                    return Poll::Ready(Err(H2RequestBodyFailure {
                        exchange_id: *exchange_id,
                        error,
                    }));
                }
                Poll::Pending => {}
            }
        }
        Poll::Pending
    })
    .await
}

async fn write_http2_request_body(
    transport: &mut Transport,
    driver: &mut ClientConnection,
    exchanges: &mut HashMap<ExchangeId, H2Exchange>,
) -> std::result::Result<bool, H2SharedFailure> {
    let exchange_ids = exchanges.keys().copied().collect::<Vec<_>>();
    // Declare every stream's readiness before asking for a chunk. A streaming
    // request body has no declared length, so the driver's fair scheduler
    // otherwise cannot tell that the streams it is not being asked about are
    // waiting, and it keeps serving whichever one this loop reaches first -
    // letting a single upload monopolize the connection. Pinned by
    // `concurrent_streaming_request_bodies_rotate_fairly` in kimojio-fsm-http
    // and `concurrent_hyper_http2_streaming_uploads_share_the_connection`.
    for exchange_id in &exchange_ids {
        let available_len = exchanges
            .get(exchange_id)
            .filter(|exchange| !exchange.abandoned)
            .and_then(|exchange| exchange.request_body.available().map(<[u8]>::len))
            .unwrap_or(0);
        driver.note_request_body_available(*exchange_id, available_len);
    }
    for exchange_id in exchange_ids {
        let Some(available_len) = exchanges
            .get(&exchange_id)
            .filter(|exchange| !exchange.abandoned)
            .and_then(|exchange| exchange.request_body.available().map(<[u8]>::len))
        else {
            continue;
        };
        if !driver
            .prepare_body_chunk(exchange_id, available_len)
            .map_err(H2SharedFailure::Driver)?
        {
            continue;
        }

        let mut wrote = false;
        let payload_len = {
            let exchange = exchanges
                .get(&exchange_id)
                .expect("the selected request body remains active");
            let available = exchange
                .request_body
                .available()
                .expect("the selected request body remains available");
            let chunk = driver
                .body_chunk(exchange_id)
                .expect("a prepared request body chunk remains available");
            let payload_len = chunk.payload_len();
            let mut slices = [
                IoSlice::new(chunk.header()),
                IoSlice::new(&available[..payload_len]),
                IoSlice::new(chunk.footer()),
            ];
            let result = transport
                .writev_with_progress(&mut slices, None, &mut wrote)
                .await;
            if let Some(exchange) = exchanges.get_mut(&exchange_id) {
                exchange.request_bytes_written |= wrote;
            }
            result.map_err(H2SharedFailure::Io)?;
            payload_len
        };
        driver
            .commit_body_chunk(exchange_id)
            .map_err(H2SharedFailure::Driver)?;
        exchanges
            .get_mut(&exchange_id)
            .expect("the committed request body remains active")
            .request_body
            .commit(payload_len);
        return Ok(true);
    }
    Ok(false)
}

async fn send(
    pooled: &mut PooledConnection,
    request: &Request<Body>,
    target: &UriTarget,
    wire_protocol: Protocol,
    expect_continue_timeout: Duration,
    response_mode: ResponseMode,
) -> std::result::Result<SentResponse, SendFailure> {
    debug_assert_eq!(pooled.driver.protocol(), fsm_protocol(wire_protocol));
    let PooledConnection {
        transport,
        driver: connection,
        input,
        current_exchange,
        ..
    } = pooled;
    if current_exchange.is_some() {
        return Err(SendFailure::other(
            protocol(
                ProtocolErrorKind::InvalidState,
                "the connection already has an active exchange",
            ),
            !input.available().is_empty(),
            false,
        ));
    }
    let mut response_bytes_received = !input.available().is_empty();
    let mut request_bytes_written = false;
    let headers = request_header_refs(request);
    let request_head = ClientRequest {
        method: request.method().as_str(),
        scheme: target.scheme(),
        authority: &target.authority,
        target: &target.request_target,
        headers: &headers,
        body_len: request.body().known_len(),
    };
    let prepared = match response_mode {
        ResponseMode::Buffered => connection.prepare_request(request_head),
        ResponseMode::Streaming => connection.prepare_request_streaming_response(request_head),
    };
    let exchange_id = prepared.into_http().map_err(|error| {
        SendFailure::other(error, response_bytes_received, request_bytes_written)
    })?;
    *current_exchange = Some(exchange_id);
    let mut response_head = None;
    let mut body = Vec::new();
    let mut response_trailers = None;
    let mut request_body = RequestBodyState::new(request.body());
    let mut continue_deadline = None;

    loop {
        if request_body.needs_chunk() {
            let polled =
                std::future::poll_fn(|context| Poll::Ready(request_body.poll_chunk(context))).await;
            if let Poll::Ready(result) = polled {
                result.map_err(|error| {
                    SendFailure::other(error, response_bytes_received, request_bytes_written)
                })?;
            }
        }

        if let Some(available) = request_body.available()
            && connection
                .prepare_body_chunk(exchange_id, available.len())
                .into_http()
                .map_err(|error| {
                    SendFailure::other(error, response_bytes_received, request_bytes_written)
                })?
        {
            let chunk = connection
                .body_chunk(exchange_id)
                .expect("a prepared body chunk remains available");
            let payload_len = chunk.payload_len();
            let mut slices = [
                IoSlice::new(chunk.header()),
                IoSlice::new(&available[..payload_len]),
                IoSlice::new(chunk.footer()),
            ];
            transport
                .writev_with_progress(&mut slices, None, &mut request_bytes_written)
                .await
                .map_err(|error| {
                    SendFailure::connection(
                        error.into(),
                        response_bytes_received,
                        request_bytes_written,
                    )
                })?;
            connection
                .commit_body_chunk(exchange_id)
                .into_http()
                .map_err(|error| {
                    SendFailure::other(error, response_bytes_received, request_bytes_written)
                })?;
            request_body.commit(payload_len);
            continue;
        }

        let action = connection
            .step(input.available(), |step| -> Result<PumpAction> {
                match step {
                    Step::NeedInput => Ok(PumpAction::NeedInput),
                    Step::Write(_) => Ok(PumpAction::Write),
                    Step::Done => Ok(PumpAction::Complete),
                    Step::Event(DriverClientEvent::ResponseHead {
                        exchange_id: observed_exchange,
                        status,
                        version,
                        headers,
                        content_length: _,
                    }) if observed_exchange == exchange_id => {
                        let status = StatusCode::from_u16(status).map_err(|error| {
                            protocol(
                                ProtocolErrorKind::MalformedMessage,
                                format!("invalid response status: {error}"),
                            )
                        })?;
                        let headers = convert_headers(headers)?;
                        body = Vec::new();
                        response_head = Some((status, version, headers));
                        Ok(if response_mode == ResponseMode::Streaming {
                            PumpAction::ResponseHead
                        } else {
                            PumpAction::Progress
                        })
                    }
                    Step::Event(DriverClientEvent::ResponseBody {
                        exchange_id: observed_exchange,
                        chunk,
                    }) if observed_exchange == exchange_id => {
                        body.extend_from_slice(chunk);
                        Ok(PumpAction::Progress)
                    }
                    Step::Event(DriverClientEvent::ResponseTrailers {
                        exchange_id: observed_exchange,
                        headers,
                    }) if observed_exchange == exchange_id => {
                        if response_head.is_none() {
                            return Err(protocol(
                                ProtocolErrorKind::UnexpectedEvent,
                                "response trailers preceded its head",
                            ));
                        }
                        if response_trailers.is_some() {
                            return Err(protocol(
                                ProtocolErrorKind::UnexpectedEvent,
                                "response trailers repeated for one exchange",
                            ));
                        }
                        response_trailers = Some(convert_headers(headers)?);
                        Ok(PumpAction::Progress)
                    }
                    Step::Event(DriverClientEvent::ResponseComplete {
                        exchange_id: observed_exchange,
                    }) if observed_exchange == exchange_id => Ok(PumpAction::Complete),
                    _ => Err(protocol(
                        ProtocolErrorKind::UnexpectedEvent,
                        "the connection driver returned an unexpected client step",
                    )),
                }
            })
            .into_http()
            .map_err(|error| {
                SendFailure::other(error, response_bytes_received, request_bytes_written)
            })?
            .map_err(|error| {
                SendFailure::other(error, response_bytes_received, request_bytes_written)
            })?;
        let consumed = connection.consumed();

        match action {
            PumpAction::Write => {
                let bytes = connection.pending_write().ok_or_else(|| {
                    protocol(
                        ProtocolErrorKind::InvalidState,
                        "the connection driver did not retain pending output",
                    )
                });
                let bytes = bytes.map_err(|error| {
                    SendFailure::other(error, response_bytes_received, request_bytes_written)
                })?;
                transport
                    .write_with_progress(bytes, None, &mut request_bytes_written)
                    .await
                    .map_err(|error| {
                        SendFailure::connection(
                            error.into(),
                            response_bytes_received,
                            request_bytes_written,
                        )
                    })?;
                connection.consume(consumed).into_http().map_err(|error| {
                    SendFailure::other(error, response_bytes_received, request_bytes_written)
                })?;
                input.consume(consumed);
            }
            PumpAction::Progress => {
                connection.consume(consumed).into_http().map_err(|error| {
                    SendFailure::other(error, response_bytes_received, request_bytes_written)
                })?;
                input.consume(consumed);
            }
            PumpAction::ResponseHead => {
                connection.consume(consumed).into_http().map_err(|error| {
                    SendFailure::other(error, response_bytes_received, request_bytes_written)
                })?;
                input.consume(consumed);
                return Ok(SentResponse::Streaming(response_head.take().ok_or_else(
                    || {
                        SendFailure::other(
                            protocol(
                                ProtocolErrorKind::MalformedMessage,
                                "response head event carried no response head",
                            ),
                            response_bytes_received,
                            request_bytes_written,
                        )
                    },
                )?));
            }
            PumpAction::NeedInput => {
                connection.consume(consumed).into_http().map_err(|error| {
                    SendFailure::other(error, response_bytes_received, request_bytes_written)
                })?;
                input.consume(consumed);
                if consumed == 0 {
                    let deadline = if connection.is_waiting_for_continue(exchange_id) {
                        match continue_deadline {
                            Some(deadline) => Some(deadline),
                            None => {
                                let deadline = crate::clock_now()
                                    .checked_add(expect_continue_timeout)
                                    .ok_or_else(|| {
                                        SendFailure::other(
                                            Error::InvalidConfiguration(
                                                "Expect: 100-continue timeout is too large",
                                            ),
                                            response_bytes_received,
                                            request_bytes_written,
                                        )
                                    })?;
                                continue_deadline = Some(deadline);
                                Some(deadline)
                            }
                        }
                    } else {
                        None
                    };
                    let wait = if request_body.needs_chunk()
                        && !connection.is_waiting_for_continue(exchange_id)
                    {
                        let read = input.read_from_with_deadline(transport, deadline).fuse();
                        let body =
                            std::future::poll_fn(|context| request_body.poll_chunk(context)).fuse();
                        pin_mut!(read, body);
                        select_biased! {
                            result = read => WaitAction::Input(result),
                            result = body => WaitAction::Body(result),
                        }
                    } else {
                        WaitAction::Input(input.read_from_with_deadline(transport, deadline).await)
                    };
                    let read = match wait {
                        WaitAction::Input(result) => result,
                        WaitAction::Body(Ok(())) => continue,
                        WaitAction::Body(Err(error)) => {
                            return Err(SendFailure::other(
                                error,
                                response_bytes_received,
                                request_bytes_written,
                            ));
                        }
                    };
                    match read {
                        Ok(0) => {
                            connection.finish_eof().into_http().map_err(|error| {
                                SendFailure::connection(
                                    error,
                                    response_bytes_received,
                                    request_bytes_written,
                                )
                            })?;
                        }
                        Ok(_) => response_bytes_received = true,
                        Err(Error::Io(error))
                            if connection.is_waiting_for_continue(exchange_id)
                                && matches!(error, crate::Errno::TIME | crate::Errno::TIMEDOUT) =>
                        {
                            connection
                                .proceed_with_body(exchange_id)
                                .into_http()
                                .map_err(|error| {
                                    SendFailure::other(
                                        error,
                                        response_bytes_received,
                                        request_bytes_written,
                                    )
                                })?;
                            continue_deadline = None;
                        }
                        Err(error) => {
                            let connection_failure = matches!(error, Error::Io(_));
                            return Err(if connection_failure {
                                SendFailure::connection(
                                    error,
                                    response_bytes_received,
                                    request_bytes_written,
                                )
                            } else {
                                SendFailure::other(
                                    error,
                                    response_bytes_received,
                                    request_bytes_written,
                                )
                            });
                        }
                    };
                }
            }
            PumpAction::Complete => {
                connection.consume(consumed).into_http().map_err(|error| {
                    SendFailure::other(error, response_bytes_received, request_bytes_written)
                })?;
                input.consume(consumed);
                return finish_response(response_head, Body::from_inbound(body, response_trailers))
                    .map(SentResponse::Buffered)
                    .map_err(|error| {
                        SendFailure::other(error, response_bytes_received, request_bytes_written)
                    });
            }
        }
    }
}

struct StreamingResponseState {
    client: Client,
    key: PoolKey,
    connection: Option<Box<PooledConnection>>,
    may_reuse_request_connection: bool,
    io_timeout: Duration,
    trailers: InboundTrailers,
}

fn finish_streaming_response(
    head: ReceivedResponseHead,
    state: StreamingResponseState,
) -> Result<Response<Body>> {
    let trailers = state.trailers.clone();
    let body = Body::from_inbound_stream(
        stream::try_unfold(state, |mut state| async move {
            let connection = state
                .connection
                .as_deref_mut()
                .expect("a live streaming response owns its connection");
            match pull_response_chunk(connection, state.io_timeout, &state.trailers).await {
                Ok(Some(chunk)) => Ok(Some((chunk, state))),
                Ok(None) => {
                    let connection = state
                        .connection
                        .take()
                        .expect("a completed streaming response owns its connection");
                    finish_response_connection(
                        &state.client,
                        state.key.clone(),
                        connection,
                        state.may_reuse_request_connection,
                    )
                    .await?;
                    Ok(None)
                }
                Err(error) => Err(error),
            }
        }),
        trailers,
    );
    finish_response(Some(head), body)
}

async fn pull_response_chunk(
    connection: &mut PooledConnection,
    io_timeout: Duration,
    trailers: &InboundTrailers,
) -> Result<Option<Vec<u8>>> {
    let exchange_id = connection.current_exchange.ok_or_else(|| {
        protocol(
            ProtocolErrorKind::InvalidState,
            "the streaming connection has no current exchange",
        )
    })?;
    loop {
        enum PullAction {
            Progress,
            NeedInput,
            Write,
            Body(Vec<u8>),
            Trailers(HeaderMap),
            Complete,
        }

        let action = connection
            .driver
            .step(connection.input.available(), |step| -> Result<PullAction> {
                match step {
                    Step::NeedInput => Ok(PullAction::NeedInput),
                    Step::Write(_) => Ok(PullAction::Write),
                    Step::Done => Ok(PullAction::Complete),
                    Step::Event(DriverClientEvent::ResponseComplete {
                        exchange_id: observed_exchange,
                    }) if observed_exchange == exchange_id => Ok(PullAction::Complete),
                    Step::Event(DriverClientEvent::ResponseBody {
                        exchange_id: observed_exchange,
                        chunk,
                    }) if observed_exchange == exchange_id => Ok(PullAction::Body(chunk.to_vec())),
                    Step::Event(DriverClientEvent::ResponseTrailers {
                        exchange_id: observed_exchange,
                        headers,
                    }) if observed_exchange == exchange_id => {
                        Ok(PullAction::Trailers(convert_headers(headers)?))
                    }
                    Step::Event(DriverClientEvent::ResponseHead {
                        exchange_id: observed_exchange,
                        ..
                    }) if observed_exchange == exchange_id => Err(protocol(
                        ProtocolErrorKind::UnexpectedEvent,
                        "response head repeated while streaming its body",
                    )),
                    Step::Event(
                        DriverClientEvent::ResponseHead { .. }
                        | DriverClientEvent::ResponseBody { .. }
                        | DriverClientEvent::ResponseTrailers { .. }
                        | DriverClientEvent::ResponseComplete { .. },
                    ) => Err(protocol(
                        ProtocolErrorKind::UnexpectedEvent,
                        "the connection driver returned an event for another exchange",
                    )),
                    _ => Ok(PullAction::Progress),
                }
            })
            .into_http()??;
        let consumed = connection.driver.consumed();
        match action {
            PullAction::Progress => {
                connection.driver.consume(consumed).into_http()?;
                connection.input.consume(consumed);
            }
            PullAction::Write => {
                let bytes = connection.driver.pending_write().ok_or_else(|| {
                    protocol(
                        ProtocolErrorKind::InvalidState,
                        "the connection driver did not retain pending output",
                    )
                })?;
                connection
                    .transport
                    .write(bytes, Some(streaming_io_deadline(io_timeout)?))
                    .await?;
                connection.driver.consume(consumed).into_http()?;
                connection.input.consume(consumed);
            }
            PullAction::Body(chunk) => {
                connection.driver.consume(consumed).into_http()?;
                connection.input.consume(consumed);
                return Ok(Some(chunk));
            }
            PullAction::Trailers(headers) => {
                trailers.set(headers).map_err(|_| {
                    protocol(
                        ProtocolErrorKind::UnexpectedEvent,
                        "response trailers repeated for one streaming body",
                    )
                })?;
                connection.driver.consume(consumed).into_http()?;
                connection.input.consume(consumed);
            }
            PullAction::Complete => {
                connection.driver.consume(consumed).into_http()?;
                connection.input.consume(consumed);
                return Ok(None);
            }
            PullAction::NeedInput => {
                connection.driver.consume(consumed).into_http()?;
                connection.input.consume(consumed);
                if consumed != 0 {
                    continue;
                }
                let read = connection
                    .input
                    .read_from_with_deadline(
                        &mut connection.transport,
                        Some(streaming_io_deadline(io_timeout)?),
                    )
                    .await?;
                if read == 0 {
                    connection.driver.finish_eof().into_http()?;
                }
            }
        }
    }
}

fn streaming_io_deadline(timeout: Duration) -> Result<Instant> {
    crate::clock_now()
        .checked_add(timeout)
        .ok_or(Error::InvalidConfiguration(
            "connection I/O timeout is too large for the current clock",
        ))
}

fn convert_headers(source: HeaderBlock<'_>) -> Result<HeaderMap> {
    let mut headers = HeaderMap::with_capacity(source.len());
    for header in source.iter() {
        let name = HeaderName::from_bytes(header.name()).map_err(Error::InvalidHeaderName)?;
        let value = HeaderValue::from_bytes(header.value()).map_err(Error::InvalidHeaderValue)?;
        headers.append(name, value);
    }
    Ok(headers)
}

fn finish_response(head: Option<ReceivedResponseHead>, body: Body) -> Result<Response<Body>> {
    let (status, version, headers) = head.ok_or_else(|| {
        protocol(
            ProtocolErrorKind::MalformedMessage,
            "response completed without headers",
        )
    })?;
    let version = match version {
        HttpVersion::Http10 => Version::HTTP_10,
        HttpVersion::Http11 => Version::HTTP_11,
        HttpVersion::Http2 => Version::HTTP_2,
        _ => {
            return Err(protocol(
                ProtocolErrorKind::InvalidState,
                "the connection driver returned an unknown HTTP version",
            ));
        }
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.version_mut() = version;
    *response.headers_mut() = headers;
    Ok(response)
}

fn protocol(kind: ProtocolErrorKind, detail: impl Into<String>) -> Error {
    Error::Protocol(ProtocolError::new(kind, detail))
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, TcpListener};
    use std::thread;

    use super::*;
    use crate::http::IntoHttpError;

    #[test]
    fn builder_supports_general_methods_headers_body_and_versions() {
        let request = Client::new()
            .request(Method::PATCH, "http://example.test/resource?q=1")
            .header("x-request-id", "42")
            .version(Version::HTTP_2)
            .body("updated")
            .build()
            .unwrap();

        assert_eq!(request.method(), Method::PATCH);
        assert_eq!(request.version(), Version::HTTP_2);
        assert_eq!(request.headers()["x-request-id"], "42");
        assert_eq!(request.headers()[CONTENT_LENGTH], "7");
        assert_eq!(request.body().as_bytes(), b"updated");
    }

    #[test]
    fn builder_defaults_to_persistent_http11() {
        let request = Client::new()
            .get("http://example.test/path")
            .build()
            .unwrap();

        assert_eq!(request.version(), Version::HTTP_11);
        assert_eq!(request.headers()[HOST], "example.test");
        assert!(!request.headers().contains_key(CONNECTION));
    }

    #[test]
    fn builder_rejects_unsupported_versions_and_oversized_bodies() {
        assert!(matches!(
            Client::new()
                .get("http://example.test")
                .version(Version::HTTP_10)
                .build(),
            Err(Error::UnsupportedVersion(Version::HTTP_10))
        ));

        let client = Client::with_config(
            ClientConfig::new().set_limits(Limits::new().set_max_body_bytes(3)),
        )
        .unwrap();
        assert!(matches!(
            client.post("http://example.test").body("four").build(),
            Err(Error::BodyTooLarge {
                limit: 3,
                actual: Some(4)
            })
        ));
    }

    #[test]
    fn builder_enforces_header_count_and_http2_rules() {
        let client =
            Client::with_config(ClientConfig::new().set_limits(Limits::new().set_max_headers(1)))
                .unwrap();
        assert!(matches!(
            client.get("http://example.test").build(),
            Err(Error::TooManyHeaders { limit: 1, .. })
        ));

        assert!(matches!(
            Client::new()
                .get("http://example.test")
                .version(Version::HTTP_2)
                .header(CONNECTION, "close")
                .build(),
            Err(Error::Protocol(error))
                if error.kind() == ProtocolErrorKind::InvalidHeader
        ));
    }

    #[test]
    fn fsm_error_mapping_has_stable_kind_and_private_diagnostic_source() {
        let Error::Protocol(error) = kimojio_fsm_http::Error::Parse.into_http_error() else {
            panic!("parse failures must remain protocol errors");
        };

        assert_eq!(error.kind(), ProtocolErrorKind::MalformedMessage);
        assert!(error.source().is_some());
        assert!(!error.to_string().contains("Parse"));
        assert!(!format!("{error:?}").contains("kimojio_fsm_http"));
    }

    #[test]
    fn request_target_and_pseudo_headers_count_toward_header_limit() {
        let client = Client::with_config(
            ClientConfig::new().set_limits(Limits::new().set_max_header_bytes(40)),
        )
        .unwrap();
        assert!(matches!(
            client
                .get("http://example.test/a/very/long/request/target")
                .version(Version::HTTP_2)
                .build(),
            Err(Error::HeadersTooLarge {
                limit: 40,
                actual: Some(_)
            })
        ));
    }

    #[test]
    fn a_written_request_blocks_non_idempotent_retry() {
        // This exercises the predicate directly rather than the `Expect` path:
        // it pins that *any* failure after request bytes were written refuses to
        // replay a non-idempotent method. Writing an `Expect` head is one way to
        // reach that state, but this test does not drive it.
        let failure = SendFailure::connection(Error::Io(crate::Errno::CONNRESET), false, true);

        assert!(!failure.can_retry_on_fresh_connection(&Method::POST));
        assert!(failure.can_retry_on_fresh_connection(&Method::PUT));
    }

    #[test]
    fn dropping_unstarted_http2_slot_releases_reservation() {
        let uri = "http://example.test/".parse().unwrap();
        let target = UriTarget::parse(&uri).unwrap();
        let pool = ConnectionPool::new();
        let (session, _commands) = H2Session::new(
            1,
            PoolKey::new(
                &target,
                Protocol::Http2PriorKnowledge,
                Protocol::Http2PriorKnowledge,
            ),
            Limits::new(),
            true,
            0,
        );

        let slot = session.try_reserve(pool).unwrap();
        assert_eq!(session.state.borrow().active, 1);
        drop(slot);
        assert_eq!(session.state.borrow().active, 0);
    }

    #[test]
    fn buffered_http2_body_avoids_owned_pump_action() {
        let mut driver =
            ClientConnection::new(HttpProtocol::Http2, kimojio_fsm_http::HttpLimits::new());
        let exchange_id = driver
            .prepare_request(ClientRequest {
                method: "GET",
                scheme: "http",
                authority: "example.test",
                target: "/",
                headers: &[],
                body_len: Some(0),
            })
            .unwrap();
        let mut exchanges = HashMap::from([(
            exchange_id,
            H2Exchange {
                request_id: 1,
                response_mode: ResponseMode::Buffered,
                reply: None,
                request_body: H2RequestBodyState::new(Body::new(Vec::new())),
                response_head: Some((StatusCode::OK, HttpVersion::Http2, HeaderMap::new())),
                response_body: Vec::new(),
                response_trailers: None,
                completed_response: None,
                body_sender: None,
                streaming_trailers: None,
                terminal_error: None,
                io_timeout: Duration::ZERO,
                request_bytes_written: true,
                response_bytes_received: false,
                abandoned: false,
            },
        )]);

        let owned = prepare_http2_response_body(&mut exchanges, exchange_id, b"borrowed").unwrap();

        assert!(
            owned.is_none(),
            "buffered DATA must not become an owned pump action"
        );
        let exchange = exchanges.get(&exchange_id).unwrap();
        assert_eq!(exchange.response_body, b"borrowed");
        assert!(exchange.response_bytes_received);
    }

    #[crate::test]
    async fn https_without_tls_configuration_returns_a_clear_error() {
        let error = Client::new()
            .get("https://127.0.0.1/")
            .send()
            .await
            .unwrap_err();
        assert!(matches!(error, Error::TlsNotConfigured));
    }

    #[crate::test]
    async fn http1_content_length_response_preserves_http10_version() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let peer = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 1024];
            let mut amount = 0;
            while !request[..amount]
                .windows(4)
                .any(|bytes| bytes == b"\r\n\r\n")
            {
                amount += stream.read(&mut request[amount..]).unwrap();
            }
            stream
                .write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 4\r\n\r\nbody")
                .unwrap();
        });

        let response = Client::new()
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.version(), Version::HTTP_10);
        assert_eq!(response.body().as_bytes(), b"body");
        peer.join().unwrap();
    }
}
