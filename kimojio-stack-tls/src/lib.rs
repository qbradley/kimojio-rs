// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! OpenSSL TLS streams for `kimojio-stack`.
//!
//! This crate adapts the existing Kimojio OpenSSL memory-BIO integration to the
//! stackful runtime. OpenSSL reads encrypted bytes directly from an internal BIO
//! buffer returned by `kimojio-tls`; the runtime fills that buffer through
//! `SocketIoRuntime::read`. OpenSSL writes encrypted bytes into another BIO
//! buffer, and the runtime drains it with `SocketIoRuntime::write`. That keeps
//! the OS-thread nonblocking without adding an intermediate copy between the
//! socket and OpenSSL's encrypted buffers.
//!
//! Use this crate when you already use OpenSSL for TLS policy/certificates and
//! need the resulting stream to park a stackful coroutine instead of blocking an
//! OS thread. The crate is intentionally transport-focused: certificate stores,
//! ALPN choices, client/server verification policy, and key material loading are
//! configured with the normal `openssl` crate before conversion.
//!
//! # Success path
//!
//! Create an OpenSSL client or server context in ordinary OpenSSL code, convert it
//! with [`TlsContext::from_openssl`], then perform the handshake from a stackful
//! coroutine. After handshake, [`TlsStream::read`] and [`TlsStream::write`]
//! exchange plaintext while the runtime drives encrypted socket I/O underneath.
//! [`TlsStream::read_async`] and [`TlsStream::write_async`] return handles that
//! implement `RuntimeWaitable` for runtime-neutral waiting and `Waitable` for
//! stack-core [`RuntimeContext::select`] / [`RuntimeContext::join`] compatibility.
//!
//! ```no_run
//! use kimojio_stack::{RuntimeContext, OwnedFd};
//! use kimojio_stack_tls::TlsContext;
//! use openssl::ssl::{SslConnector, SslMethod};
//!
//! # fn connected_socket() -> OwnedFd { unimplemented!() }
//! # fn example(cx: &RuntimeContext<'_>) -> Result<(), kimojio_stack::Errno> {
//! let connector = SslConnector::builder(SslMethod::tls())
//!     .expect("create TLS connector")
//!     .build();
//! let context = TlsContext::from_openssl(connector.into_context());
//! let socket = connected_socket();
//!
//! let mut stream = context.client(cx, 16 * 1024, socket, "example.com")?;
//! stream.write(cx, b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n")?;
//! let mut buf = [0_u8; 4096];
//! let amount = stream.read(cx, &mut buf)?;
//! # let _ = amount;
//! stream.shutdown(cx)?;
//! stream.close(cx)?;
//! # Ok(())
//! # }
//! ```
//!
//! # Caveats
//!
//! This crate is stack-runtime oriented rather than `std::io::Read`/`Write`
//! oriented. All I/O methods require a [`RuntimeContext`] and may park the
//! current coroutine. Stack-core compatibility methods take a [`RuntimeContext`];
//! runtime-agnostic constructors such as [`TlsContext::client_with_runtime`] and
//! [`TlsContext::server_with_runtime`] take any [`SocketIoRuntime`] adapter, and
//! async TLS handles can wait through any adapter that also implements
//! `StackfulWaitContext`. Canceling a pending async TLS operation drains the
//! private socket I/O through that adapter and poisons the stream, so callers
//! should close rather than reuse a stream after an active timeout.
//! `TlsContext::from_openssl` takes ownership of the OpenSSL context; do not keep
//! using the original `SslContext` value after conversion. Complete all OpenSSL
//! context configuration before conversion; after conversion, `TlsContext` is
//! immutable and shareable across stack-steal worker tasks for creating
//! independent per-connection TLS streams.

use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    fmt, mem, slice,
};

use foreign_types_shared::{ForeignType, ForeignTypeRef};
use kimojio_stack::{
    Errno, IoFd, IoReadBuffer, IoWriteBuffer, ReadOutput, RuntimeContext, RuntimeIoError,
    RuntimeReadResult, RuntimeWaitError, RuntimeWaitable, RuntimeWaitableAdapter,
    RuntimeWriteResult, SocketIoRuntime, StackfulWaitContext, WaitRegistration, Waitable,
    WriteOutput,
};
use kimojio_tls::{Response, TlsServer, TlsServerContext, TlsServerError};
use rustix::{fd::OwnedFd, net::Shutdown};

/// A TLS context backed by an OpenSSL `SSL_CTX`.
///
/// Construct one with [`TlsContext::from_openssl`] using the Rust `openssl`
/// crate's `SslContext` values, such as those produced by `SslConnector` or
/// `SslAcceptor`.
pub struct TlsContext {
    ssl_ctx: TlsServerContext,
}

// SAFETY: `TlsContext` is a transparent policy wrapper around
// `kimojio_tls::TlsServerContext`. Shared access only creates independent
// per-connection TLS handles from the immutable OpenSSL `SSL_CTX`; it does not
// mutate connection state in the context itself.
unsafe impl Sync for TlsContext {}

impl TlsContext {
    /// Creates a stack TLS context from an OpenSSL crate context.
    ///
    /// Ownership of the OpenSSL context is transferred into this type. Use a
    /// client context for [`client`](Self::client) and a server/acceptor context
    /// for [`server`](Self::server).
    pub fn from_openssl(ctx: openssl::ssl::SslContext) -> Self {
        let ssl_ctx = TlsServerContext::from_raw(ctx.as_ptr() as *mut c_void);
        mem::forget(ctx);
        Self { ssl_ctx }
    }

    /// Performs a server-side TLS handshake over `socket` on the stack-core runtime.
    ///
    /// The handshake runs synchronously from the caller's perspective but parks
    /// the current coroutine whenever OpenSSL needs socket input or output. The
    /// returned [`TlsStream`] owns the connected socket.
    pub fn server(
        &self,
        cx: &RuntimeContext<'_>,
        bufsize: usize,
        socket: OwnedFd,
    ) -> Result<TlsStream, Errno> {
        self.server_with_runtime(cx, bufsize, socket)
    }

    /// Performs a server-side TLS handshake over `socket` through a runtime adapter.
    pub fn server_with_runtime<R>(
        &self,
        cx: &R,
        bufsize: usize,
        socket: OwnedFd,
    ) -> Result<RuntimeTlsStream<R::Socket>, Errno>
    where
        R: SocketIoRuntime,
    {
        let ssl = self.ssl_ctx.server(bufsize).map_err(as_io_error)?;
        let socket = cx
            .socket_from_owned_fd(socket)
            .map_err(runtime_io_error_as_errno)?;
        let mut stream = RuntimeTlsStream::from_socket(ssl, socket);
        stream.server_side_handshake(cx)?;
        Ok(stream)
    }

    /// Performs a client-side TLS handshake over `socket` on the stack-core runtime.
    ///
    /// `server_name` is installed as the OpenSSL hostname before the handshake so
    /// normal SNI and certificate-name verification policies can apply according
    /// to the supplied OpenSSL context.
    pub fn client(
        &self,
        cx: &RuntimeContext<'_>,
        bufsize: usize,
        socket: OwnedFd,
        server_name: &str,
    ) -> Result<TlsStream, Errno> {
        self.client_with_runtime(cx, bufsize, socket, server_name)
    }

    /// Performs a client-side TLS handshake over `socket` through a runtime adapter.
    pub fn client_with_runtime<R>(
        &self,
        cx: &R,
        bufsize: usize,
        socket: OwnedFd,
        server_name: &str,
    ) -> Result<RuntimeTlsStream<R::Socket>, Errno>
    where
        R: SocketIoRuntime,
    {
        let ssl = self.ssl_ctx.client(bufsize).map_err(as_io_error)?;
        let socket = cx
            .socket_from_owned_fd(socket)
            .map_err(runtime_io_error_as_errno)?;
        let mut stream = RuntimeTlsStream::from_socket(ssl, socket);
        stream
            .ssl_mut()
            .set_hostname(server_name)
            .map_err(|_| Errno::INVAL)?;
        stream.client_side_handshake(cx)?;
        Ok(stream)
    }
}

/// A TLS stream driven through a stack runtime socket-I/O adapter.
///
/// The stream owns one connected socket until [`close`](Self::close) consumes it.
/// `shutdown_write` only half-closes the socket write side; use
/// [`shutdown`](Self::shutdown) first when TLS close-notify matters to the peer.
pub struct RuntimeTlsStream<S> {
    ssl: TlsServer,
    socket: Option<S>,
    poisoned: bool,
}

/// Stack-core TLS stream compatibility alias.
pub type TlsStream = RuntimeTlsStream<IoFd>;

impl<S> RuntimeTlsStream<S> {
    /// Creates a TLS stream from an initialized TLS handle and runtime socket.
    pub fn from_socket(ssl: TlsServer, socket: S) -> Self {
        Self {
            ssl,
            socket: Some(socket),
            poisoned: false,
        }
    }

    /// Gets the OpenSSL `SslRef` for inspection.
    pub fn ssl(&self) -> &openssl::ssl::SslRef {
        let raw_ssl = self.ssl.get_ssl_raw();
        // SAFETY: `raw_ssl` is owned by `self.ssl` and remains valid for `self`.
        unsafe { openssl::ssl::SslRef::from_ptr(raw_ssl as *mut _) }
    }

    /// Gets the mutable OpenSSL `SslRef` for pre-handshake configuration.
    ///
    /// Use this before calling a manual handshake method when you need to set
    /// OpenSSL options that are not represented by [`TlsContext`].
    pub fn ssl_mut(&mut self) -> &mut openssl::ssl::SslRef {
        let raw_ssl = self.ssl.get_ssl_raw();
        // SAFETY: `raw_ssl` is uniquely borrowed through `&mut self`.
        unsafe { openssl::ssl::SslRef::from_ptr_mut(raw_ssl as *mut _) }
    }

    /// Performs the client side of the TLS handshake.
    ///
    /// This is useful when constructing a stream manually with [`TlsStream::new`].
    /// `TlsContext::client` calls it automatically.
    pub fn client_side_handshake<R>(&mut self, cx: &R) -> Result<(), Errno>
    where
        R: SocketIoRuntime<Socket = S>,
    {
        self.ensure_usable()?;
        loop {
            match self.ssl.client_side_handshake() {
                Response::Success(_) => return self.flush_tls_write(cx),
                Response::Fail(e) => return Err(self.handle_tls_error(e)),
                Response::WantRead => self.fill_tls_read(cx)?,
                Response::WantWrite => self.flush_tls_write(cx)?,
                Response::Eof => return Err(Errno::PROTO),
            }
        }
    }

    /// Performs the server side of the TLS handshake.
    ///
    /// This is useful when constructing a stream manually with [`TlsStream::new`].
    /// `TlsContext::server` calls it automatically.
    pub fn server_side_handshake<R>(&mut self, cx: &R) -> Result<(), Errno>
    where
        R: SocketIoRuntime<Socket = S>,
    {
        self.ensure_usable()?;
        loop {
            match self.ssl.server_side_handshake() {
                Response::Success(_) => return self.flush_tls_write(cx),
                Response::Fail(e) => return Err(self.handle_tls_error(e)),
                Response::WantRead => self.fill_tls_read(cx)?,
                Response::WantWrite => self.flush_tls_write(cx)?,
                Response::Eof => return Err(Errno::PROTO),
            }
        }
    }

    /// Reads decrypted TLS plaintext into `buf`.
    ///
    /// Returns `Ok(0)` after a clean TLS EOF.
    pub fn read<R>(&mut self, cx: &R, buf: &mut [u8]) -> Result<usize, Errno>
    where
        R: SocketIoRuntime<Socket = S>,
    {
        self.ensure_usable()?;
        loop {
            match self.ssl.read(buf) {
                Response::Success(amount) => return Ok(amount),
                Response::Fail(e) => return Err(self.handle_tls_error(e)),
                Response::WantRead => self.fill_tls_read(cx)?,
                Response::WantWrite => self.flush_tls_write(cx)?,
                Response::Eof => return Ok(0),
            }
        }
    }

    /// Fills `buf` with decrypted TLS plaintext unless EOF is reached first.
    ///
    /// The return value is the number of bytes copied into `buf`. It may be less
    /// than `buf.len()` when the peer closes cleanly.
    pub fn read_exact_or_eof(
        &mut self,
        cx: &impl SocketIoRuntime<Socket = S>,
        mut buf: &mut [u8],
    ) -> Result<usize, Errno> {
        self.ensure_usable()?;
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

    /// Writes all plaintext from `buf` and flushes encrypted bytes to the socket.
    ///
    /// The method loops until every plaintext byte has been accepted by OpenSSL
    /// and all produced encrypted bytes have been written through the stack
    /// runtime. It returns the original plaintext length on success.
    pub fn write<R>(&mut self, cx: &R, mut buf: &[u8]) -> Result<usize, Errno>
    where
        R: SocketIoRuntime<Socket = S>,
    {
        self.ensure_usable()?;
        let requested = buf.len();
        while !buf.is_empty() {
            match self.ssl.write(buf) {
                Response::Success(amount) => {
                    if amount == 0 {
                        return Err(Errno::PIPE);
                    }
                    buf = &buf[amount..];
                }
                Response::Fail(e) => return Err(self.handle_tls_error(e)),
                Response::WantRead => self.fill_tls_read(cx)?,
                Response::WantWrite => self.flush_tls_write(cx)?,
                Response::Eof => return Err(Errno::PIPE),
            }
        }
        self.flush_tls_write(cx)?;
        Ok(requested)
    }

    /// Sends TLS close-notify if possible.
    ///
    /// A peer that has already closed the transport is treated as a successful
    /// shutdown. Transport or protocol errors before close-notify completes are
    /// returned as [`Errno`].
    pub fn shutdown<R>(&mut self, cx: &R) -> Result<(), Errno>
    where
        R: SocketIoRuntime<Socket = S>,
    {
        self.ensure_usable()?;
        loop {
            match self.ssl.shutdown() {
                Response::Success(_) | Response::Eof => return Ok(()),
                Response::Fail(e) => return Err(self.handle_tls_error(e)),
                Response::WantRead => {
                    if let Err(Errno::PIPE) = self.fill_tls_read(cx) {
                        return Ok(());
                    }
                }
                Response::WantWrite => self.flush_tls_write(cx)?,
            }
        }
    }

    /// Shuts down the underlying socket write half.
    pub fn shutdown_write<R>(&self, cx: &R) -> Result<(), Errno>
    where
        R: SocketIoRuntime<Socket = S>,
    {
        self.ensure_usable()?;
        let socket = self.socket.as_ref().ok_or(Errno::PIPE)?;
        cx.shutdown(socket, Shutdown::Write)
            .map_err(runtime_io_error_as_errno)
    }

    /// Closes the underlying socket through the stack runtime.
    pub fn close<R>(mut self, cx: &R) -> Result<(), Errno>
    where
        R: SocketIoRuntime<Socket = S>,
    {
        let socket = self.socket.take().ok_or(Errno::PIPE)?;
        cx.close(socket).map_err(runtime_io_error_as_errno)
    }

    fn fill_tls_read<R>(&mut self, cx: &R) -> Result<(), Errno>
    where
        R: SocketIoRuntime<Socket = S>,
    {
        self.ensure_usable()?;
        let socket = self.socket.as_ref().ok_or(Errno::PIPE)?;
        if let Some(buffer) = self.ssl.get_push_buffer() {
            let amount = cx.read(socket, buffer).map_err(runtime_io_error_as_errno)?;
            if amount == 0 {
                return Err(Errno::PIPE);
            }
            self.ssl.use_push_buffer(amount);
        }
        Ok(())
    }

    fn flush_tls_write<R>(&mut self, cx: &R) -> Result<(), Errno>
    where
        R: SocketIoRuntime<Socket = S>,
    {
        self.ensure_usable()?;
        let socket = self.socket.as_ref().ok_or(Errno::PIPE)?;
        loop {
            let amount = match self.ssl.get_pull_buffer() {
                Some(buffer) => {
                    let amount = cx
                        .write(socket, buffer)
                        .map_err(runtime_io_error_as_errno)?;
                    if amount == 0 {
                        return Err(Errno::PIPE);
                    }
                    amount
                }
                None => return Ok(()),
            };
            self.ssl.use_pull_buffer(amount);
        }
    }

    fn handle_tls_error(&self, error: TlsServerError) -> Errno {
        if self.socket.is_some() && !self.poisoned {
            as_io_error(error)
        } else {
            Errno::PIPE
        }
    }

    fn ensure_usable(&self) -> Result<(), Errno> {
        if self.poisoned {
            Err(Errno::PIPE)
        } else {
            Ok(())
        }
    }

    fn poison(&mut self) {
        self.poisoned = true;
    }
}

impl<S> RuntimeTlsStream<S> {
    /// Starts a TLS plaintext read into an owned buffer through a runtime adapter.
    ///
    /// The returned handle implements [`RuntimeWaitable`] for runtime-neutral
    /// waiting and [`Waitable`] for stack-core compatibility. It keeps an
    /// exclusive borrow of this stream until consumed or dropped.
    ///
    /// Dropping a pending TLS operation before it completes leaves the encrypted
    /// socket stream in an unknown protocol state. To prevent later duplicate or
    /// lost TLS records from being hidden, the stream is marked unusable and
    /// subsequent I/O returns [`Errno::PIPE`].
    pub fn read_async<'operation, R, B>(
        &'operation mut self,
        cx: &'operation R,
        buffer: B,
    ) -> TlsReadResult<'operation, B, R>
    where
        R: SocketIoRuntime<Socket = S> + StackfulWaitContext,
        R::ReadResult<Vec<u8>>: RuntimeReadResult<Vec<u8>, Output = ReadOutput<Vec<u8>>>,
        R::WriteResult<Vec<u8>>: RuntimeWriteResult<Vec<u8>, Output = WriteOutput<Vec<u8>>>,
        B: IoReadBuffer,
    {
        let result = TlsReadResult::new(cx, self, buffer);
        result.progress();
        result
    }

    /// Starts a TLS plaintext write from an owned buffer through a runtime adapter.
    ///
    /// The returned handle implements [`RuntimeWaitable`] for runtime-neutral
    /// waiting and [`Waitable`] for stack-core compatibility. It keeps an
    /// exclusive borrow of this stream until consumed or dropped.
    ///
    /// Dropping a pending TLS operation before it completes marks the stream
    /// unusable for the same reason as [`read_async`](Self::read_async): the
    /// underlying socket may have consumed or emitted encrypted TLS records that
    /// OpenSSL has not accounted for.
    pub fn write_async<'operation, R, B>(
        &'operation mut self,
        cx: &'operation R,
        buffer: B,
    ) -> TlsWriteResult<'operation, B, R>
    where
        R: SocketIoRuntime<Socket = S> + StackfulWaitContext,
        R::ReadResult<Vec<u8>>: RuntimeReadResult<Vec<u8>, Output = ReadOutput<Vec<u8>>>,
        R::WriteResult<Vec<u8>>: RuntimeWriteResult<Vec<u8>, Output = WriteOutput<Vec<u8>>>,
        B: IoWriteBuffer,
    {
        let result = TlsWriteResult::new(cx, self, buffer);
        result.progress();
        result
    }
}

impl RuntimeTlsStream<IoFd> {
    /// Creates a stack-core TLS stream from an initialized TLS handle and connected socket.
    ///
    /// Most callers should prefer [`TlsContext::client`] or
    /// [`TlsContext::server`], which create the TLS handle and run the handshake.
    pub fn new(ssl: TlsServer, socket: OwnedFd) -> Self {
        Self::from_socket(ssl, IoFd::from_owned(socket))
    }
}

enum TlsPendingIo<R>
where
    R: SocketIoRuntime,
{
    Read(R::ReadResult<Vec<u8>>),
    Write(R::WriteResult<Vec<u8>>),
}

impl<R> TlsPendingIo<R>
where
    R: SocketIoRuntime + StackfulWaitContext,
    R::ReadResult<Vec<u8>>: RuntimeReadResult<Vec<u8>, Output = ReadOutput<Vec<u8>>>,
    R::WriteResult<Vec<u8>>: RuntimeWriteResult<Vec<u8>, Output = WriteOutput<Vec<u8>>>,
{
    fn add_stackful_waiter(&self, waiter: kimojio_stack::StackfulWaiterHandle) -> bool {
        match self {
            Self::Read(io) => RuntimeWaitable::add_stackful_waiter(io, waiter),
            Self::Write(io) => RuntimeWaitable::add_stackful_waiter(io, waiter),
        }
    }

    fn cancel(&mut self, cx: &R) -> Result<(), Errno> {
        match self {
            Self::Read(io) => {
                SocketIoRuntime::cancel_read(cx, io).map_err(runtime_io_error_as_errno)
            }
            Self::Write(io) => {
                SocketIoRuntime::cancel_write(cx, io).map_err(runtime_io_error_as_errno)
            }
        }
    }

    fn drain(self, cx: &R) -> Result<(), Errno> {
        match self {
            Self::Read(mut io) => drain_read_result(cx, &mut io),
            Self::Write(mut io) => drain_write_result(cx, &mut io),
        }
    }
}

/// A pending TLS plaintext read.
#[must_use = "pending TLS reads should be completed with get, try_get, or a runtime wait"]
pub struct TlsReadResult<'operation, B, R = RuntimeContext<'operation>>
where
    R: SocketIoRuntime + StackfulWaitContext,
{
    runtime: &'operation R,
    stream: RefCell<&'operation mut RuntimeTlsStream<R::Socket>>,
    buffer: RefCell<Option<B>>,
    encrypted_read_buffer: RefCell<Vec<u8>>,
    encrypted_write_buffer: RefCell<Vec<u8>>,
    pending: RefCell<Option<TlsPendingIo<R>>>,
    result: RefCell<Option<Result<usize, Errno>>>,
    taken: bool,
}

impl<'operation, B, R> TlsReadResult<'operation, B, R>
where
    R: SocketIoRuntime + StackfulWaitContext,
    R::ReadResult<Vec<u8>>: RuntimeReadResult<Vec<u8>, Output = ReadOutput<Vec<u8>>>,
    R::WriteResult<Vec<u8>>: RuntimeWriteResult<Vec<u8>, Output = WriteOutput<Vec<u8>>>,
    B: IoReadBuffer,
{
    fn new(
        runtime: &'operation R,
        stream: &'operation mut RuntimeTlsStream<R::Socket>,
        buffer: B,
    ) -> Self {
        Self {
            runtime,
            stream: RefCell::new(stream),
            buffer: RefCell::new(Some(buffer)),
            encrypted_read_buffer: RefCell::new(Vec::new()),
            encrypted_write_buffer: RefCell::new(Vec::new()),
            pending: RefCell::new(None),
            result: RefCell::new(None),
            taken: false,
        }
    }

    /// Returns the completed read if it is ready.
    ///
    /// This does not park the current coroutine. If the TLS operation needs more
    /// socket I/O that has not already been submitted, call [`get`](Self::get) or
    /// wait on the handle through [`RuntimeContext::select`] or
    /// [`RuntimeContext::join`].
    pub fn try_get(&mut self) -> Option<Result<ReadOutput<B>, Errno>> {
        assert!(!self.taken, "TlsReadResult value already taken");
        self.progress();
        let result = self.result.borrow_mut().take()?;
        self.taken = true;
        let buffer = self
            .buffer
            .borrow_mut()
            .take()
            .expect("TlsReadResult buffer missing");

        Some(result.map(|bytes| ReadOutput { bytes, buffer }))
    }

    /// Waits for the TLS read to complete.
    pub fn get(mut self, cx: &RuntimeContext<'_>) -> Result<ReadOutput<B>, Errno> {
        loop {
            if let Some(result) = self.try_get() {
                return result;
            }

            let waitables: [&dyn Waitable; 1] = [&self];
            cx.wait_all(&waitables, None)
                .expect("single TLS read waitable should be waitable");
        }
    }

    /// Attempts to cancel this TLS read using the runtime captured by
    /// [`RuntimeTlsStream::read_async`].
    ///
    /// Cancellation drains the private socket I/O used by the TLS state machine,
    /// then marks the TLS stream unusable. A canceled TLS record may already have
    /// consumed encrypted bytes from the socket, so continuing the same stream
    /// would risk duplicate or lost protocol data. After successful cancellation,
    /// [`try_get`](Self::try_get) returns `Err(Errno::CANCELED)`.
    pub fn cancel(&self) -> Result<(), Errno> {
        if self.taken || self.result.borrow().is_some() {
            return Ok(());
        }

        {
            let mut pending = self.pending.borrow_mut();
            if let Some(pending) = pending.as_mut() {
                pending.cancel(self.runtime)?;
            }
        }

        if let Some(pending) = self.pending.borrow_mut().take() {
            pending.drain(self.runtime)?;
        }
        self.stream.borrow_mut().poison();
        self.complete(Err(Errno::CANCELED));
        Ok(())
    }

    fn progress(&self) {
        if self.result.borrow().is_some() {
            return;
        }

        loop {
            if !self.finish_pending() || self.result.borrow().is_some() {
                return;
            }

            let response = {
                let mut stream = self.stream.borrow_mut();
                if let Err(error) = stream.ensure_usable() {
                    self.complete(Err(error));
                    return;
                }
                let mut buffer = self.buffer.borrow_mut();
                let buffer = buffer.as_mut().expect("TlsReadResult buffer missing");
                let plaintext = plaintext_read_slice(buffer);
                stream.ssl.read(plaintext)
            };

            match response {
                Response::Success(amount) => {
                    self.complete(Ok(amount));
                    return;
                }
                Response::Fail(error) => {
                    let error = self.stream.borrow().handle_tls_error(error);
                    self.complete(Err(error));
                    return;
                }
                Response::WantRead => match self.start_encrypted_read() {
                    Ok(true) => return,
                    Ok(false) => {}
                    Err(error) => {
                        self.complete(Err(error));
                        return;
                    }
                },
                Response::WantWrite => match self.start_encrypted_write() {
                    Ok(true) => return,
                    Ok(false) => {}
                    Err(error) => {
                        self.complete(Err(error));
                        return;
                    }
                },
                Response::Eof => {
                    self.complete(Ok(0));
                    return;
                }
            }
        }
    }

    fn finish_pending(&self) -> bool {
        let completion = {
            let mut pending = self.pending.borrow_mut();
            match pending.as_mut() {
                Some(TlsPendingIo::Read(read)) => match RuntimeReadResult::try_get(read) {
                    Some(result) => {
                        *pending = None;
                        Some(PendingCompletion::Read(
                            result.map_err(runtime_io_error_as_errno),
                        ))
                    }
                    None => return false,
                },
                Some(TlsPendingIo::Write(write)) => match RuntimeWriteResult::try_get(write) {
                    Some(result) => {
                        *pending = None;
                        Some(PendingCompletion::Write(
                            result.map_err(runtime_io_error_as_errno),
                        ))
                    }
                    None => return false,
                },
                None => None,
            }
        };

        match completion {
            Some(PendingCompletion::Read(Ok(output))) => {
                let bytes = output.bytes;
                let buffer = output.buffer;
                if bytes == 0 {
                    *self.encrypted_read_buffer.borrow_mut() = buffer;
                    self.complete(Err(Errno::PIPE));
                    return true;
                }

                {
                    let mut stream = self.stream.borrow_mut();
                    let Some(push_buffer) = stream.ssl.get_push_buffer() else {
                        self.complete(Err(Errno::PROTO));
                        return true;
                    };
                    if bytes > push_buffer.len() || bytes > buffer.len() {
                        self.complete(Err(Errno::PROTO));
                        return true;
                    }
                    push_buffer[..bytes].copy_from_slice(&buffer[..bytes]);
                    stream.ssl.use_push_buffer(bytes);
                }

                *self.encrypted_read_buffer.borrow_mut() = buffer;
                true
            }
            Some(PendingCompletion::Read(Err(error))) => {
                self.complete(Err(error));
                true
            }
            Some(PendingCompletion::Write(Ok(output))) => {
                let bytes = output.bytes;
                *self.encrypted_write_buffer.borrow_mut() = output.buffer;
                if bytes == 0 {
                    self.complete(Err(Errno::PIPE));
                    return true;
                }
                self.stream.borrow_mut().ssl.use_pull_buffer(bytes);
                true
            }
            Some(PendingCompletion::Write(Err(error))) => {
                self.complete(Err(error));
                true
            }
            None => true,
        }
    }

    fn start_encrypted_read(&self) -> Result<bool, Errno> {
        let len = {
            let mut stream = self.stream.borrow_mut();
            stream.ensure_usable()?;
            if stream.socket.is_none() {
                return Err(Errno::PIPE);
            }
            let Some(buffer) = stream.ssl.get_push_buffer() else {
                return Ok(false);
            };
            buffer.len()
        };

        let mut buffer = mem::take(&mut *self.encrypted_read_buffer.borrow_mut());
        buffer.resize(len, 0);
        let read = {
            let stream = self.stream.borrow();
            let socket = stream.socket.as_ref().ok_or(Errno::PIPE)?;
            SocketIoRuntime::read_async(self.runtime, socket, buffer)
                .map_err(runtime_io_error_as_errno)?
        };
        *self.pending.borrow_mut() = Some(TlsPendingIo::Read(read));
        Ok(true)
    }

    fn start_encrypted_write(&self) -> Result<bool, Errno> {
        let mut buffer = mem::take(&mut *self.encrypted_write_buffer.borrow_mut());
        {
            let stream = self.stream.borrow_mut();
            stream.ensure_usable()?;
            if stream.socket.is_none() {
                return Err(Errno::PIPE);
            }
            let Some(pull_buffer) = stream.ssl.get_pull_buffer() else {
                *self.encrypted_write_buffer.borrow_mut() = buffer;
                return Ok(false);
            };
            buffer.clear();
            buffer.extend_from_slice(pull_buffer);
        }

        let write = {
            let stream = self.stream.borrow();
            let socket = stream.socket.as_ref().ok_or(Errno::PIPE)?;
            SocketIoRuntime::write_async(self.runtime, socket, buffer)
                .map_err(runtime_io_error_as_errno)?
        };
        *self.pending.borrow_mut() = Some(TlsPendingIo::Write(write));
        Ok(true)
    }

    fn complete(&self, result: Result<usize, Errno>) {
        *self.result.borrow_mut() = Some(result);
    }
}

impl<B, R> RuntimeWaitable for TlsReadResult<'_, B, R>
where
    R: SocketIoRuntime + StackfulWaitContext,
    R::ReadResult<Vec<u8>>: RuntimeReadResult<Vec<u8>, Output = ReadOutput<Vec<u8>>>,
    R::WriteResult<Vec<u8>>: RuntimeWriteResult<Vec<u8>, Output = WriteOutput<Vec<u8>>>,
    B: IoReadBuffer,
{
    fn is_ready(&self) -> bool {
        if self.taken {
            return true;
        }
        self.progress();
        self.result.borrow().is_some()
    }

    fn add_stackful_waiter(&self, waiter: kimojio_stack::StackfulWaiterHandle) -> bool {
        if self.taken {
            return false;
        }
        self.progress();
        if self.result.borrow().is_some() {
            return false;
        }
        if let Some(pending) = self.pending.borrow().as_ref() {
            pending.add_stackful_waiter(waiter)
        } else {
            false
        }
    }
}

impl<B, R> Waitable for TlsReadResult<'_, B, R>
where
    R: SocketIoRuntime + StackfulWaitContext,
    R::ReadResult<Vec<u8>>: RuntimeReadResult<Vec<u8>, Output = ReadOutput<Vec<u8>>>,
    R::WriteResult<Vec<u8>>: RuntimeWriteResult<Vec<u8>, Output = WriteOutput<Vec<u8>>>,
    B: IoReadBuffer,
{
    fn is_ready(&self) -> bool {
        RuntimeWaitable::is_ready(self)
    }

    fn add_waiter(&self, cx: &RuntimeContext<'_>, registration: &WaitRegistration) {
        if RuntimeWaitable::is_ready(self) {
            return;
        }
        let adapter = RuntimeWaitableAdapter::new(self);
        Waitable::add_waiter(&adapter, cx, registration);
    }
}

impl<B, R> fmt::Debug for TlsReadResult<'_, B, R>
where
    R: SocketIoRuntime + StackfulWaitContext,
    R::ReadResult<Vec<u8>>: RuntimeReadResult<Vec<u8>, Output = ReadOutput<Vec<u8>>>,
    R::WriteResult<Vec<u8>>: RuntimeWriteResult<Vec<u8>, Output = WriteOutput<Vec<u8>>>,
    B: IoReadBuffer,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsReadResult")
            .field("ready", &RuntimeWaitable::is_ready(self))
            .field("taken", &self.taken)
            .finish()
    }
}

impl<B, R> Drop for TlsReadResult<'_, B, R>
where
    R: SocketIoRuntime + StackfulWaitContext,
{
    fn drop(&mut self) {
        if !self.taken && self.result.borrow().is_none() {
            self.stream.borrow_mut().poison();
        }
    }
}

/// A pending TLS plaintext write.
#[must_use = "pending TLS writes should be completed with get, try_get, or a runtime wait"]
pub struct TlsWriteResult<'operation, B, R = RuntimeContext<'operation>>
where
    R: SocketIoRuntime + StackfulWaitContext,
{
    runtime: &'operation R,
    stream: RefCell<&'operation mut RuntimeTlsStream<R::Socket>>,
    buffer: RefCell<Option<B>>,
    encrypted_read_buffer: RefCell<Vec<u8>>,
    encrypted_write_buffer: RefCell<Vec<u8>>,
    pending: RefCell<Option<TlsPendingIo<R>>>,
    result: RefCell<Option<Result<usize, Errno>>>,
    plaintext_offset: Cell<usize>,
    taken: bool,
}

impl<'operation, B, R> TlsWriteResult<'operation, B, R>
where
    R: SocketIoRuntime + StackfulWaitContext,
    R::ReadResult<Vec<u8>>: RuntimeReadResult<Vec<u8>, Output = ReadOutput<Vec<u8>>>,
    R::WriteResult<Vec<u8>>: RuntimeWriteResult<Vec<u8>, Output = WriteOutput<Vec<u8>>>,
    B: IoWriteBuffer,
{
    fn new(
        runtime: &'operation R,
        stream: &'operation mut RuntimeTlsStream<R::Socket>,
        buffer: B,
    ) -> Self {
        Self {
            runtime,
            stream: RefCell::new(stream),
            buffer: RefCell::new(Some(buffer)),
            encrypted_read_buffer: RefCell::new(Vec::new()),
            encrypted_write_buffer: RefCell::new(Vec::new()),
            pending: RefCell::new(None),
            result: RefCell::new(None),
            plaintext_offset: Cell::new(0),
            taken: false,
        }
    }

    /// Returns the completed write if it is ready.
    ///
    /// This does not park the current coroutine. If the TLS operation needs more
    /// socket I/O that has not already been submitted, call [`get`](Self::get) or
    /// wait on the handle through [`RuntimeContext::select`] or
    /// [`RuntimeContext::join`].
    pub fn try_get(&mut self) -> Option<Result<WriteOutput<B>, Errno>> {
        assert!(!self.taken, "TlsWriteResult value already taken");
        self.progress();
        let result = self.result.borrow_mut().take()?;
        self.taken = true;
        let buffer = self
            .buffer
            .borrow_mut()
            .take()
            .expect("TlsWriteResult buffer missing");

        Some(result.map(|bytes| WriteOutput { bytes, buffer }))
    }

    /// Waits for the TLS write to complete.
    pub fn get(mut self, cx: &RuntimeContext<'_>) -> Result<WriteOutput<B>, Errno> {
        loop {
            if let Some(result) = self.try_get() {
                return result;
            }

            let waitables: [&dyn Waitable; 1] = [&self];
            cx.wait_all(&waitables, None)
                .expect("single TLS write waitable should be waitable");
        }
    }

    /// Attempts to cancel this TLS write using the runtime captured by
    /// [`RuntimeTlsStream::write_async`].
    ///
    /// Cancellation drains the private socket I/O used by the TLS state machine,
    /// then marks the TLS stream unusable. A canceled write may already have
    /// emitted encrypted bytes to the socket that OpenSSL has not accounted for,
    /// so continuing the same stream would risk duplicate protocol data. After
    /// successful cancellation, [`try_get`](Self::try_get) returns
    /// `Err(Errno::CANCELED)`.
    pub fn cancel(&self) -> Result<(), Errno> {
        if self.taken || self.result.borrow().is_some() {
            return Ok(());
        }

        {
            let mut pending = self.pending.borrow_mut();
            if let Some(pending) = pending.as_mut() {
                pending.cancel(self.runtime)?;
            }
        }

        if let Some(pending) = self.pending.borrow_mut().take() {
            pending.drain(self.runtime)?;
        }
        self.stream.borrow_mut().poison();
        self.complete(Err(Errno::CANCELED));
        Ok(())
    }

    fn progress(&self) {
        if self.result.borrow().is_some() {
            return;
        }

        loop {
            if !self.finish_pending() || self.result.borrow().is_some() {
                return;
            }

            let plaintext_len = self.plaintext_len();
            let plaintext_offset = self.plaintext_offset.get();
            if plaintext_offset >= plaintext_len {
                match self.has_encrypted_write() {
                    Ok(false) => {
                        self.complete(Ok(plaintext_len));
                    }
                    Ok(true) => match self.start_encrypted_write() {
                        Ok(true) => return,
                        Ok(false) => self.complete(Ok(plaintext_len)),
                        Err(error) => self.complete(Err(error)),
                    },
                    Err(error) => self.complete(Err(error)),
                }
                return;
            }

            let response = {
                let mut stream = self.stream.borrow_mut();
                if let Err(error) = stream.ensure_usable() {
                    self.complete(Err(error));
                    return;
                }
                let buffer = self.buffer.borrow();
                let buffer = buffer.as_ref().expect("TlsWriteResult buffer missing");
                let plaintext = plaintext_write_slice(buffer, plaintext_offset);
                stream.ssl.write(plaintext)
            };

            match response {
                Response::Success(amount) => {
                    if amount == 0 {
                        self.complete(Err(Errno::PIPE));
                        return;
                    }
                    let next = self.plaintext_offset.get().checked_add(amount);
                    let Some(next) = next else {
                        self.complete(Err(Errno::PROTO));
                        return;
                    };
                    self.plaintext_offset.set(next);
                }
                Response::Fail(error) => {
                    let error = self.stream.borrow().handle_tls_error(error);
                    self.complete(Err(error));
                    return;
                }
                Response::WantRead => match self.start_encrypted_read() {
                    Ok(true) => return,
                    Ok(false) => {}
                    Err(error) => {
                        self.complete(Err(error));
                        return;
                    }
                },
                Response::WantWrite => match self.start_encrypted_write() {
                    Ok(true) => return,
                    Ok(false) => {}
                    Err(error) => {
                        self.complete(Err(error));
                        return;
                    }
                },
                Response::Eof => {
                    self.complete(Err(Errno::PIPE));
                    return;
                }
            }
        }
    }

    fn finish_pending(&self) -> bool {
        let completion = {
            let mut pending = self.pending.borrow_mut();
            match pending.as_mut() {
                Some(TlsPendingIo::Read(read)) => match RuntimeReadResult::try_get(read) {
                    Some(result) => {
                        *pending = None;
                        Some(PendingCompletion::Read(
                            result.map_err(runtime_io_error_as_errno),
                        ))
                    }
                    None => return false,
                },
                Some(TlsPendingIo::Write(write)) => match RuntimeWriteResult::try_get(write) {
                    Some(result) => {
                        *pending = None;
                        Some(PendingCompletion::Write(
                            result.map_err(runtime_io_error_as_errno),
                        ))
                    }
                    None => return false,
                },
                None => None,
            }
        };

        match completion {
            Some(PendingCompletion::Read(Ok(output))) => {
                let bytes = output.bytes;
                let buffer = output.buffer;
                if bytes == 0 {
                    *self.encrypted_read_buffer.borrow_mut() = buffer;
                    self.complete(Err(Errno::PIPE));
                    return true;
                }

                {
                    let mut stream = self.stream.borrow_mut();
                    let Some(push_buffer) = stream.ssl.get_push_buffer() else {
                        self.complete(Err(Errno::PROTO));
                        return true;
                    };
                    if bytes > push_buffer.len() || bytes > buffer.len() {
                        self.complete(Err(Errno::PROTO));
                        return true;
                    }
                    push_buffer[..bytes].copy_from_slice(&buffer[..bytes]);
                    stream.ssl.use_push_buffer(bytes);
                }

                *self.encrypted_read_buffer.borrow_mut() = buffer;
                true
            }
            Some(PendingCompletion::Read(Err(error))) => {
                self.complete(Err(error));
                true
            }
            Some(PendingCompletion::Write(Ok(output))) => {
                let bytes = output.bytes;
                *self.encrypted_write_buffer.borrow_mut() = output.buffer;
                if bytes == 0 {
                    self.complete(Err(Errno::PIPE));
                    return true;
                }
                self.stream.borrow_mut().ssl.use_pull_buffer(bytes);
                true
            }
            Some(PendingCompletion::Write(Err(error))) => {
                self.complete(Err(error));
                true
            }
            None => true,
        }
    }

    fn start_encrypted_read(&self) -> Result<bool, Errno> {
        let len = {
            let mut stream = self.stream.borrow_mut();
            stream.ensure_usable()?;
            if stream.socket.is_none() {
                return Err(Errno::PIPE);
            }
            let Some(buffer) = stream.ssl.get_push_buffer() else {
                return Ok(false);
            };
            buffer.len()
        };

        let mut buffer = mem::take(&mut *self.encrypted_read_buffer.borrow_mut());
        buffer.resize(len, 0);
        let read = {
            let stream = self.stream.borrow();
            let socket = stream.socket.as_ref().ok_or(Errno::PIPE)?;
            SocketIoRuntime::read_async(self.runtime, socket, buffer)
                .map_err(runtime_io_error_as_errno)?
        };
        *self.pending.borrow_mut() = Some(TlsPendingIo::Read(read));
        Ok(true)
    }

    fn start_encrypted_write(&self) -> Result<bool, Errno> {
        let mut buffer = mem::take(&mut *self.encrypted_write_buffer.borrow_mut());
        {
            let stream = self.stream.borrow_mut();
            stream.ensure_usable()?;
            if stream.socket.is_none() {
                return Err(Errno::PIPE);
            }
            let Some(pull_buffer) = stream.ssl.get_pull_buffer() else {
                *self.encrypted_write_buffer.borrow_mut() = buffer;
                return Ok(false);
            };
            buffer.clear();
            buffer.extend_from_slice(pull_buffer);
        }

        let write = {
            let stream = self.stream.borrow();
            let socket = stream.socket.as_ref().ok_or(Errno::PIPE)?;
            SocketIoRuntime::write_async(self.runtime, socket, buffer)
                .map_err(runtime_io_error_as_errno)?
        };
        *self.pending.borrow_mut() = Some(TlsPendingIo::Write(write));
        Ok(true)
    }

    fn has_encrypted_write(&self) -> Result<bool, Errno> {
        let stream = self.stream.borrow_mut();
        stream.ensure_usable()?;
        if stream.socket.is_none() {
            return Err(Errno::PIPE);
        }
        Ok(stream.ssl.get_pull_buffer().is_some())
    }

    fn plaintext_len(&self) -> usize {
        self.buffer
            .borrow()
            .as_ref()
            .expect("TlsWriteResult buffer missing")
            .io_buffer_len()
    }

    fn complete(&self, result: Result<usize, Errno>) {
        *self.result.borrow_mut() = Some(result);
    }
}

impl<B, R> RuntimeWaitable for TlsWriteResult<'_, B, R>
where
    R: SocketIoRuntime + StackfulWaitContext,
    R::ReadResult<Vec<u8>>: RuntimeReadResult<Vec<u8>, Output = ReadOutput<Vec<u8>>>,
    R::WriteResult<Vec<u8>>: RuntimeWriteResult<Vec<u8>, Output = WriteOutput<Vec<u8>>>,
    B: IoWriteBuffer,
{
    fn is_ready(&self) -> bool {
        if self.taken {
            return true;
        }
        self.progress();
        self.result.borrow().is_some()
    }

    fn add_stackful_waiter(&self, waiter: kimojio_stack::StackfulWaiterHandle) -> bool {
        if self.taken {
            return false;
        }
        self.progress();
        if self.result.borrow().is_some() {
            return false;
        }
        if let Some(pending) = self.pending.borrow().as_ref() {
            pending.add_stackful_waiter(waiter)
        } else {
            false
        }
    }
}

impl<B, R> Waitable for TlsWriteResult<'_, B, R>
where
    R: SocketIoRuntime + StackfulWaitContext,
    R::ReadResult<Vec<u8>>: RuntimeReadResult<Vec<u8>, Output = ReadOutput<Vec<u8>>>,
    R::WriteResult<Vec<u8>>: RuntimeWriteResult<Vec<u8>, Output = WriteOutput<Vec<u8>>>,
    B: IoWriteBuffer,
{
    fn is_ready(&self) -> bool {
        RuntimeWaitable::is_ready(self)
    }

    fn add_waiter(&self, cx: &RuntimeContext<'_>, registration: &WaitRegistration) {
        if RuntimeWaitable::is_ready(self) {
            return;
        }
        let adapter = RuntimeWaitableAdapter::new(self);
        Waitable::add_waiter(&adapter, cx, registration);
    }
}

impl<B, R> fmt::Debug for TlsWriteResult<'_, B, R>
where
    R: SocketIoRuntime + StackfulWaitContext,
    R::ReadResult<Vec<u8>>: RuntimeReadResult<Vec<u8>, Output = ReadOutput<Vec<u8>>>,
    R::WriteResult<Vec<u8>>: RuntimeWriteResult<Vec<u8>, Output = WriteOutput<Vec<u8>>>,
    B: IoWriteBuffer,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsWriteResult")
            .field("ready", &RuntimeWaitable::is_ready(self))
            .field("taken", &self.taken)
            .finish()
    }
}

impl<B, R> Drop for TlsWriteResult<'_, B, R>
where
    R: SocketIoRuntime + StackfulWaitContext,
{
    fn drop(&mut self) {
        if !self.taken && self.result.borrow().is_none() {
            self.stream.borrow_mut().poison();
        }
    }
}

enum PendingCompletion {
    Read(Result<ReadOutput<Vec<u8>>, Errno>),
    Write(Result<WriteOutput<Vec<u8>>, Errno>),
}

fn drain_read_result<R>(cx: &R, read: &mut R::ReadResult<Vec<u8>>) -> Result<(), Errno>
where
    R: SocketIoRuntime + StackfulWaitContext,
    R::ReadResult<Vec<u8>>: RuntimeReadResult<Vec<u8>, Output = ReadOutput<Vec<u8>>>,
{
    loop {
        if RuntimeReadResult::try_get(read).is_some() {
            return Ok(());
        }
        cx.wait_stackful(read)
            .map_err(runtime_wait_error_as_errno)?;
    }
}

fn drain_write_result<R>(cx: &R, write: &mut R::WriteResult<Vec<u8>>) -> Result<(), Errno>
where
    R: SocketIoRuntime + StackfulWaitContext,
    R::WriteResult<Vec<u8>>: RuntimeWriteResult<Vec<u8>, Output = WriteOutput<Vec<u8>>>,
{
    loop {
        if RuntimeWriteResult::try_get(write).is_some() {
            return Ok(());
        }
        cx.wait_stackful(write)
            .map_err(runtime_wait_error_as_errno)?;
    }
}

fn plaintext_read_slice<B>(buffer: &mut B) -> &mut [u8]
where
    B: IoReadBuffer,
{
    let len = buffer.io_buffer_len();
    let ptr = buffer.io_buffer_mut_ptr();
    // SAFETY: IoReadBuffer requires the returned pointer to remain valid for
    // writes of io_buffer_len bytes until the buffer is dropped.
    unsafe { slice::from_raw_parts_mut(ptr, len) }
}

fn plaintext_write_slice<B>(buffer: &B, offset: usize) -> &[u8]
where
    B: IoWriteBuffer,
{
    let len = buffer.io_buffer_len();
    assert!(offset <= len, "TLS write offset exceeds buffer length");
    let ptr = buffer.io_buffer_ptr();
    // SAFETY: IoWriteBuffer requires the returned pointer to remain valid for
    // reads of io_buffer_len bytes until the buffer is dropped. `offset` is
    // checked above.
    unsafe { slice::from_raw_parts(ptr.add(offset), len - offset) }
}

fn as_io_error(error: TlsServerError) -> Errno {
    match error {
        TlsServerError::Errno(errno) => errno,
        TlsServerError::TlsError(_) => Errno::PROTO,
    }
}

fn runtime_io_error_as_errno(error: RuntimeIoError) -> Errno {
    match error {
        RuntimeIoError::Io(errno) => errno,
        RuntimeIoError::Unsupported(_) => Errno::OPNOTSUPP,
        RuntimeIoError::Runtime(kind) => match kind {
            kimojio_stack::RuntimeIoErrorKind::QueueFull
            | kimojio_stack::RuntimeIoErrorKind::ResourceLimit => Errno::AGAIN,
            kimojio_stack::RuntimeIoErrorKind::FdInUse => Errno::BUSY,
            kimojio_stack::RuntimeIoErrorKind::Canceled => Errno::CANCELED,
            kimojio_stack::RuntimeIoErrorKind::Closed => Errno::PIPE,
            kimojio_stack::RuntimeIoErrorKind::NoCurrentWorker
            | kimojio_stack::RuntimeIoErrorKind::WrongWorker
            | kimojio_stack::RuntimeIoErrorKind::WrongRuntime
            | kimojio_stack::RuntimeIoErrorKind::NoStackfulContext
            | kimojio_stack::RuntimeIoErrorKind::DurationOutOfRange => Errno::INVAL,
        },
        RuntimeIoError::Other(_) => Errno::INVAL,
    }
}

fn runtime_wait_error_as_errno(error: RuntimeWaitError) -> Errno {
    match error {
        RuntimeWaitError::Unsupported(_) => Errno::OPNOTSUPP,
        RuntimeWaitError::Empty => Errno::INVAL,
        RuntimeWaitError::NoStackfulContext { .. } => Errno::INVAL,
    }
}

#[cfg(test)]
mod tests {
    use std::{mem::size_of, num::NonZeroUsize, sync::Arc, time::Duration};

    use kimojio_stack::{Runtime, RuntimeWaitable, StackfulWaitContext, Waitable};
    use openssl::{
        asn1::Asn1Time,
        hash::MessageDigest,
        nid::Nid,
        pkey::PKey,
        rsa::Rsa,
        ssl::{SslAcceptor, SslConnector, SslMethod, SslVerifyMode},
        x509::{X509, X509NameBuilder},
    };
    use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};

    use super::*;

    const TLS_BUFFER_SIZE: usize = 16 * 1024;
    const RPC_HEADER_LEN: usize = 64;
    const RPC_RESPONSE_LEN: usize = 64;

    fn make_contexts() -> (TlsContext, TlsContext) {
        let rsa = Rsa::generate(2048).unwrap();
        let key = PKey::from_rsa(rsa).unwrap();

        let mut name = X509NameBuilder::new().unwrap();
        name.append_entry_by_nid(Nid::COMMONNAME, "localhost")
            .unwrap();
        let name = name.build();

        let mut cert = X509::builder().unwrap();
        cert.set_version(2).unwrap();
        cert.set_subject_name(&name).unwrap();
        cert.set_issuer_name(&name).unwrap();
        cert.set_pubkey(&key).unwrap();
        cert.set_not_before(Asn1Time::days_from_now(0).unwrap().as_ref())
            .unwrap();
        cert.set_not_after(Asn1Time::days_from_now(1).unwrap().as_ref())
            .unwrap();
        cert.sign(&key, MessageDigest::sha256()).unwrap();
        let cert = cert.build();

        let mut acceptor = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).unwrap();
        acceptor.set_private_key(&key).unwrap();
        acceptor.set_certificate(&cert).unwrap();
        acceptor.check_private_key().unwrap();

        let mut connector = SslConnector::builder(SslMethod::tls()).unwrap();
        connector.set_verify(SslVerifyMode::NONE);

        (
            TlsContext::from_openssl(acceptor.build().into_context()),
            TlsContext::from_openssl(connector.build().into_context()),
        )
    }

    fn make_rpc_header(body_len: usize) -> [u8; RPC_HEADER_LEN] {
        let mut header = [0xa5; RPC_HEADER_LEN];
        header[..size_of::<u64>()].copy_from_slice(&(body_len as u64).to_le_bytes());
        header
    }

    fn rpc_body_len(header: &[u8; RPC_HEADER_LEN]) -> usize {
        let mut len = [0_u8; size_of::<u64>()];
        len.copy_from_slice(&header[..size_of::<u64>()]);
        u64::from_le_bytes(len) as usize
    }

    #[test]
    fn tls_echo_over_stackful_socketpair() {
        let (server_ctx, client_ctx) = make_contexts();
        let (client_fd, server_fd) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::empty(),
            None,
        )
        .unwrap();

        let message = b"hello from stack tls; this should span several tiny reads";
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut tls = server_ctx
                        .server(cx, TLS_BUFFER_SIZE, server_fd)
                        .expect("server TLS handshake failed");
                    let mut buf = [0_u8; 7];
                    let mut read = 0;
                    while read < message.len() {
                        let amount = tls.read(cx, &mut buf).expect("server TLS read failed");
                        assert_ne!(amount, 0);
                        read += amount;
                        tls.write(cx, &buf[..amount])
                            .expect("server TLS write failed");
                    }
                    tls.shutdown(cx).expect("server TLS shutdown failed");
                    tls.close(cx).expect("server TLS close failed");
                });

                let client = scope.spawn(move |cx| {
                    let mut tls = client_ctx
                        .client(cx, TLS_BUFFER_SIZE, client_fd, "localhost")
                        .expect("client TLS handshake failed");
                    tls.write(cx, message).expect("client TLS write failed");

                    let mut echoed = Vec::new();
                    let mut buf = [0_u8; 5];
                    while echoed.len() < message.len() {
                        let amount = tls.read(cx, &mut buf).expect("client TLS read failed");
                        if amount == 0 {
                            break;
                        }
                        echoed.extend_from_slice(&buf[..amount]);
                    }
                    tls.shutdown(cx).expect("client TLS shutdown failed");
                    tls.close(cx).expect("client TLS close failed");
                    echoed
                });

                server.join(cx);
                let echoed = client.join(cx);
                assert_eq!(echoed, message);
            });
        });
    }

    #[test]
    fn tls_echo_over_runtime_agnostic_stealing_socketpair() {
        let (server_ctx, client_ctx) = make_contexts();
        let (client_fd, server_fd) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::empty(),
            None,
        )
        .unwrap();

        let message = b"hello from generic stack tls over a stealing runtime";
        let mut runtime =
            kimojio_stack_steal::Runtime::with_config(kimojio_stack_steal::RuntimeConfig {
                workers: NonZeroUsize::new(2).unwrap(),
                ..kimojio_stack_steal::RuntimeConfig::default()
            });

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut tls = server_ctx
                        .server_with_runtime(cx, TLS_BUFFER_SIZE, server_fd)
                        .expect("server TLS handshake failed");
                    let mut buf = [0_u8; 11];
                    let mut read = 0;
                    while read < message.len() {
                        let amount = tls.read(cx, &mut buf).expect("server TLS read failed");
                        assert_ne!(amount, 0);
                        read += amount;
                        tls.write(cx, &buf[..amount])
                            .expect("server TLS write failed");
                    }
                    tls.shutdown(cx).expect("server TLS shutdown failed");
                    tls.close(cx).expect("server TLS close failed");
                });

                let client = scope.spawn(move |cx| {
                    let mut tls = client_ctx
                        .client_with_runtime(cx, TLS_BUFFER_SIZE, client_fd, "localhost")
                        .expect("client TLS handshake failed");
                    tls.write(cx, message).expect("client TLS write failed");

                    let mut echoed = Vec::new();
                    let mut buf = [0_u8; 13];
                    while echoed.len() < message.len() {
                        let amount = tls.read(cx, &mut buf).expect("client TLS read failed");
                        if amount == 0 {
                            break;
                        }
                        echoed.extend_from_slice(&buf[..amount]);
                    }
                    tls.shutdown(cx).expect("client TLS shutdown failed");
                    tls.close(cx).expect("client TLS close failed");
                    echoed
                });

                server.join(cx);
                let echoed = client.join(cx);
                assert_eq!(echoed, message);
            });
        });
    }

    #[test]
    fn tls_context_can_be_shared_across_stealable_connection_tasks() {
        let (server_ctx, client_ctx) = make_contexts();
        let server_ctx = Arc::new(server_ctx);
        let client_ctx = Arc::new(client_ctx);
        let pairs = (0..4)
            .map(|_| {
                socketpair(
                    AddressFamily::UNIX,
                    SocketType::STREAM,
                    SocketFlags::empty(),
                    None,
                )
                .unwrap()
            })
            .collect::<Vec<_>>();

        let mut runtime =
            kimojio_stack_steal::Runtime::with_config(kimojio_stack_steal::RuntimeConfig {
                workers: NonZeroUsize::new(2).unwrap(),
                ..kimojio_stack_steal::RuntimeConfig::default()
            });

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let mut handles = Vec::new();
                for (index, (client_fd, server_fd)) in pairs.into_iter().enumerate() {
                    let server_ctx = Arc::clone(&server_ctx);
                    let message = format!("shared tls context message {index}").into_bytes();
                    handles.push(scope.spawn_stealable(move |cx| {
                        let mut tls = server_ctx
                            .server_with_runtime(cx, TLS_BUFFER_SIZE, server_fd)
                            .expect("server TLS handshake failed");
                        let mut buf = vec![0_u8; message.len()];
                        tls.read_exact_or_eof(cx, &mut buf)
                            .expect("server TLS read failed");
                        assert_eq!(buf, message);
                        tls.write(cx, &buf).expect("server TLS write failed");
                        tls.shutdown(cx).expect("server TLS shutdown failed");
                        tls.close(cx).expect("server TLS close failed");
                    }));

                    let client_ctx = Arc::clone(&client_ctx);
                    handles.push(scope.spawn_stealable(move |cx| {
                        let message = format!("shared tls context message {index}").into_bytes();
                        let mut tls = client_ctx
                            .client_with_runtime(cx, TLS_BUFFER_SIZE, client_fd, "localhost")
                            .expect("client TLS handshake failed");
                        tls.write(cx, &message).expect("client TLS write failed");
                        let mut echoed = vec![0_u8; message.len()];
                        tls.read_exact_or_eof(cx, &mut echoed)
                            .expect("client TLS read failed");
                        assert_eq!(echoed, message);
                        tls.shutdown(cx).expect("client TLS shutdown failed");
                        tls.close(cx).expect("client TLS close failed");
                    }));
                }

                for handle in handles {
                    handle.join(cx);
                }
            });
        });
    }

    #[test]
    fn tls_async_read_write_handles_are_runtime_neutral_on_stealing_runtime() {
        let (server_ctx, client_ctx) = make_contexts();
        let (client_fd, server_fd) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::empty(),
            None,
        )
        .unwrap();

        let message = b"runtime-neutral TLS async over a stealing runtime";
        let mut runtime =
            kimojio_stack_steal::Runtime::with_config(kimojio_stack_steal::RuntimeConfig {
                workers: NonZeroUsize::new(2).unwrap(),
                ..kimojio_stack_steal::RuntimeConfig::default()
            });

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut tls = server_ctx
                        .server_with_runtime(cx, TLS_BUFFER_SIZE, server_fd)
                        .expect("server TLS handshake failed");

                    let read = {
                        let mut read = tls.read_async(cx, vec![0_u8; message.len()]);
                        cx.wait_stackful(&read)
                            .expect("server TLS runtime-neutral read wait failed");
                        let output = read
                            .try_get()
                            .expect("server TLS async read not ready")
                            .expect("server TLS async read failed");
                        drop(read);
                        output
                    };
                    assert_eq!(&read.buffer[..read.bytes], message);

                    let written = {
                        let mut write = tls.write_async(cx, message.to_vec());
                        cx.wait_stackful(&write)
                            .expect("server TLS runtime-neutral write wait failed");
                        let output = write
                            .try_get()
                            .expect("server TLS async write not ready")
                            .expect("server TLS async write failed");
                        drop(write);
                        output
                    };
                    assert_eq!(written.bytes, message.len());

                    tls.shutdown(cx).expect("server TLS shutdown failed");
                    tls.close(cx).expect("server TLS close failed");
                });

                let client = scope.spawn(move |cx| {
                    let mut tls = client_ctx
                        .client_with_runtime(cx, TLS_BUFFER_SIZE, client_fd, "localhost")
                        .expect("client TLS handshake failed");

                    let written = {
                        let mut write = tls.write_async(cx, message.to_vec());
                        cx.wait_stackful(&write)
                            .expect("client TLS runtime-neutral write wait failed");
                        let output = write
                            .try_get()
                            .expect("client TLS async write not ready")
                            .expect("client TLS async write failed");
                        drop(write);
                        output
                    };
                    assert_eq!(written.bytes, message.len());

                    let echoed = {
                        let mut read = tls.read_async(cx, vec![0_u8; message.len()]);
                        cx.wait_stackful(&read)
                            .expect("client TLS runtime-neutral read wait failed");
                        let output = read
                            .try_get()
                            .expect("client TLS async read not ready")
                            .expect("client TLS async read failed");
                        drop(read);
                        output
                    };

                    tls.shutdown(cx).expect("client TLS shutdown failed");
                    tls.close(cx).expect("client TLS close failed");
                    echoed.buffer[..echoed.bytes].to_vec()
                });

                server.join(cx);
                let echoed = client.join(cx);
                assert_eq!(echoed, message);
            });
        });
    }

    #[test]
    fn tls_async_read_write_handles_are_waitable() {
        let (server_ctx, client_ctx) = make_contexts();
        let (client_fd, server_fd) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::empty(),
            None,
        )
        .unwrap();

        let message = b"waitable TLS plaintext";
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut tls = server_ctx
                        .server(cx, TLS_BUFFER_SIZE, server_fd)
                        .expect("server TLS handshake failed");

                    let mut read = tls.read_async(cx, vec![0_u8; message.len()]);
                    let waitables: [&dyn Waitable; 1] = [&read];
                    cx.wait_all(&waitables, Some(Duration::from_secs(1)))
                        .expect("server TLS async read timed out");
                    let read_output = read
                        .try_get()
                        .expect("server TLS async read not ready")
                        .expect("server TLS async read failed");
                    assert!(RuntimeWaitable::is_ready(&read));
                    drop(read);
                    let read = read_output;
                    assert_eq!(&read.buffer[..read.bytes], message);

                    let mut write = tls.write_async(cx, read.buffer[..read.bytes].to_vec());
                    let waitables: [&dyn Waitable; 1] = [&write];
                    cx.join(&waitables, Some(Duration::from_secs(1)))
                        .expect("server TLS async write timed out");
                    let written = write
                        .try_get()
                        .expect("server TLS async write not ready")
                        .expect("server TLS async write failed");
                    assert!(RuntimeWaitable::is_ready(&write));
                    drop(write);
                    assert_eq!(written.bytes, message.len());

                    tls.shutdown(cx).expect("server TLS shutdown failed");
                    tls.close(cx).expect("server TLS close failed");
                });

                let client = scope.spawn(move |cx| {
                    let mut tls = client_ctx
                        .client(cx, TLS_BUFFER_SIZE, client_fd, "localhost")
                        .expect("client TLS handshake failed");

                    let mut write = tls.write_async(cx, message.to_vec());
                    let waitables: [&dyn Waitable; 1] = [&write];
                    cx.wait_all(&waitables, Some(Duration::from_secs(1)))
                        .expect("client TLS async write timed out");
                    let written = write
                        .try_get()
                        .expect("client TLS async write not ready")
                        .expect("client TLS async write failed");
                    assert!(RuntimeWaitable::is_ready(&write));
                    drop(write);
                    assert_eq!(written.bytes, message.len());

                    let read = tls.read_async(cx, vec![0_u8; message.len()]);
                    let mut timeout = cx.timeout(Duration::from_secs(1));
                    let waitables: [&dyn Waitable; 2] = [&read, &timeout];
                    let ready = cx
                        .select(&waitables, None)
                        .expect("client TLS async select failed");
                    assert_eq!(ready, 0);
                    timeout.cancel();

                    let echoed = read.get(cx).expect("client TLS async read failed");
                    tls.shutdown(cx).expect("client TLS shutdown failed");
                    tls.close(cx).expect("client TLS close failed");
                    echoed.buffer[..echoed.bytes].to_vec()
                });

                server.join(cx);
                let echoed = client.join(cx);
                assert_eq!(echoed, message);
            });
        });
    }

    #[test]
    fn tls_async_read_can_be_canceled_after_select_timeout() {
        let (server_ctx, client_ctx) = make_contexts();
        let (client_fd, server_fd) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::empty(),
            None,
        )
        .unwrap();

        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut tls = server_ctx
                        .server(cx, TLS_BUFFER_SIZE, server_fd)
                        .expect("server TLS handshake failed");

                    if !cx.supports_io_uring_opcode(rustix_uring::opcode::AsyncCancel::CODE) {
                        tls.close(cx).expect("server TLS close failed");
                        return false;
                    }

                    let mut read = tls.read_async(cx, vec![0_u8; 1]);
                    let mut timeout = cx.timeout(Duration::from_millis(1));
                    let waitables: [&dyn Waitable; 2] = [&read, &timeout];
                    let ready = cx
                        .select(&waitables, None)
                        .expect("server TLS async select failed");
                    assert_eq!(ready, 1);
                    timeout.cancel();

                    read.cancel().expect("cancel TLS read failed");
                    assert!(RuntimeWaitable::is_ready(&read));
                    assert_eq!(
                        read.try_get().expect("canceled TLS read not ready").err(),
                        Some(Errno::CANCELED)
                    );
                    drop(read);

                    let mut buf = [0_u8; 1];
                    assert_eq!(tls.read(cx, &mut buf), Err(Errno::PIPE));
                    tls.close(cx).expect("server TLS close failed");
                    true
                });

                let client = scope.spawn(move |cx| {
                    let tls = client_ctx
                        .client(cx, TLS_BUFFER_SIZE, client_fd, "localhost")
                        .expect("client TLS handshake failed");
                    cx.sleep(Duration::from_millis(50)).unwrap();
                    tls.close(cx).expect("client TLS close failed");
                });

                let cancel_supported = server.join(cx);
                client.join(cx);
                if !cancel_supported {
                    eprintln!("skipped TLS async cancel path: io_uring async cancel unsupported");
                }
            });
        });
    }

    #[test]
    fn tls_rpc_write_header_body_response_over_stackful_socketpair() {
        let (server_ctx, client_ctx) = make_contexts();
        let (client_fd, server_fd) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::empty(),
            None,
        )
        .unwrap();

        let header = make_rpc_header(8 * 1024);
        let body = vec![0x5a; 8 * 1024];
        let response = [0x7b; RPC_RESPONSE_LEN];
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut tls = server_ctx
                        .server(cx, TLS_BUFFER_SIZE, server_fd)
                        .expect("server TLS handshake failed");
                    let mut header = [0_u8; RPC_HEADER_LEN];
                    let mut body = vec![0_u8; 8 * 1024];

                    let amount = tls
                        .read_exact_or_eof(cx, &mut header)
                        .expect("server TLS header read failed");
                    assert_eq!(amount, RPC_HEADER_LEN);

                    let body_len = rpc_body_len(&header);
                    assert_eq!(body_len, body.len());
                    let amount = tls
                        .read_exact_or_eof(cx, &mut body[..body_len])
                        .expect("server TLS body read failed");
                    assert_eq!(amount, body_len);
                    assert!(body.iter().all(|&byte| byte == 0x5a));

                    tls.write(cx, &response)
                        .expect("server TLS response write failed");
                    tls.shutdown(cx).expect("server TLS shutdown failed");
                    tls.close(cx).expect("server TLS close failed");
                });

                let client = scope.spawn(move |cx| {
                    let mut tls = client_ctx
                        .client(cx, TLS_BUFFER_SIZE, client_fd, "localhost")
                        .expect("client TLS handshake failed");
                    tls.write(cx, &header)
                        .expect("client TLS header write failed");
                    tls.write(cx, &body).expect("client TLS body write failed");

                    let mut received = [0_u8; RPC_RESPONSE_LEN];
                    let amount = tls
                        .read_exact_or_eof(cx, &mut received)
                        .expect("client TLS response read failed");
                    assert_eq!(amount, RPC_RESPONSE_LEN);
                    tls.shutdown(cx).expect("client TLS shutdown failed");
                    tls.close(cx).expect("client TLS close failed");
                    received
                });

                server.join(cx);
                let received = client.join(cx);
                assert_eq!(received, response);
            });
        });
    }
}
