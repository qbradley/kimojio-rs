// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, RawFd};

use openssl::ssl::{HandshakeError, SslAcceptor, SslConnector, SslStream};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};

use crate::{OperationError, OperationResult, TlsPoolError};

pub(crate) trait TlsTransport: Read + Write + AsFd + Send {}

impl<T> TlsTransport for T where T: Read + Write + AsFd + Send {}

pub(crate) type BoxedTransport = Box<dyn TlsTransport>;
pub(crate) type BoxedTlsStream = SslStream<BoxedTransport>;

pub(crate) fn connect<S>(
    connector: &SslConnector,
    domain: &str,
    stream: S,
) -> Result<BoxedTlsStream, TlsPoolError>
where
    S: Read + Write + AsFd + Send + 'static,
{
    let tls = connector
        .connect(domain, Box::new(stream) as BoxedTransport)
        .map_err(map_handshake_error)
        .map_err(TlsPoolError::from)?;
    set_nonblocking(tls.get_ref())?;
    Ok(tls)
}

pub(crate) fn accept<S>(acceptor: &SslAcceptor, stream: S) -> Result<BoxedTlsStream, TlsPoolError>
where
    S: Read + Write + AsFd + Send + 'static,
{
    let tls = acceptor
        .accept(Box::new(stream) as BoxedTransport)
        .map_err(map_handshake_error)
        .map_err(TlsPoolError::from)?;
    set_nonblocking(tls.get_ref())?;
    Ok(tls)
}

fn map_handshake_error<S>(error: HandshakeError<S>) -> OperationError {
    match error {
        HandshakeError::SetupFailure(error) => OperationError::Tls(error),
        HandshakeError::Failure(error) => OperationError::Handshake(error.error().to_string()),
        HandshakeError::WouldBlock(error) => OperationError::Handshake(error.error().to_string()),
    }
}

pub(crate) fn raw_fd(tls: &BoxedTlsStream) -> RawFd {
    tls.get_ref().as_fd().as_raw_fd()
}

pub(crate) fn set_nonblocking_stream(tls: &BoxedTlsStream) -> OperationResult<()> {
    set_nonblocking(tls.get_ref()).map_err(|error| match error {
        TlsPoolError::Operation(error) => error,
        TlsPoolError::Config(error) => OperationError::Handshake(error.to_string()),
    })
}

pub(crate) fn set_blocking_stream(tls: &BoxedTlsStream) -> OperationResult<()> {
    let stream = tls.get_ref();
    let flags = fcntl_getfl(stream).map_err(|error| {
        OperationError::Io(std::io::Error::from_raw_os_error(error.raw_os_error()))
    })?;
    fcntl_setfl(stream, flags & !OFlags::NONBLOCK).map_err(|error| {
        OperationError::Io(std::io::Error::from_raw_os_error(error.raw_os_error()))
    })
}

fn set_nonblocking(stream: &BoxedTransport) -> Result<(), TlsPoolError> {
    let flags = fcntl_getfl(stream).map_err(|error| {
        TlsPoolError::Operation(OperationError::Io(std::io::Error::from_raw_os_error(
            error.raw_os_error(),
        )))
    })?;
    fcntl_setfl(stream, flags | OFlags::NONBLOCK).map_err(|error| {
        TlsPoolError::Operation(OperationError::Io(std::io::Error::from_raw_os_error(
            error.raw_os_error(),
        )))
    })
}
