// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Router body helper types.

use bytes::Bytes;
use http::{HeaderMap, Response};
use kimojio_stack_http::{Body, BodyBuilder, BodyLimits};

/// One chunk in a streaming response body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamingChunk {
    /// Chunk bytes.
    pub data: Bytes,
    /// Whether this chunk finishes the stream.
    pub finish: bool,
}

impl StreamingChunk {
    /// Creates a non-terminal data chunk.
    pub fn data(data: impl Into<Bytes>) -> Self {
        Self {
            data: data.into(),
            finish: false,
        }
    }

    /// Creates an empty terminal chunk.
    pub fn finish() -> Self {
        Self {
            data: Bytes::new(),
            finish: true,
        }
    }
}

/// Explicit HTTP/2 streaming response representation.
///
/// Buffered HTTP/1.1 serving uses `Response<Body>`. Server integration for this
/// type is intentionally handled by the HTTP/2-specific serve adapter because
/// streaming DATA and terminal trailers require protocol-specific APIs.
#[derive(Clone, Debug)]
pub struct StreamingBody {
    response: Response<()>,
    chunks: Vec<StreamingChunk>,
}

impl StreamingBody {
    /// Creates a streaming response from headers/status and chunks.
    pub fn new(response: Response<()>, chunks: Vec<StreamingChunk>) -> Self {
        Self { response, chunks }
    }

    /// Creates a streaming response from a status and data chunks.
    pub fn from_chunks(status: http::StatusCode, chunks: Vec<StreamingChunk>) -> Self {
        let response = Response::builder()
            .status(status)
            .body(())
            .expect("valid streaming response");
        Self { response, chunks }
    }

    /// Borrows the response head.
    pub fn response(&self) -> &Response<()> {
        &self.response
    }

    /// Borrows streaming chunks.
    pub fn chunks(&self) -> &[StreamingChunk] {
        &self.chunks
    }

    /// Returns the response headers.
    pub fn headers(&self) -> &HeaderMap {
        self.response.headers()
    }

    /// Converts this streaming body to a buffered response.
    pub fn into_buffered_response(self) -> Response<Body> {
        let mut bytes = BodyBuilder::new(BodyLimits::default());
        for chunk in &self.chunks {
            if bytes.append(&chunk.data).is_err() {
                return Response::builder()
                    .status(http::StatusCode::PAYLOAD_TOO_LARGE)
                    .body(Body::empty())
                    .expect("valid too-large streaming response");
            }
        }
        let mut builder = Response::builder().status(self.response.status());
        *builder
            .headers_mut()
            .expect("new response builder has headers") = self.response.headers().clone();
        builder
            .body(bytes.finish())
            .expect("valid buffered streaming response")
    }
}
