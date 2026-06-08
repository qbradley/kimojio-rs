// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::{BTreeMap, VecDeque};

use bytes::Bytes;
use http::{Request, Response};
use kimojio_stack::Errno;
use kimojio_stack::RuntimeContext;

use crate::{Body, BodyBuilder, BodyLimits, Error, HttpConfig, StackTransport, Trailers};

use super::{
    ConnectionState, Frame, FrameFlags, FramePayload, FrameType, Header, Settings, StreamId, codec,
};

/// Completed inbound HTTP/2 request with its stream ID.
#[derive(Debug)]
pub struct IncomingRequest {
    /// Stream ID that must be used when sending the response.
    pub stream_id: StreamId,
    /// Request head and buffered body.
    pub request: Request<Body>,
}

struct PendingRequest {
    headers: Option<Vec<Header>>,
    body: BodyBuilder,
}

impl PendingRequest {
    fn new(limits: BodyLimits) -> Self {
        Self {
            headers: None,
            body: BodyBuilder::new(limits),
        }
    }
}

/// Low-level HTTP/2 server connection over a stack transport.
///
/// The server accepts one completed request at a time with [`accept`](Self::accept).
/// Responses can be sent as a single buffered body with
/// [`send_response`](Self::send_response) or incrementally with
/// [`send_response_headers`](Self::send_response_headers),
/// [`send_response_data`](Self::send_response_data), and
/// [`finish_response_stream`](Self::finish_response_stream).
pub struct ServerConnection {
    transport: StackTransport,
    config: HttpConfig,
    state: ConnectionState,
    initialized: bool,
    pending: BTreeMap<StreamId, PendingRequest>,
    completed: VecDeque<IncomingRequest>,
    reset_streams: BTreeMap<StreamId, Error>,
    goaway: Option<Error>,
}

impl ServerConnection {
    /// Creates a server connection with settings derived from [`HttpConfig`].
    pub fn new(transport: StackTransport, config: HttpConfig) -> Self {
        let settings = codec::settings_from_config(config);
        Self::new_with_settings(transport, config, settings)
    }

    /// Creates a server connection with explicit local HTTP/2 settings.
    pub fn new_with_settings(
        transport: StackTransport,
        config: HttpConfig,
        settings: Settings,
    ) -> Self {
        Self {
            transport,
            config,
            state: ConnectionState::new(settings).expect("valid HTTP/2 local settings"),
            initialized: false,
            pending: BTreeMap::new(),
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
    pub fn accept(&mut self, cx: &RuntimeContext<'_>) -> Result<Option<IncomingRequest>, Error> {
        self.ensure_initialized(cx)?;
        if let Some(request) = self.completed.pop_front() {
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
                return Ok(Some(request));
            }
            if self.goaway.is_some() && self.pending.is_empty() {
                return Ok(None);
            }
        }
    }

    /// Sends a response with no trailers.
    pub fn send_response(
        &mut self,
        cx: &RuntimeContext<'_>,
        stream_id: StreamId,
        response: &Response<Body>,
    ) -> Result<(), Error> {
        self.send_response_with_trailers(cx, stream_id, response, None)
    }

    /// Sends a response and optional terminal trailers.
    ///
    /// Non-empty trailers are sent as terminal HEADERS. Empty bodies without
    /// trailers complete in the response HEADERS frame.
    pub fn send_response_with_trailers(
        &mut self,
        cx: &RuntimeContext<'_>,
        stream_id: StreamId,
        response: &Response<Body>,
        trailers: Option<&Trailers>,
    ) -> Result<(), Error> {
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
        cx: &RuntimeContext<'_>,
        stream_id: StreamId,
        response: &Response<()>,
    ) -> Result<(), Error> {
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
        cx: &RuntimeContext<'_>,
        stream_id: StreamId,
        data: &[u8],
    ) -> Result<(), Error> {
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
        cx: &RuntimeContext<'_>,
        stream_id: StreamId,
        trailers: Option<&Trailers>,
    ) -> Result<(), Error> {
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
        cx: &RuntimeContext<'_>,
        handler: impl FnOnce(Request<Body>) -> Result<Response<Body>, Error>,
    ) -> Result<bool, Error> {
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
        cx: &RuntimeContext<'_>,
        stream_id: StreamId,
        error_code: u32,
    ) -> Result<(), Error> {
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
        cx: &RuntimeContext<'_>,
        last_stream_id: u32,
        error_code: u32,
    ) -> Result<(), Error> {
        self.ensure_initialized(cx)?;
        codec::write_frame(
            cx,
            &mut self.transport,
            &Frame::goaway(last_stream_id, error_code, Bytes::new()),
        )
    }

    /// Closes the underlying transport.
    pub fn close(self, cx: &RuntimeContext<'_>) -> Result<(), Error> {
        self.transport.close(cx)
    }

    /// Half-closes writes, drains peer data until EOF, then closes the transport.
    ///
    /// This is useful for tests and short-lived protocols where the server wants
    /// the client to observe a graceful end of writes before the socket is closed.
    pub fn shutdown_write_and_close_after_peer(
        mut self,
        cx: &RuntimeContext<'_>,
    ) -> Result<(), Error> {
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

    fn ensure_initialized(&mut self, cx: &RuntimeContext<'_>) -> Result<(), Error> {
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
        cx: &RuntimeContext<'_>,
        stream_id: StreamId,
        mut body: &[u8],
        end_stream_on_last: bool,
    ) -> Result<(), Error> {
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

    fn process_frame(&mut self, cx: &RuntimeContext<'_>, frame: Frame) -> Result<(), Error> {
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

    fn process_headers(&mut self, cx: &RuntimeContext<'_>, frame: Frame) -> Result<(), Error> {
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
            if end_stream {
                self.complete_request(stream_id)?;
            }
        } else {
            if !end_stream {
                return Err(Error::Protocol("request trailers missing END_STREAM"));
            }
            self.state
                .stream_mut(stream_id)
                .ok_or(Error::Protocol("unknown stream"))?
                .receive_trailers(headers)?;
            self.complete_request(stream_id)?;
        }
        Ok(())
    }

    fn process_data(&mut self, cx: &RuntimeContext<'_>, frame: Frame) -> Result<(), Error> {
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
        self.state.queue_connection_window_update(data_len)?;
        pending.body.append(&data)?;
        let should_update =
            !end_stream && self.state.queue_stream_window_update_to_target(stream_id)?;
        if data_len != 0 || should_update {
            codec::flush_pending(cx, &mut self.transport, &mut self.state)?;
        }
        if end_stream {
            self.complete_request(stream_id)?;
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
        let request = codec::request_from_headers(headers, pending.body.finish())?;
        self.completed
            .push_back(IncomingRequest { stream_id, request });
        Ok(())
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
