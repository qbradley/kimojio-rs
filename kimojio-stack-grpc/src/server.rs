// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::BTreeMap;

use bytes::{Bytes, BytesMut};
use http::{
    Method, Request, Response, StatusCode,
    header::{CONTENT_TYPE, TE},
};
use kimojio_stack::{RuntimeContext, RuntimeSocket, channel};
use kimojio_stack_http::{Body, BodyLimits, HttpRuntime, h2};
use prost::Message;

use crate::{Error, Metadata, Status, StatusCode as GrpcStatusCode, codec};

/// Runtime context family used by gRPC server handlers.
pub trait GrpcRuntime {
    /// Context type passed to registered handlers while serving one request.
    type Context<'cx>;
}

/// Stack-core gRPC runtime context family.
pub struct StackGrpcRuntime;

impl GrpcRuntime for StackGrpcRuntime {
    type Context<'cx> = RuntimeContext<'cx>;
}

/// Shared configuration for stackful gRPC servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerConfig {
    /// Maximum encoded gRPC message length, excluding the five-byte gRPC frame header.
    pub max_message_len: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            max_message_len: 4 * 1024 * 1024,
        }
    }
}

/// Stackful gRPC server that dispatches incoming requests to registered unary,
/// client-streaming, and server-streaming handlers over an HTTP/2 connection.
///
/// # Runtime migration boundary
///
/// The server dispatch table is independent of socket or ring ownership. Runtime
/// I/O enters only through the `h2::ServerConnection` argument to
/// [`serve_one`](Self::serve_one), so the generic HTTP/2 server connection can be
/// adopted here without introducing a separate gRPC runtime trait.
pub struct RuntimeUnaryServer<R: GrpcRuntime = StackGrpcRuntime> {
    handlers: BTreeMap<String, Box<dyn MethodHandler<R>>>,
    config: ServerConfig,
}

/// Stack-core gRPC server compatibility alias.
pub type UnaryServer = RuntimeUnaryServer<StackGrpcRuntime>;

impl<R> RuntimeUnaryServer<R>
where
    R: GrpcRuntime + 'static,
{
    /// Creates an empty gRPC dispatcher.
    pub fn new(config: ServerConfig) -> Self {
        Self {
            handlers: BTreeMap::new(),
            config,
        }
    }

    /// Registers a unary handler for a fully-qualified gRPC path.
    ///
    /// `path` should look like `/package.Service/Method`. The handler runs
    /// inline on the coroutine serving the request and may use the supplied
    /// runtime context to perform stackful I/O or synchronization.
    pub fn add_unary<Req, Resp, F>(&mut self, path: impl Into<String>, handler: F)
    where
        Req: Message + Default + 'static,
        Resp: Message + 'static,
        F: for<'a> Fn(&'a R::Context<'a>, Metadata, Req) -> Result<UnaryReply<Resp>, Status>
            + 'static,
    {
        let max_message_len = self.config.max_message_len;
        self.handlers.insert(
            path.into(),
            Box::new(TypedUnaryHandler {
                handler,
                max_message_len,
                _marker: std::marker::PhantomData,
            }),
        );
    }

    /// Registers a server-streaming handler for `path`.
    ///
    /// The handler receives the decoded request and returns a [`ServerStreamingReply`] that
    /// carries initial metadata, a [`ServerStream`] source, and optional trailing metadata.
    /// Each item yielded by the stream is encoded and sent as a separate gRPC DATA frame.
    /// On clean completion (`Ok(None)`), `grpc-status: 0` trailers are sent merged with any
    /// caller-supplied trailing metadata.  On a yielded `Err(Status)`, that status is sent
    /// as the terminal trailer without merging caller trailers.
    ///
    /// Request decode or validation errors are mapped to gRPC status-only responses the same
    /// way unary handlers handle them.  Transport reset or disconnect errors observed during
    /// sending are returned as [`Error`] values from [`serve_one`](UnaryServer::serve_one) so
    /// that the serve loop can stop.
    pub fn add_server_streaming<Req, Resp, Stream, F>(
        &mut self,
        path: impl Into<String>,
        handler: F,
    ) where
        Req: Message + Default + 'static,
        Resp: Message + 'static,
        Stream: ServerStream<Resp, R> + 'static,
        F: for<'a> Fn(
                &'a R::Context<'a>,
                Metadata,
                Req,
            ) -> Result<ServerStreamingReply<Stream>, Status>
            + 'static,
    {
        let max_message_len = self.config.max_message_len;
        self.handlers.insert(
            path.into(),
            Box::new(TypedServerStreamingHandler {
                handler,
                max_message_len,
                _marker: std::marker::PhantomData,
            }),
        );
    }

    /// Registers a client-streaming handler for `path`.
    ///
    /// The handler consumes request messages from [`ClientStreamingRequest`] and
    /// returns one unary response. Each inbound gRPC message is decoded as it is
    /// read from the HTTP/2 request body, so callers can enforce bounded upload
    /// memory by consuming one message at a time.
    pub fn add_client_streaming<Req, Resp, F>(&mut self, path: impl Into<String>, handler: F)
    where
        Req: Message + Default + 'static,
        Resp: Message + 'static,
        F: for<'a, 's> Fn(
                &'a R::Context<'a>,
                Metadata,
                ClientStreamingRequest<'s, Req>,
            ) -> Result<UnaryReply<Resp>, Status>
            + 'static,
    {
        let max_message_len = self.config.max_message_len;
        self.handlers.insert(
            path.into(),
            Box::new(TypedClientStreamingHandler {
                handler,
                max_message_len,
                _marker: std::marker::PhantomData,
            }),
        );
    }

    /// Serves one HTTP/2 request through the registered handler table.
    ///
    /// Returns `Ok(false)` when the underlying HTTP/2 connection reaches clean
    /// EOF before another request. Handler `Err(Status)` values are converted
    /// into gRPC status responses; transport/protocol failures are returned as
    /// [`Error`] so the serve loop can stop.
    pub fn serve_one<'cx, S>(
        &self,
        cx: &'cx R::Context<'cx>,
        http: &mut h2::RuntimeServerConnection<S>,
    ) -> Result<bool, Error>
    where
        R::Context<'cx>: HttpRuntime<S>,
        S: RuntimeSocket,
    {
        let Some(incoming) = http.accept_request_headers(cx).map_err(Error::from)? else {
            return Ok(false);
        };
        let stream_id = incoming.stream_id;
        let result = self.dispatch(cx, http, incoming);
        match result {
            Ok(RawResponse::Unary(reply)) => write_reply::<R, S>(cx, http, stream_id, reply),
            Ok(RawResponse::ServerStreaming(reply)) => {
                write_streaming_reply::<R, S>(cx, http, stream_id, reply)
            }
            Err(status) => write_status::<R, S>(cx, http, stream_id, status),
        }?;
        Ok(true)
    }

    fn dispatch<'cx, S>(
        &self,
        cx: &'cx R::Context<'cx>,
        http: &mut h2::RuntimeServerConnection<S>,
        incoming: h2::IncomingRequestHead,
    ) -> Result<RawResponse<R>, Status>
    where
        R::Context<'cx>: HttpRuntime<S>,
        S: RuntimeSocket,
    {
        validate_request(&incoming.request)?;
        let path = incoming.request.uri().path();
        let Some(handler) = self.handlers.get(path) else {
            return Err(Status::new(GrpcStatusCode::Unimplemented, "unknown method"));
        };
        let metadata = Metadata::from_http_headers(incoming.request.headers().clone())
            .map_err(|_| Status::new(GrpcStatusCode::InvalidArgument, "invalid metadata"))?;
        if handler.is_client_streaming() {
            let mut stream =
                HttpRawClientStream::new(cx, http, incoming.stream_id, self.config.max_message_len);
            handler.call(cx, metadata, RawRequest::Streaming(&mut stream))
        } else {
            let frame = collect_request_body::<R, S>(
                cx,
                http,
                incoming.stream_id,
                self.config.max_message_len,
            )?;
            handler.call(cx, metadata, RawRequest::Buffered(&frame))
        }
    }
}

/// Return value from a unary handler.
///
/// Handlers return this to send initial metadata, one response message, and
/// terminal OK trailers.
pub struct UnaryReply<M> {
    /// Initial response metadata sent before the message.
    pub metadata: Metadata,
    /// Response message encoded into the DATA frame.
    pub message: M,
    /// Trailing metadata merged into the terminal OK status.
    pub trailers: Metadata,
}

impl<M> UnaryReply<M> {
    /// Creates a unary reply with empty metadata and trailers.
    pub fn new(message: M) -> Self {
        Self {
            metadata: Metadata::new(),
            message,
            trailers: Metadata::new(),
        }
    }
}

/// Return value for a server-streaming handler.
///
/// Carries initial response metadata (sent as HEADERS before the first DATA frame), the
/// stream source, and optional trailing metadata merged into the terminal `grpc-status: 0`
/// trailers on clean completion.
pub struct ServerStreamingReply<S> {
    /// Initial metadata sent in the HEADERS frame before any DATA frames.
    pub metadata: Metadata,
    /// Source of response messages.  Each call to [`ServerStream::next`] may block until
    /// a message or terminal status is available; the server drives the source inline
    /// without any hidden background reader.
    pub stream: S,
    /// Trailing metadata merged with `grpc-status: 0` on clean stream completion.
    /// Ignored when the stream terminates with a yielded [`Status`] error.
    pub trailers: Metadata,
}

/// Stream of request messages passed to client-streaming handlers.
pub struct ClientStreamingRequest<'a, M> {
    stream: &'a mut dyn RawClientStream,
    max_message_len: usize,
    _marker: std::marker::PhantomData<fn() -> M>,
}

impl<M> ClientStreamingRequest<'_, M>
where
    M: Message + Default,
{
    /// Returns the next decoded request message, or `Ok(None)` when the client
    /// has finished sending the request stream.
    pub fn next<C>(&mut self, _cx: &C) -> Result<Option<M>, Status> {
        let Some(frame) = self.stream.next_frame()? else {
            return Ok(None);
        };
        codec::decode_message::<M>(&frame, self.max_message_len)
            .map(Some)
            .map_err(|error| {
                if matches!(error, Error::SizeLimit { .. }) {
                    Status::new(GrpcStatusCode::ResourceExhausted, "request too large")
                } else {
                    Status::new(GrpcStatusCode::InvalidArgument, "invalid request")
                }
            })
    }
}

impl<S> ServerStreamingReply<S> {
    /// Creates a server-streaming reply with empty metadata and trailers.
    pub fn new(stream: S) -> Self {
        Self {
            metadata: Metadata::new(),
            stream,
            trailers: Metadata::new(),
        }
    }
}

/// Stackful iterator over server-streaming response messages.
///
/// `next` is called in a loop by the server dispatch loop after response headers have been
/// sent.  Each call may block (park) until a message or terminal status is available from
/// the underlying source.
///
/// # Idle producers and cancellation
///
/// The server does **not** run a hidden background reader to detect client cancellation
/// while an idle stream source is parked.  An idle `ServerStream` that is blocked on its
/// own source (for example, waiting on a channel) will not observe cancellation until it
/// resumes and attempts the next `send_response_data` or `finish_response_stream` call.
/// Callers that need prompt cancellation detection must wire their own cancellation
/// mechanism into the source (for example, by sending a terminal item through the channel
/// or by checking a flag before parking).
pub trait ServerStream<M, R: GrpcRuntime = StackGrpcRuntime> {
    /// Returns the next response message, `Ok(None)` when the stream is complete, or
    /// `Err(Status)` to terminate the stream with a non-OK status.
    fn next<'cx>(&mut self, cx: &'cx R::Context<'cx>) -> Result<Option<M>, Status>;
}

impl<M, R, F> ServerStream<M, R> for F
where
    R: GrpcRuntime,
    F: for<'a> FnMut(&'a R::Context<'a>) -> Result<Option<M>, Status>,
{
    fn next<'cx>(&mut self, cx: &'cx R::Context<'cx>) -> Result<Option<M>, Status> {
        self(cx)
    }
}

/// A [`ServerStream`] backed by a bounded channel receiver.
///
/// Each channel item is either `Ok(message)` to yield one response message or
/// `Err(Status)` to terminate the stream with that status.  A closed (disconnected)
/// sender is treated as clean stream completion (`Ok(None)`).
///
/// The channel capacity is caller-owned: the producer will block when the channel is full,
/// providing explicit backpressure without any hidden internal queuing.
pub struct ReceiverStream<M> {
    receiver: channel::bounded::Receiver<Result<M, Status>>,
}

impl<M> ReceiverStream<M> {
    /// Creates a server stream backed by a bounded stack channel receiver.
    pub fn new(receiver: channel::bounded::Receiver<Result<M, Status>>) -> Self {
        Self { receiver }
    }
}

impl<M> ServerStream<M, StackGrpcRuntime> for ReceiverStream<M> {
    fn next(&mut self, cx: &RuntimeContext<'_>) -> Result<Option<M>, Status> {
        match self.receiver.recv(cx) {
            Ok(Ok(message)) => Ok(Some(message)),
            Ok(Err(status)) => Err(status),
            Err(_) => Ok(None),
        }
    }
}

struct RawReply {
    metadata: Metadata,
    message: Bytes,
    trailers: Metadata,
}

struct RawStreamReply<R: GrpcRuntime> {
    metadata: Metadata,
    stream: Box<dyn RawServerStream<R>>,
    trailers: Metadata,
}

enum RawResponse<R: GrpcRuntime> {
    Unary(RawReply),
    ServerStreaming(RawStreamReply<R>),
}

enum RawRequest<'a> {
    Buffered(&'a [u8]),
    Streaming(&'a mut dyn RawClientStream),
}

trait MethodHandler<R: GrpcRuntime> {
    fn is_client_streaming(&self) -> bool {
        false
    }

    fn call<'cx>(
        &self,
        cx: &'cx R::Context<'cx>,
        metadata: Metadata,
        request: RawRequest<'_>,
    ) -> Result<RawResponse<R>, Status>;
}

trait RawClientStream {
    fn next_frame(&mut self) -> Result<Option<Bytes>, Status>;
}

trait RawServerStream<R: GrpcRuntime> {
    fn next_bytes<'cx>(&mut self, cx: &'cx R::Context<'cx>) -> Result<Option<Bytes>, Status>;
}

struct TypedUnaryHandler<Req, Resp, F> {
    handler: F,
    max_message_len: usize,
    _marker: std::marker::PhantomData<fn(Req) -> Resp>,
}

impl<R, Req, Resp, F> MethodHandler<R> for TypedUnaryHandler<Req, Resp, F>
where
    R: GrpcRuntime,
    Req: Message + Default,
    Resp: Message,
    F: for<'a> Fn(&'a R::Context<'a>, Metadata, Req) -> Result<UnaryReply<Resp>, Status>,
{
    fn call<'cx>(
        &self,
        cx: &'cx R::Context<'cx>,
        metadata: Metadata,
        request: RawRequest<'_>,
    ) -> Result<RawResponse<R>, Status> {
        let RawRequest::Buffered(frame) = request else {
            return Err(Status::new(
                GrpcStatusCode::Internal,
                "unexpected streaming request",
            ));
        };
        let request =
            codec::decode_message::<Req>(frame, self.max_message_len).map_err(|error| {
                if matches!(error, Error::SizeLimit { .. }) {
                    Status::new(GrpcStatusCode::ResourceExhausted, "request too large")
                } else {
                    Status::new(GrpcStatusCode::InvalidArgument, "invalid request")
                }
            })?;
        let reply = (self.handler)(cx, metadata, request)?;
        let mut encoded = Vec::new();
        reply
            .message
            .encode(&mut encoded)
            .map_err(|_| Status::new(GrpcStatusCode::Internal, "encode failed"))?;
        let message = codec::encode_bytes(&encoded, self.max_message_len)
            .map_err(|_| Status::new(GrpcStatusCode::ResourceExhausted, "response too large"))?;
        Ok(RawResponse::Unary(RawReply {
            metadata: reply.metadata,
            message,
            trailers: reply.trailers,
        }))
    }
}

struct TypedServerStreamingHandler<Req, Resp, Stream, F> {
    handler: F,
    max_message_len: usize,
    _marker: std::marker::PhantomData<fn(Req) -> (Resp, Stream)>,
}

impl<R, Req, Resp, Stream, F> MethodHandler<R> for TypedServerStreamingHandler<Req, Resp, Stream, F>
where
    R: GrpcRuntime,
    Req: Message + Default,
    Resp: Message + 'static,
    Stream: ServerStream<Resp, R> + 'static,
    F: for<'a> Fn(
        &'a R::Context<'a>,
        Metadata,
        Req,
    ) -> Result<ServerStreamingReply<Stream>, Status>,
{
    fn call<'cx>(
        &self,
        cx: &'cx R::Context<'cx>,
        metadata: Metadata,
        request: RawRequest<'_>,
    ) -> Result<RawResponse<R>, Status> {
        let RawRequest::Buffered(frame) = request else {
            return Err(Status::new(
                GrpcStatusCode::Internal,
                "unexpected streaming request",
            ));
        };
        let request =
            codec::decode_message::<Req>(frame, self.max_message_len).map_err(|error| {
                if matches!(error, Error::SizeLimit { .. }) {
                    Status::new(GrpcStatusCode::ResourceExhausted, "request too large")
                } else {
                    Status::new(GrpcStatusCode::InvalidArgument, "invalid request")
                }
            })?;
        let reply = (self.handler)(cx, metadata, request)?;
        Ok(RawResponse::ServerStreaming(RawStreamReply {
            metadata: reply.metadata,
            stream: Box::new(EncodedServerStream {
                stream: reply.stream,
                max_message_len: self.max_message_len,
                _marker: std::marker::PhantomData,
            }),
            trailers: reply.trailers,
        }))
    }
}

struct TypedClientStreamingHandler<Req, Resp, F> {
    handler: F,
    max_message_len: usize,
    _marker: std::marker::PhantomData<fn(Req) -> Resp>,
}

impl<R, Req, Resp, F> MethodHandler<R> for TypedClientStreamingHandler<Req, Resp, F>
where
    R: GrpcRuntime,
    Req: Message + Default,
    Resp: Message,
    F: for<'a, 's> Fn(
        &'a R::Context<'a>,
        Metadata,
        ClientStreamingRequest<'s, Req>,
    ) -> Result<UnaryReply<Resp>, Status>,
{
    fn is_client_streaming(&self) -> bool {
        true
    }

    fn call<'cx>(
        &self,
        cx: &'cx R::Context<'cx>,
        metadata: Metadata,
        request: RawRequest<'_>,
    ) -> Result<RawResponse<R>, Status> {
        let RawRequest::Streaming(stream) = request else {
            return Err(Status::new(
                GrpcStatusCode::Internal,
                "expected streaming request",
            ));
        };
        let request = ClientStreamingRequest {
            stream,
            max_message_len: self.max_message_len,
            _marker: std::marker::PhantomData,
        };
        let reply = (self.handler)(cx, metadata, request)?;
        let mut encoded = Vec::new();
        reply
            .message
            .encode(&mut encoded)
            .map_err(|_| Status::new(GrpcStatusCode::Internal, "encode failed"))?;
        let message = codec::encode_bytes(&encoded, self.max_message_len)
            .map_err(|_| Status::new(GrpcStatusCode::ResourceExhausted, "response too large"))?;
        Ok(RawResponse::Unary(RawReply {
            metadata: reply.metadata,
            message,
            trailers: reply.trailers,
        }))
    }
}

struct EncodedServerStream<S, M> {
    stream: S,
    max_message_len: usize,
    _marker: std::marker::PhantomData<fn() -> M>,
}

impl<R, S, M> RawServerStream<R> for EncodedServerStream<S, M>
where
    R: GrpcRuntime,
    S: ServerStream<M, R>,
    M: Message,
{
    fn next_bytes<'cx>(&mut self, cx: &'cx R::Context<'cx>) -> Result<Option<Bytes>, Status> {
        let Some(message) = self.stream.next(cx)? else {
            return Ok(None);
        };
        let frame = codec::encode_message(&message, self.max_message_len).map_err(|error| {
            if matches!(error, Error::SizeLimit { .. }) {
                Status::new(GrpcStatusCode::ResourceExhausted, "response too large")
            } else {
                Status::new(GrpcStatusCode::Internal, "encode failed")
            }
        })?;
        Ok(Some(frame))
    }
}

struct HttpRawClientStream<'a, C, S> {
    cx: &'a C,
    http: &'a mut h2::RuntimeServerConnection<S>,
    stream_id: h2::StreamId,
    max_message_len: usize,
    data: Bytes,
    data_offset: usize,
    frame: BytesMut,
    finished: bool,
}

impl<'a, C, S> HttpRawClientStream<'a, C, S> {
    fn new(
        cx: &'a C,
        http: &'a mut h2::RuntimeServerConnection<S>,
        stream_id: h2::StreamId,
        max_message_len: usize,
    ) -> Self {
        Self {
            cx,
            http,
            stream_id,
            max_message_len,
            data: Bytes::new(),
            data_offset: 0,
            frame: BytesMut::new(),
            finished: false,
        }
    }

    fn decode_buffered_frame(&mut self) -> Result<Option<Bytes>, Status> {
        while self.data_offset < self.data.len() {
            if self.frame.is_empty() && self.data.len() - self.data_offset >= codec::HEADER_LEN {
                let header_end = self.data_offset + codec::HEADER_LEN;
                let total_len = codec::frame_len(
                    &self.data[self.data_offset..header_end],
                    self.max_message_len,
                )
                .map_err(decode_status)?;
                let frame_end = self.data_offset + total_len;
                if frame_end <= self.data.len() {
                    let frame = self.data.slice(self.data_offset..frame_end);
                    self.data_offset = frame_end;
                    return Ok(Some(frame));
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

            let total_len =
                codec::frame_len(&self.frame, self.max_message_len).map_err(decode_status)?;
            let body_remaining = total_len - self.frame.len();
            let available = self.data.len() - self.data_offset;
            let copy_len = body_remaining.min(available);
            self.frame
                .extend_from_slice(&self.data[self.data_offset..self.data_offset + copy_len]);
            self.data_offset += copy_len;

            if self.frame.len() == total_len {
                let frame = self.frame.split().freeze();
                return Ok(Some(frame));
            }
        }

        self.data = Bytes::new();
        self.data_offset = 0;
        Ok(None)
    }
}

impl<C, S> RawClientStream for HttpRawClientStream<'_, C, S>
where
    C: HttpRuntime<S>,
    S: RuntimeSocket,
{
    fn next_frame(&mut self) -> Result<Option<Bytes>, Status> {
        if self.finished {
            return Ok(None);
        }
        loop {
            if let Some(frame) = self.decode_buffered_frame()? {
                return Ok(Some(frame));
            }
            match self
                .http
                .read_request_event(self.cx, self.stream_id)
                .map_err(transport_status)?
            {
                h2::RequestStreamEvent::Data(data) => {
                    self.data = data;
                    self.data_offset = 0;
                }
                h2::RequestStreamEvent::Trailers(_trailers) => {
                    self.finished = true;
                    if !self.frame.is_empty() {
                        return Err(Status::new(
                            GrpcStatusCode::InvalidArgument,
                            "incomplete gRPC frame before stream trailers",
                        ));
                    }
                    return Ok(None);
                }
            }
        }
    }
}

fn collect_request_body<'cx, R, S>(
    cx: &'cx R::Context<'cx>,
    http: &mut h2::RuntimeServerConnection<S>,
    stream_id: h2::StreamId,
    max_message_len: usize,
) -> Result<Bytes, Status>
where
    R: GrpcRuntime,
    R::Context<'cx>: HttpRuntime<S>,
    S: RuntimeSocket,
{
    let mut body = BytesMut::new();
    let limit = max_message_len.saturating_add(codec::HEADER_LEN);
    loop {
        match http
            .read_request_event(cx, stream_id)
            .map_err(transport_status)?
        {
            h2::RequestStreamEvent::Data(data) => {
                let next_len = body.len().saturating_add(data.len());
                if next_len > limit {
                    return Err(Status::new(
                        GrpcStatusCode::ResourceExhausted,
                        "request too large",
                    ));
                }
                body.extend_from_slice(&data);
            }
            h2::RequestStreamEvent::Trailers(_trailers) => return Ok(body.freeze()),
        }
    }
}

fn validate_request<B>(request: &Request<B>) -> Result<(), Status> {
    if request.method() != Method::POST {
        return Err(Status::new(
            GrpcStatusCode::InvalidArgument,
            "invalid method",
        ));
    }

    let content_type = request
        .headers()
        .get(CONTENT_TYPE)
        .ok_or_else(|| Status::new(GrpcStatusCode::InvalidArgument, "missing content-type"))?;
    if !is_grpc_content_type(content_type.as_bytes()) {
        return Err(Status::new(
            GrpcStatusCode::InvalidArgument,
            "invalid content-type",
        ));
    }
    if request
        .headers()
        .get(TE)
        .is_some_and(|value| value != "trailers")
    {
        return Err(Status::new(GrpcStatusCode::InvalidArgument, "invalid te"));
    }
    Ok(())
}

fn decode_status(error: Error) -> Status {
    if matches!(error, Error::SizeLimit { .. }) {
        Status::new(GrpcStatusCode::ResourceExhausted, "request too large")
    } else {
        Status::new(GrpcStatusCode::InvalidArgument, "invalid request")
    }
}

fn transport_status(error: kimojio_stack_http::Error) -> Status {
    Status::new(GrpcStatusCode::Unavailable, error.to_string())
}

fn is_grpc_content_type(value: &[u8]) -> bool {
    value == b"application/grpc" || value.starts_with(b"application/grpc+")
}

fn write_reply<'cx, R, S>(
    cx: &'cx R::Context<'cx>,
    http: &mut h2::RuntimeServerConnection<S>,
    stream_id: h2::StreamId,
    reply: RawReply,
) -> Result<(), Error>
where
    R: GrpcRuntime,
    R::Context<'cx>: HttpRuntime<S>,
    S: RuntimeSocket,
{
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/grpc");
    for (name, value) in reply.metadata.into_http_headers() {
        let Some(name) = name else {
            return Err(Error::Protocol("metadata continuation header unsupported"));
        };
        response = response.header(name, value);
    }
    let response = response
        .body(Body::from_bytes(reply.message, BodyLimits::new(usize::MAX)).map_err(Error::from)?)
        .map_err(|_| Error::Protocol("failed to build gRPC response"))?;
    let mut trailers = Status::ok().to_trailers()?;
    for (name, value) in reply.trailers.into_http_headers() {
        let Some(name) = name else {
            return Err(Error::Protocol("metadata continuation header unsupported"));
        };
        trailers.insert(name, value);
    }
    http.send_response_with_trailers(cx, stream_id, &response, Some(&trailers))
        .map_err(Error::from)
}

fn write_streaming_reply<'cx, R, S>(
    cx: &'cx R::Context<'cx>,
    http: &mut h2::RuntimeServerConnection<S>,
    stream_id: h2::StreamId,
    mut reply: RawStreamReply<R>,
) -> Result<(), Error>
where
    R: GrpcRuntime,
    R::Context<'cx>: HttpRuntime<S>,
    S: RuntimeSocket,
{
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/grpc");
    for (name, value) in reply.metadata.into_http_headers() {
        let Some(name) = name else {
            return Err(Error::Protocol("metadata continuation header unsupported"));
        };
        response = response.header(name, value);
    }
    let response = response
        .body(())
        .map_err(|_| Error::Protocol("failed to build gRPC streaming response"))?;
    http.send_response_headers(cx, stream_id, &response)
        .map_err(Error::from)?;

    loop {
        match reply.stream.next_bytes(cx) {
            Ok(Some(message)) => http
                .send_response_data(cx, stream_id, &message)
                .map_err(Error::from)?,
            Ok(None) => {
                let mut trailers = Status::ok().to_trailers()?;
                for (name, value) in reply.trailers.into_http_headers() {
                    let Some(name) = name else {
                        return Err(Error::Protocol("metadata continuation header unsupported"));
                    };
                    trailers.insert(name, value);
                }
                return http
                    .finish_response_stream(cx, stream_id, Some(&trailers))
                    .map_err(Error::from);
            }
            Err(status) => {
                let trailers = status.to_trailers()?;
                return http
                    .finish_response_stream(cx, stream_id, Some(&trailers))
                    .map_err(Error::from);
            }
        }
    }
}

fn write_status<'cx, R, S>(
    cx: &'cx R::Context<'cx>,
    http: &mut h2::RuntimeServerConnection<S>,
    stream_id: h2::StreamId,
    status: Status,
) -> Result<(), Error>
where
    R: GrpcRuntime,
    R::Context<'cx>: HttpRuntime<S>,
    S: RuntimeSocket,
{
    let response = Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/grpc")
        .body(Body::empty())
        .map_err(|_| Error::Protocol("failed to build gRPC status response"))?;
    let trailers = status.to_trailers()?;
    http.send_response_with_trailers(cx, stream_id, &response, Some(&trailers))
        .map_err(Error::from)
}
