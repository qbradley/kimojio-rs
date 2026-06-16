// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::marker::PhantomData;

use bytes::{Bytes, BytesMut};
use http::{
    HeaderMap, Request, Response, StatusCode,
    header::{CONTENT_TYPE, TE},
};
use kimojio_stack::{IoFd, RuntimeSocket};
use kimojio_stack_http::{Body, BodyLimits, HttpRuntime, Trailers, h2};
use prost::Message;

use crate::{Error, Metadata, Status, StatusCode as GrpcStatusCode, codec, status::GRPC_STATUS};

/// Shared configuration for stackful gRPC clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientConfig {
    /// Maximum encoded gRPC message length, excluding the five-byte gRPC frame header.
    pub max_message_len: usize,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            max_message_len: 4 * 1024 * 1024,
        }
    }
}

/// Successful unary gRPC response.
///
/// A unary call returns this only after the HTTP response is valid, the gRPC
/// status is OK, and the response message has decoded successfully.
#[derive(Debug)]
pub struct UnaryResponse<M> {
    /// Initial response metadata from HTTP/2 headers.
    pub metadata: Metadata,
    /// Decoded response message.
    pub message: M,
    /// Terminal status. Unary calls return `Ok` only when this is [`StatusCode::Ok`](crate::StatusCode::Ok).
    pub status: Status,
    /// Terminal trailing metadata.
    pub trailers: Metadata,
}

/// Handle for an in-progress server-streaming gRPC response.
///
/// The handle holds a mutable borrow on the [`UnaryClient`] that produced it,
/// so only one streaming response may be active on a given client at a time.
/// To run concurrent server-streaming RPCs, use separate [`UnaryClient`]
/// instances, each backed by its own HTTP/2 connection.
///
/// Metadata from the initial response headers is available in [`metadata`]
/// immediately after the handle is created.  Trailing metadata is populated in
/// [`trailers`] once the stream has reached its terminal state.
///
/// # Cancellation
///
/// Call [`cancel`] to stop the stream before the server has sent its terminal
/// status.  If the handle is dropped without calling [`cancel`] or exhausting
/// the stream with [`next`], the underlying HTTP/2 stream state is not
/// explicitly reset; the server will observe a connection-level error or
/// GOAWAY when the connection is later closed.  Callers that care about
/// prompt server-side cancellation should prefer an explicit [`cancel`] call.
///
/// [`metadata`]: Self::metadata
/// [`trailers`]: Self::trailers
/// [`cancel`]: Self::cancel
/// [`next`]: Self::next
pub struct RuntimeServerStreamingResponse<'a, S, M> {
    client: &'a mut RuntimeUnaryClient<S>,
    stream_id: h2::StreamId,
    response_headers: HeaderMap,
    pub metadata: Metadata,
    pub trailers: Metadata,
    data: Bytes,
    data_offset: usize,
    frame: BytesMut,
    finished: bool,
    _marker: PhantomData<fn() -> M>,
}

/// Low-level gRPC client over one runtime-backed HTTP/2 connection.
///
/// The client name is historical: it supports unary, client-streaming, and
/// server-streaming calls. It owns the HTTP/2 connection and does not perform
/// background reads, retries, or automatic reconnects.
///
/// # Runtime migration boundary
///
/// Runtime-specific I/O is contained in the owned HTTP/2 connection. A future
/// Generic HTTP/2 connection ownership is the only runtime boundary; gRPC
/// framing, metadata, and status handling remain runtime-neutral.
pub struct RuntimeUnaryClient<S> {
    http: h2::RuntimeClientConnection<S>,
    config: ClientConfig,
}

/// Stack-core gRPC client compatibility alias.
pub type UnaryClient = RuntimeUnaryClient<IoFd>;

/// Stack-core server-streaming response compatibility alias.
pub type ServerStreamingResponse<'a, M> = RuntimeServerStreamingResponse<'a, IoFd, M>;

impl<S> RuntimeUnaryClient<S> {
    /// Creates a client over a caller-created HTTP/2 connection.
    pub fn new(http: h2::RuntimeClientConnection<S>, config: ClientConfig) -> Self {
        Self { http, config }
    }

    /// Performs one unary RPC.
    ///
    /// The request message is Prost-encoded, framed, and sent as one HTTP/2
    /// request body. The response body must contain exactly one gRPC message and
    /// terminal status must be OK; non-OK statuses return [`Error::Status`].
    pub fn call<Req, Resp>(
        &mut self,
        cx: &impl HttpRuntime<S>,
        path: &str,
        metadata: Metadata,
        request: &Req,
    ) -> Result<UnaryResponse<Resp>, Error>
    where
        S: RuntimeSocket,
        Req: Message,
        Resp: Message + Default,
    {
        let body = codec::encode_message(request, self.config.max_message_len)?;
        let http_request = build_request(path, metadata, body)?;
        let stream_id = self.http.send_request(cx, &http_request)?;
        let response = self.http.read_response_with_trailers(cx, stream_id)?;
        parse_response(response, self.config.max_message_len)
    }

    /// Performs one client-streaming RPC.
    ///
    /// The method sends request metadata, then encodes each item from
    /// `requests` as one gRPC message frame on the request body stream. The
    /// response is parsed as a unary response after the request stream is
    /// half-closed.
    pub fn call_client_streaming<Req, Resp, I>(
        &mut self,
        cx: &impl HttpRuntime<S>,
        path: &str,
        metadata: Metadata,
        requests: I,
    ) -> Result<UnaryResponse<Resp>, Error>
    where
        S: RuntimeSocket,
        Req: Message,
        Resp: Message + Default,
        I: IntoIterator<Item = Req>,
    {
        let http_request = build_request_head(path, metadata)?;
        let stream_id = self.http.send_request_headers(cx, &http_request)?;
        for request in requests {
            let body = match codec::encode_message(&request, self.config.max_message_len) {
                Ok(body) => body,
                Err(error) => {
                    let _ = self
                        .http
                        .cancel_response_stream(cx, stream_id, h2::ERROR_CODE_CANCEL);
                    return Err(error);
                }
            };
            if let Err(error) = self.http.send_request_data(cx, stream_id, body) {
                let _ = self
                    .http
                    .cancel_response_stream(cx, stream_id, h2::ERROR_CODE_CANCEL);
                return Err(error.into());
            }
        }
        if let Err(error) = self.http.finish_request_stream(cx, stream_id) {
            let _ = self
                .http
                .cancel_response_stream(cx, stream_id, h2::ERROR_CODE_CANCEL);
            return Err(error.into());
        }
        let response = self.http.read_response_with_trailers(cx, stream_id)?;
        parse_response(response, self.config.max_message_len)
    }

    /// Initiates a server-streaming RPC and returns a handle for consuming the
    /// response stream.
    ///
    /// The method sends the encoded `request`, validates the initial HTTP/2
    /// response headers (HTTP 200 + `application/grpc` content-type), and
    /// returns a [`ServerStreamingResponse`] whose [`metadata`] field already
    /// reflects the initial response metadata.  The `UnaryClient` is mutably
    /// borrowed for the lifetime of the returned handle; to run concurrent
    /// streaming RPCs, use separate client instances backed by independent
    /// HTTP/2 connections.
    ///
    /// [`metadata`]: ServerStreamingResponse::metadata
    pub fn call_server_streaming<Req, Resp>(
        &mut self,
        cx: &impl HttpRuntime<S>,
        path: &str,
        metadata: Metadata,
        request: &Req,
    ) -> Result<RuntimeServerStreamingResponse<'_, S, Resp>, Error>
    where
        S: RuntimeSocket,
        Req: Message,
        Resp: Message + Default,
    {
        let body = codec::encode_message(request, self.config.max_message_len)?;
        let http_request = build_request(path, metadata, body)?;
        let stream_id = self.http.send_request(cx, &http_request)?;
        let response = self
            .http
            .read_response_headers(cx, stream_id)
            .map_err(Error::from)?;
        if response.status() != StatusCode::OK {
            let error = Error::Protocol("gRPC response HTTP status was not 200");
            let _ = self
                .http
                .cancel_response_stream(cx, stream_id, h2::ERROR_CODE_CANCEL);
            return Err(error);
        }
        if let Err(error) = validate_content_type(&response) {
            let _ = self
                .http
                .cancel_response_stream(cx, stream_id, h2::ERROR_CODE_CANCEL);
            return Err(error);
        }
        let metadata = match Metadata::from_http_headers(response.headers().clone()) {
            Ok(metadata) => metadata,
            Err(error) => {
                let _ = self
                    .http
                    .cancel_response_stream(cx, stream_id, h2::ERROR_CODE_CANCEL);
                return Err(error);
            }
        };
        Ok(RuntimeServerStreamingResponse {
            client: self,
            stream_id,
            response_headers: response.headers().clone(),
            metadata,
            trailers: Metadata::new(),
            data: Bytes::new(),
            data_offset: 0,
            frame: BytesMut::new(),
            finished: false,
            _marker: PhantomData,
        })
    }

    /// Closes the underlying HTTP/2 connection.
    pub fn close(self, cx: &impl HttpRuntime<S>) -> Result<(), Error>
    where
        S: RuntimeSocket,
    {
        self.http.close(cx).map_err(Error::from)
    }
}

impl<S, M> RuntimeServerStreamingResponse<'_, S, M>
where
    S: RuntimeSocket,
    M: Message + Default,
{
    /// Returns the next decoded message from the stream.
    ///
    /// - `Ok(Some(message))` – a message was decoded successfully.
    /// - `Ok(None)` – the stream ended with a terminal `grpc-status: OK`.
    ///   [`trailers`] is populated before this return.
    /// - `Err(Error::Status(status))` – the stream ended with a non-OK status.
    ///   [`trailers`] is populated before this return.
    ///
    /// Once either `Ok(None)` or an `Err` has been returned, subsequent calls
    /// immediately return `Ok(None)` without blocking.
    ///
    /// [`trailers`]: Self::trailers
    pub fn next(&mut self, cx: &impl HttpRuntime<S>) -> Result<Option<M>, Error> {
        if self.finished {
            return Ok(None);
        }

        loop {
            match self.decode_buffered_message() {
                Ok(Some(message)) => return Ok(Some(message)),
                Ok(None) => {}
                Err(error) => {
                    self.cancel_after_local_error(cx);
                    return Err(error);
                }
            }

            match self
                .client
                .http
                .read_response_event(cx, self.stream_id)
                .map_err(Error::from)?
            {
                h2::ResponseStreamEvent::Data(data) => {
                    self.data = data;
                    self.data_offset = 0;
                }
                h2::ResponseStreamEvent::Trailers(trailers) => {
                    self.finished = true;
                    if !self.frame.is_empty() {
                        return Err(Error::Protocol(
                            "incomplete gRPC frame before stream trailers",
                        ));
                    }
                    let status = response_status(&self.response_headers, &trailers)?;
                    self.trailers = Metadata::from_http_trailers(trailers)?;
                    if status.code() == GrpcStatusCode::Ok {
                        return Ok(None);
                    }
                    return Err(Error::Status(status));
                }
            }
        }
    }

    /// Cancels the stream by sending `RST_STREAM` to the server.
    ///
    /// The handle is consumed; no further calls to [`next`] are possible.
    /// If the stream has already reached a terminal state this is a no-op.
    ///
    /// [`next`]: Self::next
    pub fn cancel(mut self, cx: &impl HttpRuntime<S>) -> Result<(), Error> {
        if !self.finished {
            self.client
                .http
                .cancel_response_stream(cx, self.stream_id, h2::ERROR_CODE_CANCEL)
                .map_err(Error::from)?;
            self.finished = true;
        }
        Ok(())
    }

    fn cancel_after_local_error(&mut self, cx: &impl HttpRuntime<S>) {
        let _ = self
            .client
            .http
            .cancel_response_stream(cx, self.stream_id, h2::ERROR_CODE_CANCEL);
        self.finished = true;
    }

    fn decode_buffered_message(&mut self) -> Result<Option<M>, Error> {
        while self.data_offset < self.data.len() {
            if self.frame.is_empty() && self.data.len() - self.data_offset >= codec::HEADER_LEN {
                let header_end = self.data_offset + codec::HEADER_LEN;
                let total_len = codec::frame_len(
                    &self.data[self.data_offset..header_end],
                    self.client.config.max_message_len,
                )?;
                let frame_end = self.data_offset + total_len;
                if frame_end <= self.data.len() {
                    let message = codec::decode_message::<M>(
                        &self.data[self.data_offset..frame_end],
                        self.client.config.max_message_len,
                    )?;
                    self.data_offset = frame_end;
                    return Ok(Some(message));
                }
            }

            if self.frame.len() < codec::HEADER_LEN {
                let header_remaining = codec::HEADER_LEN - self.frame.len();
                let available = self.data.len() - self.data_offset;
                let copy_len = header_remaining.min(available);
                self.frame
                    .extend_from_slice(&self.data[self.data_offset..self.data_offset + copy_len]);
                self.data_offset += copy_len;
                if self.frame.len() < codec::HEADER_LEN {
                    continue;
                }
            }

            let total_len = codec::frame_len(&self.frame, self.client.config.max_message_len)?;
            let body_remaining = total_len - self.frame.len();
            let available = self.data.len() - self.data_offset;
            let copy_len = body_remaining.min(available);
            self.frame
                .extend_from_slice(&self.data[self.data_offset..self.data_offset + copy_len]);
            self.data_offset += copy_len;

            if self.frame.len() == total_len {
                let message =
                    codec::decode_message::<M>(&self.frame, self.client.config.max_message_len)?;
                self.frame.clear();
                return Ok(Some(message));
            }
        }

        self.data = Bytes::new();
        self.data_offset = 0;
        Ok(None)
    }
}

fn build_request(
    path: &str,
    metadata: Metadata,
    body: bytes::Bytes,
) -> Result<Request<Body>, Error> {
    let body = Body::from_bytes(body, BodyLimits::new(usize::MAX)).map_err(Error::from)?;
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header(CONTENT_TYPE, "application/grpc")
        .header(TE, "trailers");
    for (name, value) in metadata.into_http_headers() {
        let Some(name) = name else {
            return Err(Error::Protocol("metadata continuation header unsupported"));
        };
        builder = builder.header(name, value);
    }
    builder
        .body(body)
        .map_err(|_| Error::Protocol("failed to build gRPC request"))
}

fn build_request_head(path: &str, metadata: Metadata) -> Result<Request<()>, Error> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header(CONTENT_TYPE, "application/grpc")
        .header(TE, "trailers");
    for (name, value) in metadata.into_http_headers() {
        let Some(name) = name else {
            return Err(Error::Protocol("metadata continuation header unsupported"));
        };
        builder = builder.header(name, value);
    }
    builder
        .body(())
        .map_err(|_| Error::Protocol("failed to build gRPC request"))
}

fn parse_response<M>(
    response: h2::ResponseWithTrailers,
    max_message_len: usize,
) -> Result<UnaryResponse<M>, Error>
where
    M: Message + Default,
{
    if response.response.status() != StatusCode::OK {
        return Err(Error::Protocol("gRPC response HTTP status was not 200"));
    }
    validate_content_type(&response.response)?;
    let status = response_status(response.response.headers(), &response.trailers)?;
    if status.code() != GrpcStatusCode::Ok {
        return Err(Error::Status(status));
    }
    let metadata = Metadata::from_http_headers(response.response.headers().clone())?;
    let trailers = Metadata::from_http_trailers(response.trailers)?;
    let message = codec::decode_message::<M>(response.response.body().as_bytes(), max_message_len)?;
    Ok(UnaryResponse {
        metadata,
        message,
        status,
        trailers,
    })
}

fn response_status(headers: &HeaderMap, trailers: &Trailers) -> Result<Status, Error> {
    if trailers.get(&GRPC_STATUS).is_some() {
        return Status::from_trailers(trailers);
    }

    let mut trailers = Trailers::new();
    for (name, value) in headers {
        trailers.insert(name.clone(), value.clone());
    }
    Status::from_trailers(&trailers)
}

fn validate_content_type<B>(response: &Response<B>) -> Result<(), Error> {
    let value = response
        .headers()
        .get(CONTENT_TYPE)
        .ok_or(Error::Protocol("missing gRPC content-type"))?;
    if is_grpc_content_type(value.as_bytes()) {
        Ok(())
    } else {
        Err(Error::Protocol("invalid gRPC content-type"))
    }
}

fn is_grpc_content_type(value: &[u8]) -> bool {
    value == b"application/grpc" || value.starts_with(b"application/grpc+")
}
