// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::io::{Read, Write};

use openssl::ssl::{HandshakeError, SslAcceptor, SslConnector, SslStream};

use crate::{OperationError, OperationResult, TlsPoolError};

pub(crate) trait TlsTransport: Read + Write + Send {}

impl<T> TlsTransport for T where T: Read + Write + Send {}

pub(crate) type BoxedTransport = Box<dyn TlsTransport>;
pub(crate) type BoxedTlsStream = SslStream<BoxedTransport>;

pub(crate) fn connect<S>(
    connector: &SslConnector,
    domain: &str,
    stream: S,
) -> Result<BoxedTlsStream, TlsPoolError>
where
    S: Read + Write + Send + 'static,
{
    connector
        .connect(domain, Box::new(stream) as BoxedTransport)
        .map_err(map_handshake_error)
        .map_err(TlsPoolError::from)
}

pub(crate) fn accept<S>(acceptor: &SslAcceptor, stream: S) -> Result<BoxedTlsStream, TlsPoolError>
where
    S: Read + Write + Send + 'static,
{
    acceptor
        .accept(Box::new(stream) as BoxedTransport)
        .map_err(map_handshake_error)
        .map_err(TlsPoolError::from)
}

pub(crate) fn map_io<T>(result: std::io::Result<T>) -> OperationResult<T> {
    result.map_err(OperationError::from)
}

fn map_handshake_error<S>(error: HandshakeError<S>) -> OperationError {
    match error {
        HandshakeError::SetupFailure(error) => OperationError::Tls(error),
        HandshakeError::Failure(error) => OperationError::Handshake(error.error().to_string()),
        HandshakeError::WouldBlock(error) => OperationError::Handshake(error.error().to_string()),
    }
}
