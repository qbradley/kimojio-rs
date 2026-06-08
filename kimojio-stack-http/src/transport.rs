// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::time::{Duration, Instant};

use kimojio_stack::{Errno, RuntimeContext, WaitError, Waitable};
#[cfg(feature = "tls")]
use kimojio_stack_tls::TlsStream;
use rustix::{fd::OwnedFd, net::Shutdown};

use crate::error::Error;

/// Stackful plaintext or TLS transport over a connected socket.
pub enum StackTransport {
    Plaintext {
        fd: OwnedFd,
        deadline: Option<Instant>,
    },
    #[cfg(feature = "tls")]
    Tls {
        stream: TlsStream,
        deadline: Option<Instant>,
    },
}

impl StackTransport {
    pub fn plaintext(fd: OwnedFd) -> Self {
        Self::Plaintext { fd, deadline: None }
    }

    #[cfg(feature = "tls")]
    pub fn tls(stream: TlsStream) -> Self {
        Self::Tls {
            stream,
            deadline: None,
        }
    }

    /// Sets a relative read/write deadline budget and returns the previous deadline.
    pub fn set_io_timeout(&mut self, timeout: Option<Duration>) -> Option<Instant> {
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        self.set_io_deadline(deadline)
    }

    /// Sets an absolute read/write deadline and returns the previous deadline.
    pub fn set_io_deadline(&mut self, deadline: Option<Instant>) -> Option<Instant> {
        match self {
            Self::Plaintext {
                deadline: current, ..
            } => std::mem::replace(current, deadline),
            #[cfg(feature = "tls")]
            Self::Tls {
                deadline: current, ..
            } => std::mem::replace(current, deadline),
        }
    }

    pub fn read(&mut self, cx: &RuntimeContext<'_>, buf: &mut [u8]) -> Result<usize, Error> {
        match self {
            Self::Plaintext { fd, deadline } => match remaining_io_timeout(*deadline)? {
                Some(timeout) => read_with_timeout(cx, fd, buf, timeout),
                None => cx.read(fd, buf).map_err(Error::io),
            },
            #[cfg(feature = "tls")]
            Self::Tls { stream, deadline } => {
                let _ = remaining_io_timeout(*deadline)?;
                stream.read(cx, buf).map_err(Error::tls)
            }
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
            Self::Plaintext { fd, deadline } => match remaining_io_timeout(*deadline)? {
                Some(timeout) => write_with_timeout(cx, fd, buf, timeout),
                None => cx.write(fd, buf).map_err(Error::io),
            },
            #[cfg(feature = "tls")]
            Self::Tls { stream, deadline } => {
                let _ = remaining_io_timeout(*deadline)?;
                stream.write(cx, buf).map_err(Error::tls)
            }
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
            Self::Plaintext { fd, .. } => cx.shutdown(fd, Shutdown::Write).map_err(Error::io),
            #[cfg(feature = "tls")]
            Self::Tls { stream, .. } => stream.shutdown_write(cx).map_err(Error::tls),
        }
    }

    pub fn shutdown(&mut self, cx: &RuntimeContext<'_>) -> Result<(), Error> {
        match self {
            Self::Plaintext { fd, .. } => cx.shutdown(fd, Shutdown::Both).map_err(Error::io),
            #[cfg(feature = "tls")]
            Self::Tls { stream, .. } => stream.shutdown(cx).map_err(Error::tls),
        }
    }

    pub fn close(self, cx: &RuntimeContext<'_>) -> Result<(), Error> {
        match self {
            Self::Plaintext { fd, .. } => cx.close(fd).map_err(Error::io),
            #[cfg(feature = "tls")]
            Self::Tls { stream, .. } => stream.close(cx).map_err(Error::tls),
        }
    }
}

fn remaining_io_timeout(deadline: Option<Instant>) -> Result<Option<Duration>, Error> {
    let Some(deadline) = deadline else {
        return Ok(None);
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(Error::io(Errno::TIME))
    } else {
        Ok(Some(remaining))
    }
}

fn read_with_timeout(
    cx: &RuntimeContext<'_>,
    fd: &OwnedFd,
    buf: &mut [u8],
    timeout: Duration,
) -> Result<usize, Error> {
    let pending = cx.read_async(fd, vec![0_u8; buf.len()]);
    let waitables: [&dyn Waitable; 1] = [&pending];
    match cx.wait_all(&waitables, Some(timeout)) {
        Ok(()) => {
            let output = pending.get(cx).map_err(Error::io)?;
            let amount = output.bytes;
            buf[..amount].copy_from_slice(&output.buffer[..amount]);
            Ok(amount)
        }
        Err(WaitError::TimedOut) => {
            cx.cancel_io(&pending).map_err(Error::io)?;
            let _ = pending.get(cx);
            Err(Error::io(Errno::TIME))
        }
        Err(WaitError::Empty) => unreachable!("read timeout wait has one waitable"),
    }
}

fn write_with_timeout(
    cx: &RuntimeContext<'_>,
    fd: &OwnedFd,
    buf: &[u8],
    timeout: Duration,
) -> Result<usize, Error> {
    let pending = cx.write_async(fd, Vec::from(buf));
    let waitables: [&dyn Waitable; 1] = [&pending];
    match cx.wait_all(&waitables, Some(timeout)) {
        Ok(()) => pending
            .get(cx)
            .map(|output| output.bytes)
            .map_err(Error::io),
        Err(WaitError::TimedOut) => {
            cx.cancel_io(&pending).map_err(Error::io)?;
            let _ = pending.get(cx);
            Err(Error::io(Errno::TIME))
        }
        Err(WaitError::Empty) => unreachable!("write timeout wait has one waitable"),
    }
}
