// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::BTreeMap;

use bytes::Bytes;
use http::{
    Method, Request, Response, StatusCode,
    header::{CONTENT_TYPE, TE},
};
use kimojio_stack::RuntimeContext;
use kimojio_stack_http::{Body, BodyLimits, h2};
use prost::Message;

use crate::{Error, Metadata, Status, StatusCode as GrpcStatusCode, codec};

/// Shared configuration for unary gRPC servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerConfig {
    pub max_message_len: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            max_message_len: 4 * 1024 * 1024,
        }
    }
}

pub struct UnaryServer {
    handlers: BTreeMap<String, Box<dyn UnaryHandler>>,
    config: ServerConfig,
}

impl UnaryServer {
    pub fn new(config: ServerConfig) -> Self {
        Self {
            handlers: BTreeMap::new(),
            config,
        }
    }

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
            Ok(reply) => write_reply(cx, http, incoming.stream_id, reply),
            Err(status) => write_status(cx, http, incoming.stream_id, status),
        }?;
        Ok(true)
    }

    fn dispatch(
        &self,
        cx: &RuntimeContext<'_>,
        request: Request<Body>,
    ) -> Result<RawReply, Status> {
        validate_request(&request)?;
        let path = request.uri().path();
        let Some(handler) = self.handlers.get(path) else {
            return Err(Status::new(GrpcStatusCode::Unimplemented, "unknown method"));
        };
        let metadata = Metadata::from_headers(request.headers().clone())
            .map_err(|_| Status::new(GrpcStatusCode::InvalidArgument, "invalid metadata"))?;
        handler.call(cx, metadata, request.body().as_bytes())
    }
}

pub struct UnaryReply<M> {
    pub metadata: Metadata,
    pub message: M,
    pub trailers: Metadata,
}

impl<M> UnaryReply<M> {
    pub fn new(message: M) -> Self {
        Self {
            metadata: Metadata::new(),
            message,
            trailers: Metadata::new(),
        }
    }
}

struct RawReply {
    metadata: Metadata,
    message: Bytes,
    trailers: Metadata,
}

trait UnaryHandler {
    fn call(
        &self,
        cx: &RuntimeContext<'_>,
        metadata: Metadata,
        frame: &[u8],
    ) -> Result<RawReply, Status>;
}

struct TypedUnaryHandler<Req, Resp, F> {
    handler: F,
    max_message_len: usize,
    _marker: std::marker::PhantomData<fn(Req) -> Resp>,
}

impl<Req, Resp, F> UnaryHandler for TypedUnaryHandler<Req, Resp, F>
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
    ) -> Result<RawReply, Status> {
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
        Ok(RawReply {
            metadata: reply.metadata,
            message,
            trailers: reply.trailers,
        })
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
    for (name, value) in reply.metadata.into_headers() {
        let Some(name) = name else {
            return Err(Error::Protocol("metadata continuation header unsupported"));
        };
        response = response.header(name, value);
    }
    let response = response
        .body(Body::from_bytes(reply.message, BodyLimits::new(usize::MAX)).map_err(Error::from)?)
        .map_err(|_| Error::Protocol("failed to build gRPC response"))?;
    let mut trailers = Status::ok().to_trailers()?;
    for (name, value) in reply.trailers.into_headers() {
        let Some(name) = name else {
            return Err(Error::Protocol("metadata continuation header unsupported"));
        };
        trailers.insert(name, value);
    }
    http.send_response_with_trailers(cx, stream_id, &response, Some(&trailers))
        .map_err(Error::from)
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
