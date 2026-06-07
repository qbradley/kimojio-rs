// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::{BTreeMap, VecDeque};

use bytes::Bytes;
use http::{Request, Response};
use kimojio_stack::RuntimeContext;

use crate::{Body, BodyBuilder, BodyLimits, Error, HttpConfig, StackTransport, Trailers};

use super::{
    ConnectionState, Frame, FrameFlags, FramePayload, FrameType, Header, Settings, StreamId, codec,
};

#[derive(Debug)]
pub struct IncomingRequest {
    pub stream_id: StreamId,
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

pub struct ServerConnection {
    transport: StackTransport,
    config: HttpConfig,
    state: ConnectionState,
    initialized: bool,
    pending: BTreeMap<StreamId, PendingRequest>,
    completed: VecDeque<IncomingRequest>,
}

impl ServerConnection {
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
            pending: BTreeMap::new(),
            completed: VecDeque::new(),
        }
    }

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
        }
    }

    pub fn send_response(
        &mut self,
        cx: &RuntimeContext<'_>,
        stream_id: StreamId,
        response: &Response<Body>,
    ) -> Result<(), Error> {
        self.send_response_with_trailers(cx, stream_id, response, None)
    }

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
        codec::write_frame(
            cx,
            &mut self.transport,
            &Frame::headers(stream_id, Bytes::from(block), end_stream),
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
            codec::write_frame(
                cx,
                &mut self.transport,
                &Frame::headers(stream_id, Bytes::from(block), true),
            )?;
        }
        Ok(())
    }

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

    pub fn close(self, cx: &RuntimeContext<'_>) -> Result<(), Error> {
        self.transport.close(cx)
    }

    /// Half-closes writes, drains peer data until EOF, then closes the transport.
    pub fn shutdown_write_and_close_after_peer(
        mut self,
        cx: &RuntimeContext<'_>,
    ) -> Result<(), Error> {
        self.transport.shutdown_write(cx)?;
        let mut buffer = [0_u8; 1024];
        loop {
            match self.transport.read(cx, &mut buffer)? {
                0 => return self.transport.close(cx),
                _ => continue,
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
            FrameType::Goaway | FrameType::RstStream => Err(Error::PeerReset),
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
        self.state.open_stream(stream_id)?;
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
        pending.body.append(&data)?;
        let should_update = !end_stream
            && self
                .state
                .stream(stream_id)
                .ok_or(Error::Protocol("unknown stream"))?
                .inbound_window()
                .available()
                == 0;
        if should_update {
            self.state.queue_window_update(stream_id, data.len())?;
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
}
