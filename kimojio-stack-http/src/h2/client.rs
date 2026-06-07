// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::BTreeMap;

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
        }
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
        self.state.open_stream(stream_id)?;

        let headers = codec::request_headers(request)?;
        let block = self.state.encode_header_block(&headers);
        let body = request.body().as_bytes();
        let end_stream = body.is_empty();
        self.state
            .stream_mut(stream_id)
            .ok_or(Error::Protocol("unknown stream"))?
            .send_headers(end_stream)?;
        codec::write_frame(
            cx,
            &mut self.transport,
            &Frame::headers(stream_id, Bytes::from(block), end_stream),
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
        mut body: &[u8],
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
            self.complete_response(stream_id)?;
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
        let response = codec::response_from_headers(headers, pending.body.finish())?;
        self.completed.insert(
            stream_id,
            ResponseWithTrailers {
                response,
                trailers: pending.trailers,
            },
        );
        Ok(())
    }
}
