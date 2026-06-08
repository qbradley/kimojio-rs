// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::{
    collections::{BTreeMap, VecDeque},
    time::Instant,
};

use bytes::Bytes;
use http::{Request, Response};
use kimojio_stack::RuntimeContext;

use crate::{Body, BodyBuilder, BodyLimits, Error, HttpConfig, StackTransport, Trailers};

use super::{
    ConnectionState, Frame, FrameFlags, FramePayload, FrameType, Header, Settings, StreamId, codec,
};

#[derive(Debug)]
pub struct ResponseWithTrailers {
    pub response: Response<Body>,
    pub trailers: Trailers,
}

/// Incremental response event returned by [`ClientConnection::read_response_event`].
#[derive(Debug)]
pub enum ResponseStreamEvent {
    /// One DATA payload for the response stream.
    Data(Bytes),
    /// Terminal trailers for the response stream.
    ///
    /// Empty trailers represent a DATA frame with `END_STREAM` and no trailer
    /// HEADERS.
    Trailers(Trailers),
}

struct QueuedResponseStreamEvent {
    event: ResponseStreamEvent,
    data_len: usize,
    update_stream_window: bool,
}

struct PendingResponse {
    headers: Option<Vec<Header>>,
    body: Option<BodyBuilder>,
    trailers: Trailers,
    events: VecDeque<QueuedResponseStreamEvent>,
    head_delivered: bool,
}

impl PendingResponse {
    fn new(limits: BodyLimits) -> Self {
        Self {
            headers: None,
            body: Some(BodyBuilder::new(limits)),
            trailers: Trailers::new(),
            events: VecDeque::new(),
            head_delivered: false,
        }
    }

    fn new_streaming() -> Self {
        Self {
            headers: None,
            body: None,
            trailers: Trailers::new(),
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
            self.events.push_back(QueuedResponseStreamEvent {
                event: ResponseStreamEvent::Data(data),
                data_len,
                update_stream_window,
            });
        }
    }

    fn push_trailers_event(&mut self, trailers: Trailers) {
        self.events.push_back(QueuedResponseStreamEvent {
            event: ResponseStreamEvent::Trailers(trailers),
            data_len: 0,
            update_stream_window: false,
        });
    }
}

pub struct ClientConnection {
    transport: StackTransport,
    config: HttpConfig,
    state: ConnectionState,
    initialized: bool,
    next_stream_id: u32,
    pending: BTreeMap<StreamId, PendingResponse>,
    completed: BTreeMap<StreamId, ResponseWithTrailers>,
    reset_streams: BTreeMap<StreamId, Error>,
    goaway: Option<Error>,
}

impl ClientConnection {
    pub fn new(transport: StackTransport, config: HttpConfig) -> Self {
        let settings = codec::settings_from_config(config);
        Self::new_with_settings(transport, config, settings)
    }

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
            next_stream_id: 1,
            pending: BTreeMap::new(),
            completed: BTreeMap::new(),
            reset_streams: BTreeMap::new(),
            goaway: None,
        }
    }

    pub fn set_io_deadline(&mut self, deadline: Option<Instant>) -> Option<Instant> {
        self.transport.set_io_deadline(deadline)
    }

    pub fn send(
        &mut self,
        cx: &RuntimeContext<'_>,
        request: &Request<Body>,
    ) -> Result<Response<Body>, Error> {
        let stream_id = self.send_request(cx, request)?;
        self.read_response_with_trailers(cx, stream_id)
            .map(|response| response.response)
    }

    pub fn send_request(
        &mut self,
        cx: &RuntimeContext<'_>,
        request: &Request<Body>,
    ) -> Result<StreamId, Error> {
        self.ensure_initialized(cx)?;
        let stream_id = StreamId::new(self.next_stream_id)?;
        self.next_stream_id = self
            .next_stream_id
            .checked_add(2)
            .ok_or(Error::Protocol("HTTP/2 stream id overflow"))?;
        self.state.open_outbound_stream(stream_id)?;

        let headers = codec::request_headers(request)?;
        let block = self.state.encode_header_block(&headers);
        let body = request.body().clone().into_bytes();
        let end_stream = body.is_empty();
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
        if !end_stream {
            self.write_data(cx, stream_id, body)?;
        }
        Ok(stream_id)
    }

    pub fn read_response(
        &mut self,
        cx: &RuntimeContext<'_>,
        stream_id: StreamId,
    ) -> Result<Response<Body>, Error> {
        self.read_response_with_trailers(cx, stream_id)
            .map(|response| response.response)
    }

    pub fn read_response_with_trailers(
        &mut self,
        cx: &RuntimeContext<'_>,
        stream_id: StreamId,
    ) -> Result<ResponseWithTrailers, Error> {
        self.ensure_initialized(cx)?;
        if let Some(response) = self.completed.remove(&stream_id) {
            return Ok(response);
        }
        if let Some(error) = self.reset_streams.remove(&stream_id) {
            return Err(error);
        }
        if let Some(error) = self.goaway_for_stream(stream_id) {
            return Err(error);
        }
        loop {
            let frame = codec::read_frame(
                cx,
                &mut self.transport,
                self.state.local_settings().max_frame_size,
            )?;
            self.process_frame(cx, frame)?;
            if let Some(response) = self.completed.remove(&stream_id) {
                return Ok(response);
            }
            if let Some(error) = self.reset_streams.remove(&stream_id) {
                return Err(error);
            }
            if let Some(error) = self.goaway_for_stream(stream_id) {
                return Err(error);
            }
        }
    }

    pub fn read_response_with_body_chunks<F>(
        &mut self,
        cx: &RuntimeContext<'_>,
        stream_id: StreamId,
        on_chunk: F,
    ) -> Result<ResponseWithTrailers, Error>
    where
        F: FnMut(Bytes) -> Result<(), Error>,
    {
        self.read_response_with_body_chunks_after_headers(cx, stream_id, |_| Ok(true), on_chunk)
    }

    pub fn read_response_with_body_chunks_after_headers<H, F>(
        &mut self,
        cx: &RuntimeContext<'_>,
        stream_id: StreamId,
        mut on_headers: H,
        mut on_chunk: F,
    ) -> Result<ResponseWithTrailers, Error>
    where
        H: FnMut(&Response<Body>) -> Result<bool, Error>,
        F: FnMut(Bytes) -> Result<(), Error>,
    {
        let head = self.read_response_headers(cx, stream_id)?;
        let (parts, ()) = head.into_parts();
        let response = Response::from_parts(parts, Body::empty());
        let stream_body = on_headers(&response)?;
        let trailers;
        loop {
            match self.read_response_event(cx, stream_id)? {
                ResponseStreamEvent::Data(data) => {
                    if stream_body {
                        on_chunk(data)?;
                    }
                }
                ResponseStreamEvent::Trailers(done) => {
                    trailers = done;
                    break;
                }
            }
        }
        Ok(ResponseWithTrailers { response, trailers })
    }

    /// Reads and returns response headers for an incremental response stream.
    ///
    /// Call this after [`send_request`](Self::send_request) and before
    /// [`read_response_event`](Self::read_response_event). While waiting for the
    /// target stream, the connection may process unrelated frames for other
    /// streams so connection state remains current.
    pub fn read_response_headers(
        &mut self,
        cx: &RuntimeContext<'_>,
        stream_id: StreamId,
    ) -> Result<Response<()>, Error> {
        self.ensure_initialized(cx)?;
        self.ensure_streaming_pending(stream_id)?;
        loop {
            if let Some(error) = self.reset_streams.remove(&stream_id) {
                self.pending.remove(&stream_id);
                return Err(error);
            }
            if let Some(error) = self.goaway_for_stream(stream_id) {
                self.pending.remove(&stream_id);
                return Err(error);
            }
            if let Some(pending) = self.pending.get_mut(&stream_id)
                && let Some(headers) = pending.headers.clone().filter(|_| !pending.head_delivered)
            {
                pending.head_delivered = true;
                return codec::response_from_headers(headers, ());
            }

            let frame = codec::read_frame(
                cx,
                &mut self.transport,
                self.state.local_settings().max_frame_size,
            )?;
            self.process_frame(cx, frame)?;
        }
    }

    /// Reads the next DATA or terminal trailers event for a response stream.
    ///
    /// [`read_response_headers`](Self::read_response_headers) must be called for
    /// the same `stream_id` first. For DATA events, inbound HTTP/2 flow-control
    /// credit is returned immediately before the event is delivered to the
    /// caller, so unread queued DATA remains bounded by the advertised windows.
    /// A trailers event is terminal; after it is returned, local pending state
    /// for the stream is removed.
    pub fn read_response_event(
        &mut self,
        cx: &RuntimeContext<'_>,
        stream_id: StreamId,
    ) -> Result<ResponseStreamEvent, Error> {
        self.ensure_initialized(cx)?;
        self.ensure_streaming_pending(stream_id)?;
        if self
            .pending
            .get(&stream_id)
            .is_some_and(|pending| !pending.head_delivered)
        {
            return Err(Error::Protocol("response headers not read"));
        }

        loop {
            if let Some(event) = self.pop_streaming_event(cx, stream_id)? {
                return Ok(event);
            }
            if let Some(error) = self.reset_streams.remove(&stream_id) {
                self.pending.remove(&stream_id);
                return Err(error);
            }
            if let Some(error) = self.goaway_for_stream(stream_id) {
                self.pending.remove(&stream_id);
                return Err(error);
            }

            let frame = codec::read_frame(
                cx,
                &mut self.transport,
                self.state.local_settings().max_frame_size,
            )?;
            self.process_frame(cx, frame)?;
        }
    }

    /// Cancels an open response stream with the supplied HTTP/2 error code.
    ///
    /// If local state for the stream is still active, this sends `RST_STREAM`
    /// and removes any pending buffered or streaming response state. Calling it
    /// after the stream has already completed is a no-op.
    pub fn cancel_response_stream(
        &mut self,
        cx: &RuntimeContext<'_>,
        stream_id: StreamId,
        error_code: u32,
    ) -> Result<(), Error> {
        self.ensure_initialized(cx)?;
        self.pending.remove(&stream_id);
        self.completed.remove(&stream_id);
        self.reset_streams.remove(&stream_id);
        if self.state.remove_stream(stream_id).is_some() {
            codec::write_frame(
                cx,
                &mut self.transport,
                &Frame::rst_stream(stream_id, error_code),
            )?;
        }
        Ok(())
    }

    pub fn close(self, cx: &RuntimeContext<'_>) -> Result<(), Error> {
        self.transport.close(cx)
    }

    fn ensure_initialized(&mut self, cx: &RuntimeContext<'_>) -> Result<(), Error> {
        if self.initialized {
            return Ok(());
        }
        codec::write_client_preface(cx, &mut self.transport)?;
        codec::write_frame(
            cx,
            &mut self.transport,
            &codec::settings_frame(self.state.local_settings()),
        )?;

        let mut received_peer_settings = false;
        while !received_peer_settings {
            let frame = codec::read_frame(
                cx,
                &mut self.transport,
                self.state.local_settings().max_frame_size,
            )?;
            if frame.frame_type == FrameType::Settings {
                let is_ack = frame.flags.contains(FrameFlags::ACK);
                self.state.receive_settings(&frame)?;
                codec::flush_pending(cx, &mut self.transport, &mut self.state)?;
                received_peer_settings |= !is_ack;
            } else {
                self.process_frame(cx, frame)?;
            }
        }
        self.initialized = true;
        Ok(())
    }

    fn write_data(
        &mut self,
        cx: &RuntimeContext<'_>,
        stream_id: StreamId,
        mut body: Bytes,
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
                continue;
            }

            let chunk_len = body
                .len()
                .min(capacity)
                .min(self.state.peer_settings().max_frame_size as usize);
            let end_stream = chunk_len == body.len();
            self.state.consume_outbound_window(stream_id, chunk_len)?;
            self.state
                .stream_mut(stream_id)
                .ok_or(Error::Protocol("unknown stream"))?
                .send_data(end_stream)?;
            let chunk = body.split_to(chunk_len);
            codec::write_data_frame(cx, &mut self.transport, stream_id, &chunk, end_stream)?;
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
                self.completed.remove(&stream_id);
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
        let headers = self.state.decode_header_block(&block)?;
        let limits = BodyLimits::new(self.config.max_body_bytes);
        let pending = self
            .pending
            .entry(stream_id)
            .or_insert_with(|| PendingResponse::new(limits));

        if pending.headers.is_none() {
            let streaming = pending.is_streaming();
            self.state
                .stream_mut(stream_id)
                .ok_or(Error::Protocol("unknown stream"))?
                .receive_headers(true, end_stream)?;
            pending.headers = Some(headers);
            if end_stream {
                if streaming {
                    self.complete_streaming_response(stream_id, Trailers::new())?;
                } else {
                    self.complete_response(stream_id)?;
                }
            }
        } else {
            if !end_stream {
                return Err(Error::Protocol("response trailers missing END_STREAM"));
            }
            self.state
                .stream_mut(stream_id)
                .ok_or(Error::Protocol("unknown stream"))?
                .receive_trailers(headers.clone())?;
            let trailers = codec::trailers_from_headers(headers)?;
            if pending.is_streaming() {
                self.complete_streaming_response(stream_id, trailers)?;
            } else {
                pending.trailers = trailers;
                self.complete_response(stream_id)?;
            }
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
            .ok_or(Error::Protocol("DATA before response headers"))?;
        let limits = BodyLimits::new(self.config.max_body_bytes);
        self.state
            .stream_mut(stream_id)
            .ok_or(Error::Protocol("unknown stream"))?
            .receive_data(&data, end_stream, limits)?;
        let data_len = data.len();
        self.state.consume_inbound_connection_window(data_len)?;
        if let Some(body) = pending.body.as_mut() {
            self.state.queue_connection_window_update(data_len)?;
            body.append(&data)?;
            let should_update =
                !end_stream && self.state.queue_stream_window_update_to_target(stream_id)?;
            if data_len != 0 || should_update {
                codec::flush_pending(cx, &mut self.transport, &mut self.state)?;
            }
        } else if !data.is_empty() {
            pending.push_data_event(data, !end_stream);
        }
        if end_stream {
            if self
                .pending
                .get(&stream_id)
                .is_some_and(PendingResponse::is_streaming)
            {
                self.complete_streaming_response(stream_id, Trailers::new())?;
            } else {
                self.complete_response(stream_id)?;
            }
        }
        Ok(())
    }

    fn complete_response(&mut self, stream_id: StreamId) -> Result<(), Error> {
        let pending = self
            .pending
            .remove(&stream_id)
            .ok_or(Error::Protocol("response state missing"))?;
        let headers = pending
            .headers
            .ok_or(Error::Protocol("response headers missing"))?;
        let body = pending
            .body
            .ok_or(Error::Protocol("response body state missing"))?
            .finish();
        let response = codec::response_from_headers(headers, body)?;
        self.completed.insert(
            stream_id,
            ResponseWithTrailers {
                response,
                trailers: pending.trailers,
            },
        );
        self.state.remove_stream(stream_id);
        Ok(())
    }

    fn complete_streaming_response(
        &mut self,
        stream_id: StreamId,
        trailers: Trailers,
    ) -> Result<(), Error> {
        self.pending
            .get_mut(&stream_id)
            .ok_or(Error::Protocol("response state missing"))?
            .push_trailers_event(trailers);
        Ok(())
    }

    fn ensure_streaming_pending(&mut self, stream_id: StreamId) -> Result<(), Error> {
        if let Some(completed) = self.completed.remove(&stream_id) {
            let (parts, body) = completed.response.into_parts();
            let response = Response::from_parts(parts, ());
            let headers = codec::response_headers(&response)?;
            let mut pending = PendingResponse::new_streaming();
            pending.headers = Some(headers);
            pending.head_delivered = false;
            pending.push_data_event(body.into_bytes(), false);
            pending.push_trailers_event(completed.trailers);
            self.pending.insert(stream_id, pending);
            return Ok(());
        }
        match self.pending.get_mut(&stream_id) {
            Some(pending) if pending.is_streaming() => Ok(()),
            Some(pending) => {
                let body = pending
                    .body
                    .take()
                    .ok_or(Error::Protocol("response body state missing"))?;
                pending.push_data_event(body.finish().into_bytes(), true);
                Ok(())
            }
            None => {
                self.pending
                    .insert(stream_id, PendingResponse::new_streaming());
                Ok(())
            }
        }
    }

    fn pop_streaming_event(
        &mut self,
        cx: &RuntimeContext<'_>,
        stream_id: StreamId,
    ) -> Result<Option<ResponseStreamEvent>, Error> {
        let Some(queued) = self
            .pending
            .get_mut(&stream_id)
            .and_then(|pending| pending.events.pop_front())
        else {
            return Ok(None);
        };

        if queued.data_len != 0 {
            self.state.queue_connection_window_update(queued.data_len)?;
            let should_update = queued.update_stream_window
                && self.state.queue_stream_window_update_to_target(stream_id)?;
            if should_update || queued.data_len != 0 {
                codec::flush_pending(cx, &mut self.transport, &mut self.state)?;
            }
        }
        if matches!(queued.event, ResponseStreamEvent::Trailers(_)) {
            self.state.remove_stream(stream_id);
            self.pending.remove(&stream_id);
        }
        Ok(Some(queued.event))
    }

    fn goaway_for_stream(&self, stream_id: StreamId) -> Option<Error> {
        let Some(Error::PeerReset {
            last_stream_id: Some(last_stream_id),
            error_code,
            debug_data,
            ..
        }) = &self.goaway
        else {
            return None;
        };
        if stream_id.get() > *last_stream_id || *error_code != 0 {
            Some(Error::goaway(
                *last_stream_id,
                *error_code,
                debug_data.clone(),
            ))
        } else {
            None
        }
    }
}
