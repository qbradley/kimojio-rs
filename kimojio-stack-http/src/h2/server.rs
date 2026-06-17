// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::{BTreeMap, VecDeque};

use bytes::Bytes;
use http::{Request, Response};
use kimojio_stack::Errno;
use kimojio_stack::{IoFd, RuntimeSocket};

use crate::{
    Body, BodyBuilder, BodyLimits, Error, HttpConfig, HttpRuntime, RuntimeStackTransport, Trailers,
};

use super::{
    ConnectionState, Frame, FrameFlags, FramePayload, FrameType, Header, Settings, StreamId, codec,
    connection_window_target,
};

/// Completed inbound HTTP/2 request with its stream ID.
#[derive(Debug)]
pub struct IncomingRequest {
    /// Stream ID that must be used when sending the response.
    pub stream_id: StreamId,
    /// Request head and buffered body.
    pub request: Request<Body>,
}

/// Inbound request headers for a request body stream.
#[derive(Debug)]
pub struct IncomingRequestHead {
    /// Stream ID used to read request DATA events and send the response.
    pub stream_id: StreamId,
    /// Request head with an empty body marker.
    pub request: Request<()>,
}

/// Incremental request event returned by [`RuntimeServerConnection::read_request_event`].
#[derive(Debug, PartialEq, Eq)]
pub enum RequestStreamEvent {
    /// One DATA payload for the request stream.
    Data(Bytes),
    /// Terminal request trailers.
    ///
    /// Empty trailers represent a DATA frame with `END_STREAM` and no trailer
    /// HEADERS.
    Trailers(Trailers),
}

struct QueuedRequestStreamEvent {
    event: RequestStreamEvent,
    data_len: usize,
    update_stream_window: bool,
}

struct PendingRequest {
    headers: Option<Vec<Header>>,
    body: Option<BodyBuilder>,
    events: VecDeque<QueuedRequestStreamEvent>,
    head_delivered: bool,
}

impl PendingRequest {
    fn new(limits: BodyLimits) -> Self {
        Self {
            headers: None,
            body: Some(BodyBuilder::new(limits)),
            events: VecDeque::new(),
            head_delivered: false,
        }
    }

    fn is_streaming(&self) -> bool {
        self.body.is_none()
    }

    fn push_data_event(&mut self, data: Bytes, update_stream_window: bool) {
        let data_len = data.len();
        if data_len != 0 {
            self.events.push_back(QueuedRequestStreamEvent {
                event: RequestStreamEvent::Data(data),
                data_len,
                update_stream_window,
            });
        }
    }

    fn push_precredited_data_event(&mut self, data: Bytes) {
        if !data.is_empty() {
            self.events.push_back(QueuedRequestStreamEvent {
                event: RequestStreamEvent::Data(data),
                data_len: 0,
                update_stream_window: false,
            });
        }
    }

    fn push_trailers_event(&mut self, trailers: Trailers) {
        self.events.push_back(QueuedRequestStreamEvent {
            event: RequestStreamEvent::Trailers(trailers),
            data_len: 0,
            update_stream_window: false,
        });
    }
}

/// Low-level HTTP/2 server connection over a stack transport.
///
/// The server can accept one completed, buffered request at a time with
/// [`accept`](Self::accept), or it can accept only the request head with
/// [`accept_request_headers`](Self::accept_request_headers) and consume the body
/// incrementally with [`read_request_event`](Self::read_request_event).
/// Responses can be sent as a single buffered body with
/// [`send_response`](Self::send_response) or incrementally with
/// [`send_response_headers`](Self::send_response_headers),
/// [`send_response_data`](Self::send_response_data), and
/// [`finish_response_stream`](Self::finish_response_stream).
pub struct RuntimeServerConnection<S> {
    transport: RuntimeStackTransport<S>,
    config: HttpConfig,
    state: ConnectionState,
    initialized: bool,
    pending: BTreeMap<StreamId, PendingRequest>,
    ready_heads: VecDeque<StreamId>,
    completed: VecDeque<IncomingRequest>,
    reset_streams: BTreeMap<StreamId, Error>,
    goaway: Option<Error>,
}

/// Stack-core HTTP/2 server compatibility alias.
pub type ServerConnection = RuntimeServerConnection<IoFd>;

impl<S> RuntimeServerConnection<S> {
    /// Creates a server connection with settings derived from [`HttpConfig`].
    pub fn new(transport: RuntimeStackTransport<S>, config: HttpConfig) -> Self {
        let settings = codec::settings_from_config(config);
        Self::new_with_settings(transport, config, settings)
    }

    /// Creates a server connection with explicit local HTTP/2 settings.
    pub fn new_with_settings(
        transport: RuntimeStackTransport<S>,
        config: HttpConfig,
        settings: Settings,
    ) -> Self {
        Self {
            transport,
            config,
            state: ConnectionState::new(settings).expect("valid HTTP/2 local settings"),
            initialized: false,
            pending: BTreeMap::new(),
            ready_heads: VecDeque::new(),
            completed: VecDeque::new(),
            reset_streams: BTreeMap::new(),
            goaway: None,
        }
    }

    /// Accepts the next complete request, or `Ok(None)` after peer EOF.
    ///
    /// Request bodies are currently buffered up to [`HttpConfig::max_body_bytes`].
    /// While waiting for a request, peer resets and GOAWAY frames update
    /// connection state and can cause the connection to finish.
    pub fn accept(&mut self, cx: &impl HttpRuntime<S>) -> Result<Option<IncomingRequest>, Error>
    where
        S: RuntimeSocket,
    {
        self.ensure_initialized(cx)?;
        if let Some(request) = self.completed.pop_front() {
            self.ready_heads.retain(|id| *id != request.stream_id);
            return Ok(Some(request));
        }
        loop {
            let frame = match codec::read_frame(
                cx,
                &mut self.transport,
                self.state.local_settings().max_frame_size,
            ) {
                Ok(frame) => frame,
                Err(Error::Eof) if self.pending.is_empty() => return Ok(None),
                Err(error) => return Err(error),
            };
            self.process_frame(cx, frame)?;
            if let Some(request) = self.completed.pop_front() {
                self.ready_heads.retain(|id| *id != request.stream_id);
                return Ok(Some(request));
            }
            if self.goaway.is_some() && self.pending.is_empty() {
                return Ok(None);
            }
        }
    }

    /// Accepts the next request head without buffering the request body.
    ///
    /// After this returns `Some`, callers must consume the matching body with
    /// [`read_request_event`](Self::read_request_event) before sending a response
    /// for protocols that require full request consumption.
    pub fn accept_request_headers(
        &mut self,
        cx: &impl HttpRuntime<S>,
    ) -> Result<Option<IncomingRequestHead>, Error>
    where
        S: RuntimeSocket,
    {
        self.ensure_initialized(cx)?;
        if let Some(head) = self.pop_request_head()? {
            return Ok(Some(head));
        }
        loop {
            let frame = match codec::read_frame(
                cx,
                &mut self.transport,
                self.state.local_settings().max_frame_size,
            ) {
                Ok(frame) => frame,
                Err(Error::Eof) if self.pending.is_empty() => return Ok(None),
                Err(error) => return Err(error),
            };
            self.process_frame(cx, frame)?;
            if let Some(head) = self.pop_request_head()? {
                return Ok(Some(head));
            }
            if self.goaway.is_some() && self.pending.is_empty() {
                return Ok(None);
            }
        }
    }

    /// Reads the next DATA or terminal trailers event for a request stream.
    pub fn read_request_event(
        &mut self,
        cx: &impl HttpRuntime<S>,
        stream_id: StreamId,
    ) -> Result<RequestStreamEvent, Error>
    where
        S: RuntimeSocket,
    {
        self.ensure_initialized(cx)?;
        if self
            .pending
            .get(&stream_id)
            .is_some_and(|pending| !pending.head_delivered)
        {
            return Err(Error::Protocol("request headers not read"));
        }

        loop {
            if let Some(event) = self.pop_request_streaming_event(cx, stream_id)? {
                return Ok(event);
            }
            if let Some(error) = self.reset_streams.remove(&stream_id) {
                self.pending.remove(&stream_id);
                return Err(error);
            }
            let frame = match codec::read_frame(
                cx,
                &mut self.transport,
                self.state.local_settings().max_frame_size,
            ) {
                Ok(frame) => frame,
                Err(error) => {
                    self.pending.remove(&stream_id);
                    return Err(error);
                }
            };
            self.process_frame(cx, frame)?;
        }
    }

    /// Sends a response with no trailers.
    pub fn send_response(
        &mut self,
        cx: &impl HttpRuntime<S>,
        stream_id: StreamId,
        response: &Response<Body>,
    ) -> Result<(), Error>
    where
        S: RuntimeSocket,
    {
        self.send_response_with_trailers(cx, stream_id, response, None)
    }

    /// Sends a response and optional terminal trailers.
    ///
    /// Non-empty trailers are sent as terminal HEADERS. Empty bodies without
    /// trailers complete in the response HEADERS frame.
    pub fn send_response_with_trailers(
        &mut self,
        cx: &impl HttpRuntime<S>,
        stream_id: StreamId,
        response: &Response<Body>,
        trailers: Option<&Trailers>,
    ) -> Result<(), Error>
    where
        S: RuntimeSocket,
    {
        self.ensure_initialized(cx)?;
        let has_trailers = trailers.is_some_and(|trailers| !trailers.is_empty());
        let body = response.body().as_bytes();
        let end_stream = body.is_empty() && !has_trailers;
        let headers = codec::response_headers(response)?;
        let block = self.state.encode_header_block(&headers);
        self.state
            .stream_mut(stream_id)
            .ok_or(Error::Protocol("unknown stream"))?
            .send_headers(end_stream)?;
        codec::write_header_block(
            cx,
            &mut self.transport,
            stream_id,
            Bytes::from(block),
            end_stream,
            self.state.peer_settings().max_frame_size,
        )?;
        if !body.is_empty() {
            self.write_data(cx, stream_id, body, !has_trailers)?;
        }
        if let Some(trailers) = trailers.filter(|trailers| !trailers.is_empty()) {
            let headers = codec::trailers_headers(trailers)?;
            let block = self.state.encode_header_block(&headers);
            self.state
                .stream_mut(stream_id)
                .ok_or(Error::Protocol("unknown stream"))?
                .send_headers(true)?;
            codec::write_header_block(
                cx,
                &mut self.transport,
                stream_id,
                Bytes::from(block),
                true,
                self.state.peer_settings().max_frame_size,
            )?;
        }
        self.state.remove_stream(stream_id);
        Ok(())
    }

    /// Starts an incremental response stream by sending response HEADERS.
    ///
    /// Call this once for a stream returned by [`accept`](Self::accept), before
    /// any [`send_response_data`](Self::send_response_data) or
    /// [`finish_response_stream`](Self::finish_response_stream) call. The
    /// response body type is `()` because DATA is provided incrementally.
    /// Stream state remains active until `finish_response_stream` or a reset.
    pub fn send_response_headers(
        &mut self,
        cx: &impl HttpRuntime<S>,
        stream_id: StreamId,
        response: &Response<()>,
    ) -> Result<(), Error>
    where
        S: RuntimeSocket,
    {
        self.ensure_initialized(cx)?;
        self.check_stream_reset(stream_id)?;
        let headers = codec::response_headers(response)?;
        let block = self.state.encode_header_block(&headers);
        self.state
            .stream_mut(stream_id)
            .ok_or(Error::Protocol("unknown stream"))?
            .send_headers(false)?;
        codec::write_header_block(
            cx,
            &mut self.transport,
            stream_id,
            Bytes::from(block),
            false,
            self.state.peer_settings().max_frame_size,
        )
    }

    /// Sends one DATA payload on an incremental response stream.
    ///
    /// This method preserves HTTP/2 flow control by blocking in the existing
    /// write path when outbound stream or connection capacity is unavailable.
    /// Peer resets observed while waiting for capacity are surfaced as errors.
    pub fn send_response_data(
        &mut self,
        cx: &impl HttpRuntime<S>,
        stream_id: StreamId,
        data: &[u8],
    ) -> Result<(), Error>
    where
        S: RuntimeSocket,
    {
        self.ensure_initialized(cx)?;
        self.check_stream_reset(stream_id)?;
        self.write_data(cx, stream_id, data, false)
    }

    /// Finishes an incremental response stream.
    ///
    /// Non-empty `trailers` are sent as terminal HEADERS with `END_STREAM`.
    /// Empty or absent trailers finish with an empty DATA frame carrying
    /// `END_STREAM`. Local stream state is removed after this succeeds.
    pub fn finish_response_stream(
        &mut self,
        cx: &impl HttpRuntime<S>,
        stream_id: StreamId,
        trailers: Option<&Trailers>,
    ) -> Result<(), Error>
    where
        S: RuntimeSocket,
    {
        self.ensure_initialized(cx)?;
        self.check_stream_reset(stream_id)?;
        if let Some(trailers) = trailers.filter(|trailers| !trailers.is_empty()) {
            let headers = codec::trailers_headers(trailers)?;
            let block = self.state.encode_header_block(&headers);
            self.state
                .stream_mut(stream_id)
                .ok_or(Error::Protocol("unknown stream"))?
                .send_headers(true)?;
            codec::write_header_block(
                cx,
                &mut self.transport,
                stream_id,
                Bytes::from(block),
                true,
                self.state.peer_settings().max_frame_size,
            )?;
        } else {
            self.state
                .stream_mut(stream_id)
                .ok_or(Error::Protocol("unknown stream"))?
                .send_data(true)?;
            codec::write_frame(
                cx,
                &mut self.transport,
                &Frame::data(stream_id, Bytes::new(), true),
            )?;
        }
        self.state.remove_stream(stream_id);
        Ok(())
    }

    /// Accepts one request, calls `handler`, and sends the returned response.
    ///
    /// Returns `Ok(false)` when the peer has closed and no request remains.
    pub fn serve_one(
        &mut self,
        cx: &impl HttpRuntime<S>,
        handler: impl FnOnce(Request<Body>) -> Result<Response<Body>, Error>,
    ) -> Result<bool, Error>
    where
        S: RuntimeSocket,
    {
        let Some(incoming) = self.accept(cx)? else {
            return Ok(false);
        };
        let response = handler(incoming.request)?;
        self.send_response(cx, incoming.stream_id, &response)?;
        Ok(true)
    }

    /// Sends `RST_STREAM` for `stream_id`.
    ///
    /// Use this when a request cannot be processed and the caller wants to abort
    /// only that stream while keeping the connection alive.
    pub fn reset_stream(
        &mut self,
        cx: &impl HttpRuntime<S>,
        stream_id: StreamId,
        error_code: u32,
    ) -> Result<(), Error>
    where
        S: RuntimeSocket,
    {
        self.ensure_initialized(cx)?;
        codec::write_frame(
            cx,
            &mut self.transport,
            &Frame::rst_stream(stream_id, error_code),
        )
    }

    /// Sends a GOAWAY frame.
    ///
    /// The caller supplies the last stream ID it will process and an HTTP/2 error
    /// code. The connection is not closed automatically.
    pub fn goaway(
        &mut self,
        cx: &impl HttpRuntime<S>,
        last_stream_id: u32,
        error_code: u32,
    ) -> Result<(), Error>
    where
        S: RuntimeSocket,
    {
        self.ensure_initialized(cx)?;
        codec::write_frame(
            cx,
            &mut self.transport,
            &Frame::goaway(last_stream_id, error_code, Bytes::new()),
        )
    }

    /// Closes the underlying transport.
    pub fn close(self, cx: &impl HttpRuntime<S>) -> Result<(), Error>
    where
        S: RuntimeSocket,
    {
        self.transport.close(cx)
    }

    /// Half-closes writes, drains peer data until EOF, then closes the transport.
    ///
    /// This is useful for tests and short-lived protocols where the server wants
    /// the client to observe a graceful end of writes before the socket is closed.
    pub fn shutdown_write_and_close_after_peer(
        mut self,
        cx: &impl HttpRuntime<S>,
    ) -> Result<(), Error>
    where
        S: RuntimeSocket,
    {
        if let Err(error) = self.transport.shutdown_write(cx) {
            if peer_closed(&error) {
                return self.transport.close(cx);
            }
            return Err(error);
        }
        let mut buffer = [0_u8; 1024];
        loop {
            match self.transport.read(cx, &mut buffer) {
                Ok(0) => return self.transport.close(cx),
                Ok(_) => continue,
                Err(error) if peer_closed(&error) => return self.transport.close(cx),
                Err(error) => return Err(error),
            }
        }
    }

    fn ensure_initialized(&mut self, cx: &impl HttpRuntime<S>) -> Result<(), Error>
    where
        S: RuntimeSocket,
    {
        if self.initialized {
            return Ok(());
        }

        codec::read_client_preface(cx, &mut self.transport, &mut self.state)?;
        let frame = codec::read_frame(
            cx,
            &mut self.transport,
            self.state.local_settings().max_frame_size,
        )?;
        if frame.frame_type != FrameType::Settings || frame.flags.contains(FrameFlags::ACK) {
            return Err(Error::Protocol("expected client SETTINGS"));
        }
        self.state.receive_settings(&frame)?;
        self.state
            .queue_connection_window_update_to_target(connection_window_target(self.config))?;
        codec::write_frame(
            cx,
            &mut self.transport,
            &codec::settings_frame(self.state.local_settings()),
        )?;
        codec::flush_pending(cx, &mut self.transport, &mut self.state)?;
        self.initialized = true;
        Ok(())
    }

    fn write_data(
        &mut self,
        cx: &impl HttpRuntime<S>,
        stream_id: StreamId,
        mut body: &[u8],
        end_stream_on_last: bool,
    ) -> Result<(), Error>
    where
        S: RuntimeSocket,
    {
        while !body.is_empty() {
            let capacity = self.state.outbound_capacity(stream_id)?;
            if capacity == 0 {
                let frame = codec::read_frame(
                    cx,
                    &mut self.transport,
                    self.state.local_settings().max_frame_size,
                )?;
                self.process_frame(cx, frame)?;
                self.check_stream_reset(stream_id)?;
                continue;
            }

            let chunk_len = body
                .len()
                .min(capacity)
                .min(self.state.peer_settings().max_frame_size as usize);
            let end_stream = end_stream_on_last && chunk_len == body.len();
            self.state.consume_outbound_window(stream_id, chunk_len)?;
            self.state
                .stream_mut(stream_id)
                .ok_or(Error::Protocol("unknown stream"))?
                .send_data(end_stream)?;
            codec::write_frame(
                cx,
                &mut self.transport,
                &Frame::data(
                    stream_id,
                    Bytes::copy_from_slice(&body[..chunk_len]),
                    end_stream,
                ),
            )?;
            body = &body[chunk_len..];
        }
        Ok(())
    }

    fn process_frame(&mut self, cx: &impl HttpRuntime<S>, frame: Frame) -> Result<(), Error>
    where
        S: RuntimeSocket,
    {
        match frame.frame_type {
            FrameType::Settings => {
                self.state.receive_settings(&frame)?;
                codec::flush_pending(cx, &mut self.transport, &mut self.state)
            }
            FrameType::WindowUpdate => self.state.receive_window_update(&frame),
            FrameType::Ping => {
                self.state.track_inbound_frame(&frame)?;
                if !frame.flags.contains(FrameFlags::ACK) {
                    let FramePayload::Ping(opaque) = frame.payload else {
                        return Err(Error::Protocol("expected PING payload"));
                    };
                    codec::write_frame(
                        cx,
                        &mut self.transport,
                        &Frame {
                            frame_type: FrameType::Ping,
                            flags: FrameFlags::ACK,
                            stream_id: 0,
                            payload: FramePayload::Ping(opaque),
                        },
                    )?;
                }
                Ok(())
            }
            FrameType::RstStream => {
                self.state.track_inbound_frame(&frame)?;
                let FramePayload::RstStream { error_code } = frame.payload else {
                    return Err(Error::Protocol("expected RST_STREAM payload"));
                };
                let stream_id = StreamId::new(frame.stream_id)?;
                self.state.remove_stream(stream_id);
                self.pending.remove(&stream_id);
                self.ready_heads.retain(|id| *id != stream_id);
                self.completed
                    .retain(|request| request.stream_id != stream_id);
                self.reset_streams
                    .insert(stream_id, Error::stream_reset(frame.stream_id, error_code));
                Ok(())
            }
            FrameType::Goaway => {
                self.state.track_inbound_frame(&frame)?;
                let FramePayload::Goaway {
                    last_stream_id,
                    error_code,
                    debug_data,
                } = frame.payload
                else {
                    return Err(Error::Protocol("expected GOAWAY payload"));
                };
                self.goaway = Some(Error::goaway(
                    last_stream_id,
                    error_code,
                    debug_data.to_vec(),
                ));
                Ok(())
            }
            FrameType::Headers => self.process_headers(cx, frame),
            FrameType::Data => self.process_data(cx, frame),
            FrameType::Continuation => {
                self.state.track_inbound_frame(&frame)?;
                Err(Error::Protocol("unexpected CONTINUATION frame"))
            }
        }
    }

    fn process_headers(&mut self, cx: &impl HttpRuntime<S>, frame: Frame) -> Result<(), Error>
    where
        S: RuntimeSocket,
    {
        let (stream_id, end_stream, block) =
            codec::collect_header_block(cx, &mut self.transport, &mut self.state, frame)?;
        if stream_id.get().is_multiple_of(2) {
            return Err(Error::Protocol("client stream id must be odd"));
        }
        self.state.open_inbound_stream(stream_id)?;
        let headers = self.state.decode_header_block(&block)?;
        let limits = BodyLimits::new(self.config.max_body_bytes);
        let pending = self
            .pending
            .entry(stream_id)
            .or_insert_with(|| PendingRequest::new(limits));

        if pending.headers.is_none() {
            self.state
                .stream_mut(stream_id)
                .ok_or(Error::Protocol("unknown stream"))?
                .receive_headers(true, end_stream)?;
            pending.headers = Some(headers);
            self.ready_heads.push_back(stream_id);
            if end_stream {
                if pending.is_streaming() {
                    self.complete_streaming_request(stream_id, Trailers::new())?;
                } else {
                    self.complete_request(stream_id)?;
                }
            }
        } else {
            if !end_stream {
                return Err(Error::Protocol("request trailers missing END_STREAM"));
            }
            self.state
                .stream_mut(stream_id)
                .ok_or(Error::Protocol("unknown stream"))?
                .receive_trailers(headers.clone())?;
            let trailers = codec::trailers_from_headers(headers)?;
            if pending.is_streaming() {
                self.complete_streaming_request(stream_id, trailers)?;
            } else {
                self.complete_request(stream_id)?;
            }
        }
        Ok(())
    }

    fn process_data(&mut self, cx: &impl HttpRuntime<S>, frame: Frame) -> Result<(), Error>
    where
        S: RuntimeSocket,
    {
        self.state.track_inbound_frame(&frame)?;
        let stream_id = StreamId::new(frame.stream_id)?;
        let end_stream = frame.flags.contains(FrameFlags::END_STREAM);
        let FramePayload::Data(data) = frame.payload else {
            return Err(Error::Protocol("expected DATA payload"));
        };
        let pending = self
            .pending
            .get_mut(&stream_id)
            .ok_or(Error::Protocol("DATA before request headers"))?;
        let limits = BodyLimits::new(self.config.max_body_bytes);
        self.state
            .stream_mut(stream_id)
            .ok_or(Error::Protocol("unknown stream"))?
            .receive_data(&data, end_stream, limits)?;
        let data_len = data.len();
        self.state.consume_inbound_connection_window(data_len)?;
        if let Some(body) = pending.body.as_mut() {
            let mut should_flush = self
                .state
                .queue_connection_window_update_to_target(connection_window_target(self.config))?;
            body.append(&data)?;
            should_flush |=
                !end_stream && self.state.queue_stream_window_update_to_target(stream_id)?;
            if should_flush {
                codec::flush_pending(cx, &mut self.transport, &mut self.state)?;
            }
        } else if !data.is_empty() {
            pending.push_data_event(data, !end_stream);
        }
        if end_stream {
            if self
                .pending
                .get(&stream_id)
                .is_some_and(PendingRequest::is_streaming)
            {
                self.complete_streaming_request(stream_id, Trailers::new())?;
            } else {
                self.complete_request(stream_id)?;
            }
        }
        Ok(())
    }

    fn complete_request(&mut self, stream_id: StreamId) -> Result<(), Error> {
        let pending = self
            .pending
            .remove(&stream_id)
            .ok_or(Error::Protocol("request state missing"))?;
        let headers = pending
            .headers
            .ok_or(Error::Protocol("request headers missing"))?;
        let body = pending
            .body
            .ok_or(Error::Protocol("request body state missing"))?
            .finish();
        let request = codec::request_from_headers(headers, body)?;
        self.completed
            .push_back(IncomingRequest { stream_id, request });
        Ok(())
    }

    fn complete_streaming_request(
        &mut self,
        stream_id: StreamId,
        trailers: Trailers,
    ) -> Result<(), Error> {
        self.pending
            .get_mut(&stream_id)
            .ok_or(Error::Protocol("request state missing"))?
            .push_trailers_event(trailers);
        Ok(())
    }

    fn pop_request_head(&mut self) -> Result<Option<IncomingRequestHead>, Error> {
        while let Some(stream_id) = self.ready_heads.pop_front() {
            if let Some(pending) = self.pending.get_mut(&stream_id) {
                if pending.head_delivered {
                    continue;
                }
                let headers = pending
                    .headers
                    .clone()
                    .ok_or(Error::Protocol("request headers missing"))?;
                pending.head_delivered = true;
                if let Some(body) = pending.body.take() {
                    let bytes = body.finish().into_bytes();
                    pending.push_precredited_data_event(bytes);
                }
                let request = codec::request_from_headers(headers, ())?;
                return Ok(Some(IncomingRequestHead { stream_id, request }));
            }
            if let Some(position) = self
                .completed
                .iter()
                .position(|request| request.stream_id == stream_id)
            {
                let completed = self
                    .completed
                    .remove(position)
                    .ok_or(Error::Protocol("completed request state missing"))?;
                let (parts, body) = completed.request.into_parts();
                let mut pending = PendingRequest {
                    headers: None,
                    body: None,
                    events: VecDeque::new(),
                    head_delivered: true,
                };
                pending.push_precredited_data_event(body.into_bytes());
                pending.push_trailers_event(Trailers::new());
                self.pending.insert(stream_id, pending);
                return Ok(Some(IncomingRequestHead {
                    stream_id,
                    request: Request::from_parts(parts, ()),
                }));
            }
        }
        Ok(None)
    }

    fn pop_request_streaming_event(
        &mut self,
        cx: &impl HttpRuntime<S>,
        stream_id: StreamId,
    ) -> Result<Option<RequestStreamEvent>, Error>
    where
        S: RuntimeSocket,
    {
        let Some(queued) = self
            .pending
            .get_mut(&stream_id)
            .and_then(|pending| pending.events.pop_front())
        else {
            return Ok(None);
        };

        if queued.data_len != 0 {
            let mut should_flush = self
                .state
                .queue_connection_window_update_to_target(connection_window_target(self.config))?;
            if queued.update_stream_window {
                should_flush |= self.state.queue_stream_window_update_to_target(stream_id)?;
            }
            if should_flush {
                codec::flush_pending(cx, &mut self.transport, &mut self.state)?;
            }
        }
        if matches!(queued.event, RequestStreamEvent::Trailers(_)) {
            self.pending.remove(&stream_id);
        }
        Ok(Some(queued.event))
    }

    fn check_stream_reset(&mut self, stream_id: StreamId) -> Result<(), Error> {
        if let Some(error) = self.reset_streams.remove(&stream_id) {
            Err(error)
        } else {
            Ok(())
        }
    }
}

fn peer_closed(error: &Error) -> bool {
    matches!(error, Error::Io(Errno::PIPE) | Error::Tls(Errno::PIPE))
}
