// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(any(
    feature = "compression-br",
    feature = "compression-gzip",
    feature = "compression-zstd"
))]
use std::io::Read;

use bytes::Bytes;
#[cfg(feature = "compression-gzip")]
use flate2::read::{GzDecoder, ZlibDecoder};
use http::{Request, header};
#[cfg(any(
    feature = "compression-br",
    feature = "compression-gzip",
    feature = "compression-zstd"
))]
use kimojio_stack_http::BodyBuilder;
use kimojio_stack_http::{Body, BodyLimits};

use crate::{BoxError, Layer, Readiness, Service, ServiceError};

use super::compression::Encoding;

/// Request decompression layer.
#[derive(Clone, Copy, Debug, Default)]
pub struct DecompressionLayer {
    limits: BodyLimits,
}

impl DecompressionLayer {
    /// Creates a decompression layer with default decoded body limits.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a decompression layer with an explicit decoded body limit.
    pub fn with_limits(limits: BodyLimits) -> Self {
        Self { limits }
    }
}

impl<S> Layer<S> for DecompressionLayer {
    type Service = Decompression<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Decompression {
            inner,
            limits: self.limits,
        }
    }
}

/// Request decompression middleware.
pub struct Decompression<S> {
    inner: S,
    limits: BodyLimits,
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
            let decoded = decode(encoding, request.body().as_bytes(), self.limits)?;
            *request.body_mut() = Body::from_bytes(decoded, self.limits)
                .map_err(|_| ServiceError::InvalidRequest("decompressed body exceeded limit"))?;
            request.headers_mut().remove(header::CONTENT_ENCODING);
            request.headers_mut().remove(header::CONTENT_LENGTH);
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

pub(crate) fn decode(
    encoding: Encoding,
    bytes: &[u8],
    limits: BodyLimits,
) -> Result<Bytes, ServiceError> {
    #[cfg(not(any(
        feature = "compression-br",
        feature = "compression-gzip",
        feature = "compression-zstd"
    )))]
    {
        let _ = (encoding, bytes, limits);
        Err(ServiceError::InvalidRequest("unsupported content encoding"))
    }
    #[cfg(any(
        feature = "compression-br",
        feature = "compression-gzip",
        feature = "compression-zstd"
    ))]
    {
        let mut reader: Box<dyn Read + '_> = match encoding {
            #[cfg(feature = "compression-gzip")]
            Encoding::Gzip => Box::new(GzDecoder::new(bytes)),
            #[cfg(not(feature = "compression-gzip"))]
            Encoding::Gzip => {
                return Err(ServiceError::InvalidRequest("unsupported content encoding"));
            }
            #[cfg(feature = "compression-gzip")]
            Encoding::Deflate => Box::new(ZlibDecoder::new(bytes)),
            #[cfg(not(feature = "compression-gzip"))]
            Encoding::Deflate => {
                return Err(ServiceError::InvalidRequest("unsupported content encoding"));
            }
            #[cfg(feature = "compression-br")]
            Encoding::Brotli => Box::new(brotli::Decompressor::new(bytes, 4096)),
            #[cfg(not(feature = "compression-br"))]
            Encoding::Brotli => {
                return Err(ServiceError::InvalidRequest("unsupported content encoding"));
            }
            #[cfg(feature = "compression-zstd")]
            Encoding::Zstd => Box::new(
                zstd::stream::read::Decoder::new(bytes)
                    .map_err(|_| ServiceError::InvalidRequest("zstd decompression failed"))?,
            ),
            #[cfg(not(feature = "compression-zstd"))]
            Encoding::Zstd => {
                return Err(ServiceError::InvalidRequest("unsupported content encoding"));
            }
        };
        let mut output = BodyBuilder::new(limits);
        let mut chunk = [0_u8; 8192];
        loop {
            let read = reader.read(&mut chunk).map_err(|_| match encoding {
                Encoding::Gzip => ServiceError::InvalidRequest("gzip decompression failed"),
                Encoding::Deflate => ServiceError::InvalidRequest("deflate decompression failed"),
                Encoding::Brotli => ServiceError::InvalidRequest("brotli decompression failed"),
                Encoding::Zstd => ServiceError::InvalidRequest("zstd decompression failed"),
            })?;
            if read == 0 {
                break;
            }
            output
                .append(&chunk[..read])
                .map_err(|_| ServiceError::InvalidRequest("decompressed body exceeded limit"))?;
        }
        Ok(output.finish().into_bytes())
    }
}
