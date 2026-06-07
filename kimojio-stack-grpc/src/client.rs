// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use http::{
    HeaderMap, Request, Response, StatusCode,
    header::{CONTENT_TYPE, TE},
};
use kimojio_stack::RuntimeContext;
use kimojio_stack_http::{Body, BodyLimits, Trailers, h2};
use prost::Message;

use crate::{Error, Metadata, Status, StatusCode as GrpcStatusCode, codec, status::GRPC_STATUS};

/// Shared configuration for unary gRPC clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientConfig {
    pub max_message_len: usize,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            max_message_len: 4 * 1024 * 1024,
        }
    }
}

#[derive(Debug)]
pub struct UnaryResponse<M> {
    pub metadata: Metadata,
    pub message: M,
    pub status: Status,
    pub trailers: Metadata,
}

pub struct UnaryClient {
    http: h2::ClientConnection,
    config: ClientConfig,
}

impl UnaryClient {
    pub fn new(http: h2::ClientConnection, config: ClientConfig) -> Self {
        Self { http, config }
    }

    pub fn call<Req, Resp>(
        &mut self,
        cx: &RuntimeContext<'_>,
        path: &str,
        metadata: Metadata,
        request: &Req,
    ) -> Result<UnaryResponse<Resp>, Error>
    where
        Req: Message,
        Resp: Message + Default,
    {
        let body = codec::encode_message(request, self.config.max_message_len)?;
        let http_request = build_request(path, metadata, body)?;
        let stream_id = self.http.send_request(cx, &http_request)?;
        let response = self.http.read_response_with_trailers(cx, stream_id)?;
        parse_response(response, self.config.max_message_len)
    }

    pub fn close(self, cx: &RuntimeContext<'_>) -> Result<(), Error> {
        self.http.close(cx).map_err(Error::from)
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
    for (name, value) in metadata.into_headers() {
        let Some(name) = name else {
            return Err(Error::Protocol("metadata continuation header unsupported"));
        };
        builder = builder.header(name, value);
    }
    builder
        .body(body)
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
    let metadata = Metadata::from_headers(response.response.headers().clone())?;
    let trailers = Metadata::from_headers(response.trailers.into_map())?;
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

fn validate_content_type(response: &Response<Body>) -> Result<(), Error> {
    let value = response
        .headers()
        .get(CONTENT_TYPE)
        .ok_or(Error::Protocol("missing gRPC content-type"))?;
    if value.as_bytes().starts_with(b"application/grpc") {
        Ok(())
    } else {
        Err(Error::Protocol("invalid gRPC content-type"))
    }
}
