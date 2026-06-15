// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::BTreeMap;

use bytes::Bytes;
use http::{
    Method, Request, Response, StatusCode,
    header::{CONTENT_TYPE, TE},
};
use kimojio_stack::{RuntimeContext, channel};
use kimojio_stack_http::{Body, BodyLimits, h2};
use prost::Message;

use crate::{Error, Metadata, Status, StatusCode as GrpcStatusCode, codec};

/// Shared configuration for unary gRPC servers.
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

/// Stackful gRPC server that dispatches incoming requests to registered unary and
/// server-streaming handlers over an HTTP/2 connection.
///
/// # Runtime migration boundary
///
/// The server dispatch table is independent of socket or ring ownership. Runtime
/// I/O enters only through the `h2::ServerConnection` argument to
/// [`serve_one`](Self::serve_one), so the generic HTTP/2 server connection can be
/// adopted here without introducing a separate gRPC runtime trait.
pub struct UnaryServer {
    handlers: BTreeMap<String, Box<dyn MethodHandler>>,
    config: ServerConfig,
}

impl UnaryServer {
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
    /// [`RuntimeContext`] to perform stackful I/O or synchronization.
    pub fn add_unary<Req, Resp, F>(&mut self, path: impl Into<String>, handler: F)
    where
        Req: Message + Default + 'static,
        Resp: Message + 'static,
        F: for<'a> Fn(&'a RuntimeContext<'a>, Metadata, Req) -> Result<UnaryReply<Resp>, Status>
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
        Stream: ServerStream<Resp> + 'static,
        F: for<'a> Fn(
                &'a RuntimeContext<'a>,
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

    /// Serves one HTTP/2 request through the registered handler table.
    ///
    /// Returns `Ok(false)` when the underlying HTTP/2 connection reaches clean
    /// EOF before another request. Handler `Err(Status)` values are converted
    /// into gRPC status responses; transport/protocol failures are returned as
    /// [`Error`] so the serve loop can stop.
    pub fn serve_one(
        &self,
        cx: &RuntimeContext<'_>,
        http: &mut h2::ServerConnection,
    ) -> Result<bool, Error> {
        let Some(incoming) = http.accept(cx).map_err(Error::from)? else {
            return Ok(false);
        };
        let result = self.dispatch(cx, incoming.request);
        match result {
            Ok(RawResponse::Unary(reply)) => write_reply(cx, http, incoming.stream_id, reply),
            Ok(RawResponse::ServerStreaming(reply)) => {
                write_streaming_reply(cx, http, incoming.stream_id, reply)
            }
            Err(status) => write_status(cx, http, incoming.stream_id, status),
        }?;
        Ok(true)
    }

    fn dispatch(
        &self,
        cx: &RuntimeContext<'_>,
        request: Request<Body>,
    ) -> Result<RawResponse, Status> {
        validate_request(&request)?;
        let path = request.uri().path();
        let Some(handler) = self.handlers.get(path) else {
            return Err(Status::new(GrpcStatusCode::Unimplemented, "unknown method"));
        };
        let metadata = Metadata::from_http_headers(request.headers().clone())
            .map_err(|_| Status::new(GrpcStatusCode::InvalidArgument, "invalid metadata"))?;
        handler.call(cx, metadata, request.body().as_bytes())
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
pub trait ServerStream<M> {
    /// Returns the next response message, `Ok(None)` when the stream is complete, or
    /// `Err(Status)` to terminate the stream with a non-OK status.
    fn next(&mut self, cx: &RuntimeContext<'_>) -> Result<Option<M>, Status>;
}

impl<M, F> ServerStream<M> for F
where
    F: for<'a> FnMut(&'a RuntimeContext<'a>) -> Result<Option<M>, Status>,
{
    fn next(&mut self, cx: &RuntimeContext<'_>) -> Result<Option<M>, Status> {
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

impl<M> ServerStream<M> for ReceiverStream<M> {
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

struct RawStreamReply {
    metadata: Metadata,
    stream: Box<dyn RawServerStream>,
    trailers: Metadata,
}

enum RawResponse {
    Unary(RawReply),
    ServerStreaming(RawStreamReply),
}

trait MethodHandler {
    fn call(
        &self,
        cx: &RuntimeContext<'_>,
        metadata: Metadata,
        frame: &[u8],
    ) -> Result<RawResponse, Status>;
}

trait RawServerStream {
    fn next_bytes(&mut self, cx: &RuntimeContext<'_>) -> Result<Option<Bytes>, Status>;
}

struct TypedUnaryHandler<Req, Resp, F> {
    handler: F,
    max_message_len: usize,
    _marker: std::marker::PhantomData<fn(Req) -> Resp>,
}

impl<Req, Resp, F> MethodHandler for TypedUnaryHandler<Req, Resp, F>
where
    Req: Message + Default,
    Resp: Message,
    F: for<'a> Fn(&'a RuntimeContext<'a>, Metadata, Req) -> Result<UnaryReply<Resp>, Status>,
{
    fn call(
        &self,
        cx: &RuntimeContext<'_>,
        metadata: Metadata,
        frame: &[u8],
    ) -> Result<RawResponse, Status> {
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

impl<Req, Resp, Stream, F> MethodHandler for TypedServerStreamingHandler<Req, Resp, Stream, F>
where
    Req: Message + Default,
    Resp: Message + 'static,
    Stream: ServerStream<Resp> + 'static,
    F: for<'a> Fn(
        &'a RuntimeContext<'a>,
        Metadata,
        Req,
    ) -> Result<ServerStreamingReply<Stream>, Status>,
{
    fn call(
        &self,
        cx: &RuntimeContext<'_>,
        metadata: Metadata,
        frame: &[u8],
    ) -> Result<RawResponse, Status> {
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

struct EncodedServerStream<S, M> {
    stream: S,
    max_message_len: usize,
    _marker: std::marker::PhantomData<fn() -> M>,
}

impl<S, M> RawServerStream for EncodedServerStream<S, M>
where
    S: ServerStream<M>,
    M: Message,
{
    fn next_bytes(&mut self, cx: &RuntimeContext<'_>) -> Result<Option<Bytes>, Status> {
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

fn validate_request(request: &Request<Body>) -> Result<(), Status> {
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

fn is_grpc_content_type(value: &[u8]) -> bool {
    value == b"application/grpc" || value.starts_with(b"application/grpc+")
}

fn write_reply(
    cx: &RuntimeContext<'_>,
    http: &mut h2::ServerConnection,
    stream_id: h2::StreamId,
    reply: RawReply,
) -> Result<(), Error> {
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

fn write_streaming_reply(
    cx: &RuntimeContext<'_>,
    http: &mut h2::ServerConnection,
    stream_id: h2::StreamId,
    mut reply: RawStreamReply,
) -> Result<(), Error> {
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

fn write_status(
    cx: &RuntimeContext<'_>,
    http: &mut h2::ServerConnection,
    stream_id: h2::StreamId,
    status: Status,
) -> Result<(), Error> {
    let response = Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/grpc")
        .body(Body::empty())
        .map_err(|_| Error::Protocol("failed to build gRPC status response"))?;
    let trailers = status.to_trailers()?;
    http.send_response_with_trailers(cx, stream_id, &response, Some(&trailers))
        .map_err(Error::from)
}
