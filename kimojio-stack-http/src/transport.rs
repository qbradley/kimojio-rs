// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Stackful transport abstraction used by HTTP/1.1 and HTTP/2.
//!
//! [`RuntimeStackTransport`] owns either a plaintext connected socket or, with
//! the `tls` feature, a stack TLS stream. Deadlines are stored on the transport
//! and apply to subsequent read/write calls until replaced. [`StackTransport`]
//! is the stack-core compatibility alias.

use std::time::{Duration, Instant};

use kimojio_stack::{
    Errno, IoFd, ReadOutput, RuntimeIoError, RuntimeReadResult, RuntimeSocket, RuntimeWaitError,
    RuntimeWaitable, RuntimeWriteResult, SocketIoRuntime, StackfulWaitContext, WriteOutput,
};
#[cfg(feature = "tls")]
use kimojio_stack_tls::{RuntimeTlsStream, TlsStream};
use rustix::{fd::OwnedFd, net::Shutdown};

use crate::error::Error;

/// Runtime capability bound used by HTTP transports with optional deadlines.
pub trait HttpRuntime<S>: SocketIoRuntime<Socket = S> + StackfulWaitContext
where
    S: RuntimeSocket,
    Self::ReadResult<Vec<u8>>: RuntimeReadResult<Vec<u8>, Output = ReadOutput<Vec<u8>>>,
    Self::WriteResult<Vec<u8>>: RuntimeWriteResult<Vec<u8>, Output = WriteOutput<Vec<u8>>>,
{
    /// Reads with an HTTP transport deadline.
    fn http_read_with_timeout(
        &self,
        fd: &S,
        buf: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, Error>;

    /// Writes with an HTTP transport deadline.
    fn http_write_with_timeout(
        &self,
        fd: &S,
        buf: &[u8],
        timeout: Duration,
    ) -> Result<usize, Error>;
}

impl<R, S> HttpRuntime<S> for R
where
    S: RuntimeSocket,
    R: SocketIoRuntime<Socket = S> + StackfulWaitContext,
    R::ReadResult<Vec<u8>>: RuntimeReadResult<Vec<u8>, Output = ReadOutput<Vec<u8>>>,
    R::WriteResult<Vec<u8>>: RuntimeWriteResult<Vec<u8>, Output = WriteOutput<Vec<u8>>>,
{
    fn http_read_with_timeout(
        &self,
        fd: &S,
        buf: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, Error> {
        read_with_timeout(self, fd, buf, timeout)
    }

    fn http_write_with_timeout(
        &self,
        fd: &S,
        buf: &[u8],
        timeout: Duration,
    ) -> Result<usize, Error> {
        write_with_timeout(self, fd, buf, timeout)
    }
}

/// Stackful plaintext or TLS transport over a connected runtime socket.
pub enum RuntimeStackTransport<S> {
    /// Plain connected socket.
    Plaintext {
        /// Runtime socket handle.
        fd: S,
        /// Optional absolute I/O deadline.
        deadline: Option<Instant>,
    },
    /// Stack TLS stream.
    #[cfg(feature = "tls")]
    Tls {
        /// TLS stream.
        stream: RuntimeTlsStream<S>,
        /// Optional absolute I/O deadline.
        deadline: Option<Instant>,
    },
}

/// Stack-core HTTP transport compatibility alias.
pub type StackTransport = RuntimeStackTransport<IoFd>;

impl<S> RuntimeStackTransport<S> {
    /// Creates a plaintext transport from an existing runtime socket handle.
    pub fn plaintext_socket(fd: S) -> Self {
        Self::Plaintext { fd, deadline: None }
    }

    /// Creates a TLS transport from an established runtime TLS stream.
    #[cfg(feature = "tls")]
    pub fn tls_stream(stream: RuntimeTlsStream<S>) -> Self {
        Self::Tls {
            stream,
            deadline: None,
        }
    }

    /// Sets a relative read/write deadline budget and returns the previous deadline.
    pub fn set_io_timeout(&mut self, timeout: Option<Duration>) -> Option<Instant> {
        let deadline = timeout.map(saturating_deadline_after);
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

    /// Reads bytes into `buf`.
    ///
    /// The method may park the current stackful coroutine while the underlying
    /// socket or TLS stream waits for input.
    pub fn read<R>(&mut self, cx: &R, buf: &mut [u8]) -> Result<usize, Error>
    where
        R: HttpRuntime<S>,
        S: RuntimeSocket,
    {
        match self {
            Self::Plaintext { fd, deadline } => match remaining_io_timeout(*deadline)? {
                Some(timeout) => cx.http_read_with_timeout(fd, buf, timeout),
                None => cx.read(fd, buf).map_err(runtime_io_error),
            },
            #[cfg(feature = "tls")]
            Self::Tls { stream, deadline } => match remaining_io_timeout(*deadline)? {
                Some(timeout) => tls_read_with_timeout(cx, stream, buf, timeout),
                None => stream.read(cx, buf).map_err(Error::tls),
            },
        }
    }

    /// Fills `buf` unless EOF is reached first.
    ///
    /// Returns the number of bytes copied into `buf`.
    pub fn read_exact_or_eof(
        &mut self,
        cx: &impl HttpRuntime<S>,
        mut buf: &mut [u8],
    ) -> Result<usize, Error>
    where
        S: RuntimeSocket,
    {
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

    /// Writes bytes from `buf`, returning the number accepted by the transport.
    pub fn write<R>(&mut self, cx: &R, buf: &[u8]) -> Result<usize, Error>
    where
        R: HttpRuntime<S>,
        S: RuntimeSocket,
    {
        match self {
            Self::Plaintext { fd, deadline } => match remaining_io_timeout(*deadline)? {
                Some(timeout) => cx.http_write_with_timeout(fd, buf, timeout),
                None => cx.write(fd, buf).map_err(runtime_io_error),
            },
            #[cfg(feature = "tls")]
            Self::Tls { stream, deadline } => match remaining_io_timeout(*deadline)? {
                Some(timeout) => tls_write_with_timeout(cx, stream, buf, timeout),
                None => stream.write(cx, buf).map_err(Error::tls),
            },
        }
    }

    /// Writes all bytes from `buf` or returns an error.
    pub fn write_all<R>(&mut self, cx: &R, mut buf: &[u8]) -> Result<(), Error>
    where
        R: HttpRuntime<S>,
        S: RuntimeSocket,
    {
        while !buf.is_empty() {
            let amount = self.write(cx, buf)?;
            if amount == 0 {
                return Err(Error::io(Errno::PIPE));
            }
            buf = &buf[amount..];
        }
        Ok(())
    }

    /// Half-closes the write side of the transport.
    pub fn shutdown_write<R>(&mut self, cx: &R) -> Result<(), Error>
    where
        R: SocketIoRuntime<Socket = S>,
        S: RuntimeSocket,
    {
        match self {
            Self::Plaintext { fd, .. } => {
                cx.shutdown(fd, Shutdown::Write).map_err(runtime_io_error)
            }
            #[cfg(feature = "tls")]
            Self::Tls { stream, .. } => stream.shutdown_write(cx).map_err(Error::tls),
        }
    }

    /// Shuts down both directions where supported.
    pub fn shutdown<R>(&mut self, cx: &R) -> Result<(), Error>
    where
        R: HttpRuntime<S>,
        S: RuntimeSocket,
    {
        match self {
            Self::Plaintext { fd, .. } => cx.shutdown(fd, Shutdown::Both).map_err(runtime_io_error),
            #[cfg(feature = "tls")]
            Self::Tls { stream, .. } => stream.shutdown(cx).map_err(Error::tls),
        }
    }

    /// Closes the transport through the stack runtime.
    pub fn close<R>(self, cx: &R) -> Result<(), Error>
    where
        R: HttpRuntime<S>,
        S: RuntimeSocket,
    {
        match self {
            Self::Plaintext { fd, .. } => cx.close(fd).map_err(runtime_io_error),
            #[cfg(feature = "tls")]
            Self::Tls { stream, .. } => stream.close(cx).map_err(Error::tls),
        }
    }
}

impl RuntimeStackTransport<IoFd> {
    /// Creates a stack-core plaintext transport from a connected socket.
    pub fn plaintext(fd: OwnedFd) -> Self {
        Self::plaintext_socket(IoFd::from_owned(fd))
    }

    /// Creates a stack-core TLS transport from an established stack TLS stream.
    #[cfg(feature = "tls")]
    pub fn tls(stream: TlsStream) -> Self {
        Self::tls_stream(stream)
    }
}

fn saturating_deadline_after(timeout: Duration) -> Instant {
    let now = Instant::now();
    let mut timeout = timeout;
    loop {
        if let Some(deadline) = now.checked_add(timeout) {
            return deadline;
        }

        timeout = halve_duration(timeout);
        if timeout.is_zero() {
            return now;
        }
    }
}

fn halve_duration(duration: Duration) -> Duration {
    let extra_nanos = if duration.as_secs().is_multiple_of(2) {
        0
    } else {
        500_000_000
    };
    Duration::new(
        duration.as_secs() / 2,
        duration.subsec_nanos() / 2 + extra_nanos,
    )
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

fn read_with_timeout<R, S>(
    cx: &R,
    fd: &S,
    buf: &mut [u8],
    timeout: Duration,
) -> Result<usize, Error>
where
    R: SocketIoRuntime<Socket = S> + StackfulWaitContext,
    S: RuntimeSocket,
    R::ReadResult<Vec<u8>>: RuntimeReadResult<Vec<u8>, Output = ReadOutput<Vec<u8>>>,
{
    let timer = cx.sleep_async(timeout).map_err(runtime_io_error)?;
    let mut pending = cx
        .read_async(fd, vec![0_u8; buf.len()])
        .map_err(runtime_io_error)?;
    loop {
        if let Some(result) = RuntimeReadResult::try_get(&mut pending) {
            let output = result.map_err(runtime_io_error)?;
            let amount = output.bytes;
            buf[..amount].copy_from_slice(&output.buffer[..amount]);
            return Ok(amount);
        }
        let waitables: [&dyn RuntimeWaitable; 2] = [&pending, &timer];
        if cx
            .wait_any_stackful(&waitables)
            .map_err(runtime_wait_error)?
            == 1
        {
            cancel_and_drain_read(cx, &mut pending)?;
            return Err(Error::io(Errno::TIME));
        }
    }
}

fn write_with_timeout<R, S>(cx: &R, fd: &S, buf: &[u8], timeout: Duration) -> Result<usize, Error>
where
    R: SocketIoRuntime<Socket = S> + StackfulWaitContext,
    S: RuntimeSocket,
    R::WriteResult<Vec<u8>>: RuntimeWriteResult<Vec<u8>, Output = WriteOutput<Vec<u8>>>,
{
    let timer = cx.sleep_async(timeout).map_err(runtime_io_error)?;
    let mut pending = cx
        .write_async(fd, Vec::from(buf))
        .map_err(runtime_io_error)?;
    loop {
        if let Some(result) = RuntimeWriteResult::try_get(&mut pending) {
            return result.map(|output| output.bytes).map_err(runtime_io_error);
        }
        let waitables: [&dyn RuntimeWaitable; 2] = [&pending, &timer];
        if cx
            .wait_any_stackful(&waitables)
            .map_err(runtime_wait_error)?
            == 1
        {
            cancel_and_drain_write(cx, &mut pending)?;
            return Err(Error::io(Errno::TIME));
        }
    }
}

fn cancel_and_drain_read<R, S>(cx: &R, pending: &mut R::ReadResult<Vec<u8>>) -> Result<(), Error>
where
    R: SocketIoRuntime<Socket = S> + StackfulWaitContext,
    S: RuntimeSocket,
    R::ReadResult<Vec<u8>>: RuntimeReadResult<Vec<u8>, Output = ReadOutput<Vec<u8>>>,
{
    cx.cancel_read(pending).map_err(runtime_io_error)?;
    cx.wait_stackful(pending).map_err(runtime_wait_error)?;
    let _ = RuntimeReadResult::try_get(pending);
    Ok(())
}

fn cancel_and_drain_write<R, S>(cx: &R, pending: &mut R::WriteResult<Vec<u8>>) -> Result<(), Error>
where
    R: SocketIoRuntime<Socket = S> + StackfulWaitContext,
    S: RuntimeSocket,
    R::WriteResult<Vec<u8>>: RuntimeWriteResult<Vec<u8>, Output = WriteOutput<Vec<u8>>>,
{
    cx.cancel_write(pending).map_err(runtime_io_error)?;
    cx.wait_stackful(pending).map_err(runtime_wait_error)?;
    let _ = RuntimeWriteResult::try_get(pending);
    Ok(())
}

#[cfg(feature = "tls")]
fn tls_read_with_timeout<R, S>(
    cx: &R,
    stream: &mut RuntimeTlsStream<S>,
    buf: &mut [u8],
    timeout: Duration,
) -> Result<usize, Error>
where
    R: HttpRuntime<S>,
    S: RuntimeSocket,
    R::ReadResult<Vec<u8>>: RuntimeReadResult<Vec<u8>, Output = ReadOutput<Vec<u8>>>,
    R::WriteResult<Vec<u8>>: RuntimeWriteResult<Vec<u8>, Output = WriteOutput<Vec<u8>>>,
{
    let timer = cx.sleep_async(timeout).map_err(runtime_io_error)?;
    let mut pending = stream.read_async(cx, vec![0_u8; buf.len()]);
    loop {
        if let Some(result) = pending.try_get() {
            let output = result.map_err(Error::tls)?;
            let amount = output.bytes;
            buf[..amount].copy_from_slice(&output.buffer[..amount]);
            return Ok(amount);
        }
        let waitables: [&dyn RuntimeWaitable; 2] = [&pending, &timer];
        if cx
            .wait_any_stackful(&waitables)
            .map_err(runtime_wait_error)?
            == 1
        {
            if let Some(result) = pending.try_get() {
                let output = result.map_err(Error::tls)?;
                let amount = output.bytes;
                buf[..amount].copy_from_slice(&output.buffer[..amount]);
                return Ok(amount);
            } else {
                pending.cancel().map_err(Error::tls)?;
                return Err(Error::io(Errno::TIME));
            }
        }
    }
}

#[cfg(feature = "tls")]
fn tls_write_with_timeout<R, S>(
    cx: &R,
    stream: &mut RuntimeTlsStream<S>,
    buf: &[u8],
    timeout: Duration,
) -> Result<usize, Error>
where
    R: HttpRuntime<S>,
    S: RuntimeSocket,
    R::ReadResult<Vec<u8>>: RuntimeReadResult<Vec<u8>, Output = ReadOutput<Vec<u8>>>,
    R::WriteResult<Vec<u8>>: RuntimeWriteResult<Vec<u8>, Output = WriteOutput<Vec<u8>>>,
{
    let timer = cx.sleep_async(timeout).map_err(runtime_io_error)?;
    let mut pending = stream.write_async(cx, Vec::from(buf));
    loop {
        if let Some(result) = pending.try_get() {
            return result.map(|output| output.bytes).map_err(Error::tls);
        }
        let waitables: [&dyn RuntimeWaitable; 2] = [&pending, &timer];
        if cx
            .wait_any_stackful(&waitables)
            .map_err(runtime_wait_error)?
            == 1
        {
            if let Some(result) = pending.try_get() {
                return result.map(|output| output.bytes).map_err(Error::tls);
            } else {
                pending.cancel().map_err(Error::tls)?;
                return Err(Error::io(Errno::TIME));
            }
        }
    }
}

fn runtime_io_error(error: RuntimeIoError) -> Error {
    match error {
        RuntimeIoError::Io(errno) => Error::io(errno),
        RuntimeIoError::Unsupported(_) => Error::Unsupported("runtime does not support socket I/O"),
        RuntimeIoError::Other(message) => Error::Protocol(message),
    }
}

fn runtime_wait_error(error: RuntimeWaitError) -> Error {
    match error {
        RuntimeWaitError::Unsupported(_) => {
            Error::Unsupported("runtime does not support stackful waiting")
        }
        RuntimeWaitError::Empty => Error::Protocol("deadline wait requires at least one waitable"),
        RuntimeWaitError::NoStackfulContext { .. } => {
            Error::Protocol("deadline wait requires a stackful coroutine context")
        }
    }
}
