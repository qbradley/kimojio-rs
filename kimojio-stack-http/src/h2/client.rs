// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::{collections::BTreeMap, time::Instant};

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

struct PendingResponse {
    headers: Option<Vec<Header>>,
    body: BodyBuilder,
    trailers: Trailers,
}

impl PendingResponse {
    fn new(limits: BodyLimits) -> Self {
        Self {
            headers: None,
            body: BodyBuilder::new(limits),
            trailers: Trailers::new(),
        }
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
        self.ensure_initialized(cx)?;
        if let Some(response) = self.completed.remove(&stream_id) {
            let stream_body = on_headers(&response.response)?;
            if stream_body && !response.response.body().is_empty() {
                on_chunk(Bytes::copy_from_slice(response.response.body().as_bytes()))?;
            }
            return Ok(response);
        }
        if let Some(error) = self.reset_streams.remove(&stream_id) {
            return Err(error);
        }
        if let Some(error) = self.goaway_for_stream(stream_id) {
            return Err(error);
        }
        let mut stream_body = None;
        loop {
            let frame = codec::read_frame(
                cx,
                &mut self.transport,
                self.state.local_settings().max_frame_size,
            )?;
            if frame.frame_type == FrameType::Data && frame.stream_id == stream_id.get() {
                let should_stream = match stream_body {
                    Some(should_stream) => should_stream,
                    None => {
                        let response = self
                            .pending_response_head(stream_id)?
                            .ok_or(Error::Protocol("DATA before response headers"))?;
                        let should_stream = on_headers(&response)?;
                        stream_body = Some(should_stream);
                        should_stream
                    }
                };
                if should_stream {
                    self.process_data_chunk(cx, frame, &mut on_chunk)?;
                } else {
                    self.process_data_discard(cx, frame)?;
                }
            } else {
                self.process_frame(cx, frame)?;
                if stream_body.is_none()
                    && let Some(response) = self.pending_response_head(stream_id)?
                {
                    stream_body = Some(on_headers(&response)?);
                }
            }
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
            self.state
                .stream_mut(stream_id)
                .ok_or(Error::Protocol("unknown stream"))?
                .receive_headers(true, end_stream)?;
            pending.headers = Some(headers);
            if end_stream {
                self.complete_response(stream_id)?;
            }
        } else {
            if !end_stream {
                return Err(Error::Protocol("response trailers missing END_STREAM"));
            }
            self.state
                .stream_mut(stream_id)
                .ok_or(Error::Protocol("unknown stream"))?
                .receive_trailers(headers.clone())?;
            pending.trailers = codec::trailers_from_headers(headers)?;
            self.complete_response(stream_id)?;
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
        self.state.queue_connection_window_update(data_len)?;
        pending.body.append(&data)?;
        let should_update =
            !end_stream && self.state.queue_stream_window_update_to_target(stream_id)?;
        if data_len != 0 || should_update {
            codec::flush_pending(cx, &mut self.transport, &mut self.state)?;
        }
        if end_stream {
            self.complete_response(stream_id)?;
        }
        Ok(())
    }

    fn process_data_chunk<F>(
        &mut self,
        cx: &RuntimeContext<'_>,
        frame: Frame,
        on_chunk: &mut F,
    ) -> Result<(), Error>
    where
        F: FnMut(Bytes) -> Result<(), Error>,
    {
        self.state.track_inbound_frame(&frame)?;
        let stream_id = StreamId::new(frame.stream_id)?;
        let end_stream = frame.flags.contains(FrameFlags::END_STREAM);
        let FramePayload::Data(data) = frame.payload else {
            return Err(Error::Protocol("expected DATA payload"));
        };
        if !self.pending.contains_key(&stream_id) {
            return Err(Error::Protocol("DATA before response headers"));
        }
        let limits = BodyLimits::new(self.config.max_body_bytes);
        self.state
            .stream_mut(stream_id)
            .ok_or(Error::Protocol("unknown stream"))?
            .receive_data(&data, end_stream, limits)?;
        let data_len = data.len();
        self.state.consume_inbound_connection_window(data_len)?;
        self.state.queue_connection_window_update(data_len)?;
        on_chunk(data)?;
        let should_update =
            !end_stream && self.state.queue_stream_window_update_to_target(stream_id)?;
        if data_len != 0 || should_update {
            codec::flush_pending(cx, &mut self.transport, &mut self.state)?;
        }
        if end_stream {
            self.complete_response(stream_id)?;
        }
        Ok(())
    }

    fn process_data_discard(&mut self, cx: &RuntimeContext<'_>, frame: Frame) -> Result<(), Error> {
        self.state.track_inbound_frame(&frame)?;
        let stream_id = StreamId::new(frame.stream_id)?;
        let end_stream = frame.flags.contains(FrameFlags::END_STREAM);
        let FramePayload::Data(data) = frame.payload else {
            return Err(Error::Protocol("expected DATA payload"));
        };
        if !self.pending.contains_key(&stream_id) {
            return Err(Error::Protocol("DATA before response headers"));
        }
        let limits = BodyLimits::new(self.config.max_body_bytes);
        self.state
            .stream_mut(stream_id)
            .ok_or(Error::Protocol("unknown stream"))?
            .receive_data(&data, end_stream, limits)?;
        let data_len = data.len();
        self.state.consume_inbound_connection_window(data_len)?;
        self.state.queue_connection_window_update(data_len)?;
        let should_update =
            !end_stream && self.state.queue_stream_window_update_to_target(stream_id)?;
        if data_len != 0 || should_update {
            codec::flush_pending(cx, &mut self.transport, &mut self.state)?;
        }
        if end_stream {
            self.complete_response(stream_id)?;
        }
        Ok(())
    }

    fn pending_response_head(&self, stream_id: StreamId) -> Result<Option<Response<Body>>, Error> {
        let Some(pending) = self.pending.get(&stream_id) else {
            return Ok(None);
        };
        let Some(headers) = &pending.headers else {
            return Ok(None);
        };
        codec::response_from_headers(headers.clone(), Body::empty()).map(Some)
    }

    fn complete_response(&mut self, stream_id: StreamId) -> Result<(), Error> {
        let pending = self
            .pending
            .remove(&stream_id)
            .ok_or(Error::Protocol("response state missing"))?;
        let headers = pending
            .headers
            .ok_or(Error::Protocol("response headers missing"))?;
        let response = codec::response_from_headers(headers, pending.body.finish())?;
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
