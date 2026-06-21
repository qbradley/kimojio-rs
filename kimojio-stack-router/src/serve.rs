// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Server adapters for stackful routers.

use http::{Request, Response};
use kimojio_stack::RuntimeSocket;
use kimojio_stack_http::{
    Body, BodyBuilder, Error, HttpConfig, HttpRuntime, RuntimeStackTransport, Trailers, h2, http1,
};
use kimojio_stack_tower::Service;

use crate::{IntoResponse, StreamingBody};

/// Serves one protocol-neutral buffered request using an existing server connection and service.
pub fn serve_one<Cx, S, Sv>(
    cx: &Cx,
    server: &mut kimojio_stack_http::server::RuntimeServerConnection<S>,
    service: &mut Sv,
) -> Result<bool, Error>
where
    Cx: HttpRuntime<S>,
    S: RuntimeSocket,
    Sv: Service<Cx, Request<Body>, Response = Response<Body>>,
    Sv::Error: IntoResponse,
{
    server.serve_one(cx, |request| {
        Ok(service
            .call(cx, request)
            .unwrap_or_else(IntoResponse::into_response))
    })
}

/// Serves one buffered HTTP/1.1 request using a transport and service.
pub fn serve_http1_one<Cx, S, Sv>(
    cx: &Cx,
    transport: RuntimeStackTransport<S>,
    service: &mut Sv,
    config: HttpConfig,
) -> Result<bool, Error>
where
    Cx: HttpRuntime<S>,
    S: RuntimeSocket,
    Sv: Service<Cx, Request<Body>, Response = Response<Body>>,
    Sv::Error: IntoResponse,
{
    let mut server = http1::RuntimeServerConnection::new(transport, config);
    server.serve_one(cx, |request| {
        Ok(service
            .call(cx, request)
            .unwrap_or_else(IntoResponse::into_response))
    })
}

/// Serves up to `requests` buffered HTTP/2 requests with a service and gracefully closes.
pub fn serve_h2_buffered_requests<Cx, S, Sv>(
    cx: &Cx,
    transport: RuntimeStackTransport<S>,
    service: &mut Sv,
    config: HttpConfig,
    requests: usize,
) -> Result<(), Error>
where
    Cx: HttpRuntime<S>,
    S: RuntimeSocket,
    Sv: Service<Cx, Request<Body>, Response = Response<Body>>,
    Sv::Error: IntoResponse,
{
    let mut server = h2::RuntimeServerConnection::new(transport, config);
    for _ in 0..requests {
        if !server.serve_one(cx, |request| {
            Ok(service
                .call(cx, request)
                .unwrap_or_else(IntoResponse::into_response))
        })? {
            break;
        }
    }
    server.shutdown_write_and_close_after_peer(cx)
}

/// Reads a complete HTTP/2 request body from a streaming request head.
pub fn read_h2_request_body<Cx, S>(
    cx: &Cx,
    server: &mut h2::RuntimeServerConnection<S>,
    stream_id: h2::StreamId,
    limits: kimojio_stack_http::BodyLimits,
) -> Result<Body, Error>
where
    Cx: HttpRuntime<S>,
    S: RuntimeSocket,
{
    let mut body = BodyBuilder::new(limits);
    loop {
        match server.read_request_event(cx, stream_id)? {
            h2::RequestStreamEvent::Data(bytes) => body.append(&bytes)?,
            h2::RequestStreamEvent::Trailers(_) => return Ok(body.finish()),
        }
    }
}

/// Sends a streaming HTTP/2 response using protocol-specific APIs.
pub fn send_h2_streaming_response<Cx, S>(
    cx: &Cx,
    server: &mut h2::RuntimeServerConnection<S>,
    stream_id: h2::StreamId,
    response: StreamingBody,
) -> Result<(), Error>
where
    Cx: HttpRuntime<S>,
    S: RuntimeSocket,
{
    server.send_response_headers(cx, stream_id, response.response())?;
    for chunk in response.chunks() {
        if !chunk.data.is_empty() {
            server.send_response_data(cx, stream_id, &chunk.data)?;
        }
        if chunk.finish {
            return server.finish_response_stream(cx, stream_id, None);
        }
    }
    server.finish_response_stream(cx, stream_id, None)
}

/// Serves one HTTP/2 streaming request and response.
pub fn serve_h2_streaming_once<Cx, S>(
    cx: &Cx,
    server: &mut h2::RuntimeServerConnection<S>,
    limits: kimojio_stack_http::BodyLimits,
    handler: impl FnOnce(Request<Body>) -> Result<StreamingBody, Error>,
) -> Result<bool, Error>
where
    Cx: HttpRuntime<S>,
    S: RuntimeSocket,
{
    let Some(head) = server.accept_request_headers(cx)? else {
        return Ok(false);
    };
    let (parts, ()) = head.request.into_parts();
    let body = read_h2_request_body(cx, server, head.stream_id, limits)?;
    let request = Request::from_parts(parts, body);
    let response = handler(request)?;
    send_h2_streaming_response(cx, server, head.stream_id, response)?;
    Ok(true)
}

/// Converts an ordinary response into a response head with empty body marker.
pub fn response_head(response: &Response<Body>) -> Response<()> {
    let mut builder = Response::builder()
        .status(response.status())
        .version(response.version());
    *builder
        .headers_mut()
        .expect("new response builder has headers") = response.headers().clone();
    builder.body(()).expect("valid response head")
}

/// Empty trailers helper for callers that need an explicit terminal trailer set.
pub fn empty_trailers() -> Trailers {
    Trailers::new()
}
