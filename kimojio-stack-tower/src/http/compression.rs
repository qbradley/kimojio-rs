// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::io::Write;

use bytes::Bytes;
use flate2::Compression as FlateCompression;
use flate2::write::{GzEncoder, ZlibEncoder};
use http::{Request, Response, header};
use kimojio_stack_http::Body;

use crate::{BoxError, Layer, Readiness, Service, ServiceError};

/// Supported response compression encodings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Encoding {
    /// gzip content encoding.
    Gzip,
    /// deflate/zlib content encoding.
    Deflate,
    /// brotli content encoding.
    Brotli,
    /// zstd content encoding.
    Zstd,
}

impl Encoding {
    fn as_str(self) -> &'static str {
        match self {
            Self::Gzip => "gzip",
            Self::Deflate => "deflate",
            Self::Brotli => "br",
            Self::Zstd => "zstd",
        }
    }
}

/// Response compression layer.
#[derive(Clone, Debug)]
pub struct CompressionLayer {
    encodings: Vec<Encoding>,
}

impl CompressionLayer {
    /// Creates a compression layer with allowed encodings.
    pub fn new(encodings: Vec<Encoding>) -> Self {
        Self { encodings }
    }

    /// Creates a layer supporting all implemented encodings.
    pub fn all() -> Self {
        Self::new(vec![
            Encoding::Gzip,
            Encoding::Deflate,
            Encoding::Brotli,
            Encoding::Zstd,
        ])
    }
}

impl<S> Layer<S> for CompressionLayer {
    type Service = Compression<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Compression {
            inner,
            encodings: self.encodings.clone(),
        }
    }
}

/// Response compression middleware.
pub struct Compression<S> {
    inner: S,
    encodings: Vec<Encoding>,
}

impl<Cx, S> Service<Cx, Request<Body>> for Compression<S>
where
    S: Service<Cx, Request<Body>, Response = Response<Body>>,
    S::Error: Into<BoxError>,
{
    type Response = Response<Body>;
    type Error = ServiceError;

    fn ready(&mut self, cx: &Cx) -> Result<Readiness, Self::Error> {
        self.inner
            .ready(cx)
            .map_err(|error| ServiceError::Inner(error.into()))
    }

    fn call(&mut self, cx: &Cx, request: Request<Body>) -> Result<Self::Response, Self::Error> {
        let accepted = request
            .headers()
            .get(header::ACCEPT_ENCODING)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let mut response = self
            .inner
            .call(cx, request)
            .map_err(|error| ServiceError::Inner(error.into()))?;
        if response.headers().contains_key(header::CONTENT_ENCODING) {
            return Ok(response);
        }
        let Some(encoding) = self
            .encodings
            .iter()
            .copied()
            .find(|encoding| accepts(&accepted, *encoding))
        else {
            return Ok(response);
        };
        let compressed = encode(encoding, response.body().as_bytes())?;
        *response.body_mut() = Body::from_bytes(compressed, Default::default())
            .map_err(|_| ServiceError::InvalidRequest("compressed body exceeded body limit"))?;
        response.headers_mut().insert(
            header::CONTENT_ENCODING,
            encoding.as_str().parse().expect("valid encoding header"),
        );
        Ok(response)
    }
}

fn accepts(header: &str, encoding: Encoding) -> bool {
    header
        .split(',')
        .map(str::trim)
        .any(|value| value == encoding.as_str() || value == "*")
}

pub(crate) fn encode(encoding: Encoding, bytes: &[u8]) -> Result<Bytes, ServiceError> {
    match encoding {
        Encoding::Gzip => {
            let mut encoder = GzEncoder::new(Vec::new(), FlateCompression::default());
            encoder
                .write_all(bytes)
                .map_err(|_| ServiceError::InvalidRequest("gzip compression failed"))?;
            Ok(Bytes::from(encoder.finish().map_err(|_| {
                ServiceError::InvalidRequest("gzip compression failed")
            })?))
        }
        Encoding::Deflate => {
            let mut encoder = ZlibEncoder::new(Vec::new(), FlateCompression::default());
            encoder
                .write_all(bytes)
                .map_err(|_| ServiceError::InvalidRequest("deflate compression failed"))?;
            Ok(Bytes::from(encoder.finish().map_err(|_| {
                ServiceError::InvalidRequest("deflate compression failed")
            })?))
        }
        Encoding::Brotli => {
            let mut output = Vec::new();
            {
                let mut writer = brotli::CompressorWriter::new(&mut output, 4096, 5, 22);
                writer
                    .write_all(bytes)
                    .map_err(|_| ServiceError::InvalidRequest("brotli compression failed"))?;
            }
            Ok(Bytes::from(output))
        }
        Encoding::Zstd => zstd::stream::encode_all(bytes, 0)
            .map(Bytes::from)
            .map_err(|_| ServiceError::InvalidRequest("zstd compression failed")),
    }
}
