// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::io::Read;

use bytes::Bytes;
use flate2::read::{GzDecoder, ZlibDecoder};
use http::{Request, header};
use kimojio_stack_http::Body;

use crate::{BoxError, Layer, Readiness, Service, ServiceError};

use super::compression::Encoding;

/// Request decompression layer.
#[derive(Clone, Copy, Debug, Default)]
pub struct DecompressionLayer;

impl<S> Layer<S> for DecompressionLayer {
    type Service = Decompression<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Decompression { inner }
    }
}

/// Request decompression middleware.
pub struct Decompression<S> {
    inner: S,
}

impl<Cx, S> Service<Cx, Request<Body>> for Decompression<S>
where
    S: Service<Cx, Request<Body>>,
    S::Error: Into<BoxError>,
{
    type Response = S::Response;
    type Error = ServiceError;

    fn ready(&mut self, cx: &Cx) -> Result<Readiness, Self::Error> {
        self.inner
            .ready(cx)
            .map_err(|error| ServiceError::Inner(error.into()))
    }

    fn call(&mut self, cx: &Cx, mut request: Request<Body>) -> Result<Self::Response, Self::Error> {
        if let Some(encoding) = request
            .headers()
            .get(header::CONTENT_ENCODING)
            .and_then(|value| value.to_str().ok())
            .and_then(parse_encoding)
        {
            let decoded = decode(encoding, request.body().as_bytes())?;
            *request.body_mut() = Body::from_bytes(decoded, Default::default())
                .map_err(|_| ServiceError::InvalidRequest("decompressed body exceeded limit"))?;
            request.headers_mut().remove(header::CONTENT_ENCODING);
        } else if request.headers().contains_key(header::CONTENT_ENCODING) {
            return Err(ServiceError::InvalidRequest("unsupported content encoding"));
        }
        self.inner
            .call(cx, request)
            .map_err(|error| ServiceError::Inner(error.into()))
    }
}

fn parse_encoding(value: &str) -> Option<Encoding> {
    match value {
        "gzip" => Some(Encoding::Gzip),
        "deflate" => Some(Encoding::Deflate),
        "br" => Some(Encoding::Brotli),
        "zstd" => Some(Encoding::Zstd),
        _ => None,
    }
}

pub(crate) fn decode(encoding: Encoding, bytes: &[u8]) -> Result<Bytes, ServiceError> {
    let mut output = Vec::new();
    match encoding {
        Encoding::Gzip => {
            GzDecoder::new(bytes)
                .read_to_end(&mut output)
                .map_err(|_| ServiceError::InvalidRequest("gzip decompression failed"))?;
        }
        Encoding::Deflate => {
            ZlibDecoder::new(bytes)
                .read_to_end(&mut output)
                .map_err(|_| ServiceError::InvalidRequest("deflate decompression failed"))?;
        }
        Encoding::Brotli => {
            brotli::Decompressor::new(bytes, 4096)
                .read_to_end(&mut output)
                .map_err(|_| ServiceError::InvalidRequest("brotli decompression failed"))?;
        }
        Encoding::Zstd => {
            output = zstd::stream::decode_all(bytes)
                .map_err(|_| ServiceError::InvalidRequest("zstd decompression failed"))?;
        }
    }
    Ok(Bytes::from(output))
}
