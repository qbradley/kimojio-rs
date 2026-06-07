// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use kimojio_stack::{Errno, RuntimeContext};
use kimojio_stack_tls::TlsStream;
use rustix::{fd::OwnedFd, net::Shutdown};

use crate::error::Error;

/// Stackful plaintext or TLS transport over a connected socket.
pub enum StackTransport {
    Plaintext(OwnedFd),
    Tls(TlsStream),
}

impl StackTransport {
    pub fn plaintext(fd: OwnedFd) -> Self {
        Self::Plaintext(fd)
    }

    pub fn tls(stream: TlsStream) -> Self {
        Self::Tls(stream)
    }

    pub fn read(&mut self, cx: &RuntimeContext<'_>, buf: &mut [u8]) -> Result<usize, Error> {
        match self {
            Self::Plaintext(fd) => cx.read(fd, buf).map_err(Error::io),
            Self::Tls(stream) => stream.read(cx, buf).map_err(Error::tls),
        }
    }

    pub fn read_exact_or_eof(
        &mut self,
        cx: &RuntimeContext<'_>,
        mut buf: &mut [u8],
    ) -> Result<usize, Error> {
        let requested = buf.len();
        while !buf.is_empty() {
            let amount = self.read(cx, buf)?;
            if amount == 0 {
                break;
            }
            buf = &mut buf[amount..];
        }
        Ok(requested - buf.len())
    }

    pub fn write(&mut self, cx: &RuntimeContext<'_>, buf: &[u8]) -> Result<usize, Error> {
        match self {
            Self::Plaintext(fd) => cx.write(fd, buf).map_err(Error::io),
            Self::Tls(stream) => stream.write(cx, buf).map_err(Error::tls),
        }
    }

    pub fn write_all(&mut self, cx: &RuntimeContext<'_>, mut buf: &[u8]) -> Result<(), Error> {
        while !buf.is_empty() {
            let amount = self.write(cx, buf)?;
            if amount == 0 {
                return Err(Error::io(Errno::PIPE));
            }
            buf = &buf[amount..];
        }
        Ok(())
    }

    pub fn shutdown_write(&mut self, cx: &RuntimeContext<'_>) -> Result<(), Error> {
        match self {
            Self::Plaintext(fd) => cx.shutdown(fd, Shutdown::Write).map_err(Error::io),
            Self::Tls(stream) => stream.shutdown_write(cx).map_err(Error::tls),
        }
    }

    pub fn shutdown(&mut self, cx: &RuntimeContext<'_>) -> Result<(), Error> {
        match self {
            Self::Plaintext(fd) => cx.shutdown(fd, Shutdown::Both).map_err(Error::io),
            Self::Tls(stream) => stream.shutdown(cx).map_err(Error::tls),
        }
    }

    pub fn close(self, cx: &RuntimeContext<'_>) -> Result<(), Error> {
        match self {
            Self::Plaintext(fd) => cx.close(fd).map_err(Error::io),
            Self::Tls(stream) => stream.close(cx).map_err(Error::tls),
        }
    }
}
