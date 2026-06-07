// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! OpenSSL TLS streams for `kimojio-stack`.
//!
//! This crate adapts the existing Kimojio OpenSSL memory-BIO integration to the
//! stackful runtime. OpenSSL reads encrypted bytes directly from an internal BIO
//! buffer returned by `kimojio-tls`; the runtime fills that buffer with
//! `RuntimeContext::read`. OpenSSL writes encrypted bytes into another BIO
//! buffer, and the runtime drains it with `RuntimeContext::write`. That keeps the
//! OS-thread nonblocking without adding an intermediate copy between the socket
//! and OpenSSL's encrypted buffers.
//!
//! See `docs/OFFLOAD.md` for the experimental TLS CPU offload design.

use std::{
    ffi::c_void,
    mem,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
        mpsc,
    },
    thread,
};

use foreign_types_shared::{ForeignType, ForeignTypeRef};
use kimojio_stack::{Errno, FutexWaitFlags, RuntimeContext};
use kimojio_tls::{Response, TlsServer, TlsServerContext, TlsServerError};
use rustix::{
    fd::OwnedFd,
    net::Shutdown,
    thread::futex::{self, Flags as FutexFlags},
};

const OFFLOAD_PENDING: u32 = 0;
const OFFLOAD_DONE: u32 = 1;

/// A TLS context backed by an OpenSSL `SSL_CTX`.
///
/// Construct one with [`TlsContext::from_openssl`] using the Rust `openssl`
/// crate's `SslContext` values, such as those produced by `SslConnector` or
/// `SslAcceptor`.
pub struct TlsContext {
    ssl_ctx: TlsServerContext,
}

impl TlsContext {
    /// Creates a stack TLS context from an OpenSSL crate context.
    ///
    /// Ownership of the OpenSSL context is transferred into this type.
    pub fn from_openssl(ctx: openssl::ssl::SslContext) -> Self {
        let ssl_ctx = TlsServerContext::from_raw(ctx.as_ptr() as *mut c_void);
        mem::forget(ctx);
        Self { ssl_ctx }
    }

    /// Performs a server-side TLS handshake over `socket`.
    pub fn server(
        &self,
        cx: &RuntimeContext<'_>,
        bufsize: usize,
        socket: OwnedFd,
    ) -> Result<TlsStream, Errno> {
        let ssl = self.ssl_ctx.server(bufsize).map_err(as_io_error)?;
        let mut stream = TlsStream::new(ssl, socket);
        stream.server_side_handshake(cx)?;
        Ok(stream)
    }

    /// Performs a server-side TLS handshake and enables CPU offload for data I/O.
    pub fn server_with_offload(
        &self,
        cx: &RuntimeContext<'_>,
        bufsize: usize,
        socket: OwnedFd,
        offload: TlsOffloadConfig,
    ) -> Result<TlsStream, Errno> {
        let ssl = self.ssl_ctx.server(bufsize).map_err(as_io_error)?;
        let mut stream = TlsStream::with_offload(ssl, socket, offload);
        stream.server_side_handshake(cx)?;
        Ok(stream)
    }

    /// Performs a client-side TLS handshake over `socket`.
    pub fn client(
        &self,
        cx: &RuntimeContext<'_>,
        bufsize: usize,
        socket: OwnedFd,
    ) -> Result<TlsStream, Errno> {
        let ssl = self.ssl_ctx.client(bufsize).map_err(as_io_error)?;
        let mut stream = TlsStream::new(ssl, socket);
        stream.client_side_handshake(cx)?;
        Ok(stream)
    }

    /// Performs a client-side TLS handshake and enables CPU offload for data I/O.
    pub fn client_with_offload(
        &self,
        cx: &RuntimeContext<'_>,
        bufsize: usize,
        socket: OwnedFd,
        offload: TlsOffloadConfig,
    ) -> Result<TlsStream, Errno> {
        let ssl = self.ssl_ctx.client(bufsize).map_err(as_io_error)?;
        let mut stream = TlsStream::with_offload(ssl, socket, offload);
        stream.client_side_handshake(cx)?;
        Ok(stream)
    }
}

/// Pool used by TLS streams to run large OpenSSL read/write calls off-thread.
#[derive(Clone)]
pub struct TlsOffloadPool {
    inner: Arc<TlsOffloadPoolInner>,
}

impl TlsOffloadPool {
    /// Starts `worker_count` offload worker threads.
    pub fn new(worker_count: usize) -> std::io::Result<Self> {
        if worker_count == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "TLS offload pool must have at least one worker",
            ));
        }

        let (sender, receiver) = mpsc::channel();
        let receiver = Arc::new(Mutex::new(receiver));
        let mut workers = Vec::with_capacity(worker_count);

        for worker in 0..worker_count {
            let receiver = Arc::clone(&receiver);
            match thread::Builder::new()
                .name(format!("kimojio-tls-offload-{worker}"))
                .spawn(move || run_offload_worker(receiver))
            {
                Ok(handle) => workers.push(handle),
                Err(error) => {
                    for _ in 0..workers.len() {
                        let _ = sender.send(OffloadJob::Shutdown);
                    }
                    for worker in workers {
                        let _ = worker.join();
                    }
                    return Err(error);
                }
            }
        }

        Ok(Self {
            inner: Arc::new(TlsOffloadPoolInner {
                sender,
                worker_count,
                workers: Mutex::new(workers),
            }),
        })
    }
}

struct TlsOffloadPoolInner {
    sender: mpsc::Sender<OffloadJob>,
    worker_count: usize,
    workers: Mutex<Vec<thread::JoinHandle<()>>>,
}

impl Drop for TlsOffloadPoolInner {
    fn drop(&mut self) {
        for _ in 0..self.worker_count {
            let _ = self.sender.send(OffloadJob::Shutdown);
        }

        let mut workers = self
            .workers
            .lock()
            .expect("TLS offload worker mutex poisoned");
        for worker in workers.drain(..) {
            let _ = worker.join();
        }
    }
}

/// Configuration for TLS CPU offload.
#[derive(Clone)]
pub struct TlsOffloadConfig {
    pool: TlsOffloadPool,
    min_read_size: usize,
    min_write_size: usize,
}

impl TlsOffloadConfig {
    /// Creates a config with offload disabled until thresholds are set.
    pub fn new(pool: TlsOffloadPool) -> Self {
        Self {
            pool,
            min_read_size: usize::MAX,
            min_write_size: usize::MAX,
        }
    }

    /// Creates a config that offloads every non-empty TLS data read and write.
    pub fn always(pool: TlsOffloadPool) -> Self {
        Self {
            pool,
            min_read_size: 1,
            min_write_size: 1,
        }
    }

    /// Sets the minimum caller buffer length for read/decrypt offload.
    pub fn with_read_threshold(mut self, min_read_size: usize) -> Self {
        self.min_read_size = min_read_size;
        self
    }

    /// Sets the minimum plaintext length for write/encrypt offload.
    pub fn with_write_threshold(mut self, min_write_size: usize) -> Self {
        self.min_write_size = min_write_size;
        self
    }
}

/// Counts how a [`TlsStream`] drove TLS data operations.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TlsOffloadStats {
    /// Number of TLS read/decrypt calls executed on the stack runtime thread.
    pub inline_reads: usize,
    /// Number of TLS write/encrypt calls executed on the stack runtime thread.
    pub inline_writes: usize,
    /// Number of TLS read/decrypt calls executed by the offload pool.
    pub offloaded_reads: usize,
    /// Number of TLS write/encrypt calls executed by the offload pool.
    pub offloaded_writes: usize,
}

/// A TLS stream driven by `kimojio-stack` I/O.
pub struct TlsStream {
    ssl: Option<TlsServer>,
    socket: Option<OwnedFd>,
    offload: Option<TlsOffloadConfig>,
    offload_stats: TlsOffloadStats,
}

impl TlsStream {
    /// Creates a TLS stream from an initialized TLS handle and connected socket.
    pub fn new(ssl: TlsServer, socket: OwnedFd) -> Self {
        Self {
            ssl: Some(ssl),
            socket: Some(socket),
            offload: None,
            offload_stats: TlsOffloadStats::default(),
        }
    }

    /// Creates a TLS stream with CPU offload enabled for data I/O.
    pub fn with_offload(ssl: TlsServer, socket: OwnedFd, offload: TlsOffloadConfig) -> Self {
        Self {
            ssl: Some(ssl),
            socket: Some(socket),
            offload: Some(offload),
            offload_stats: TlsOffloadStats::default(),
        }
    }

    /// Enables or replaces CPU offload for data I/O.
    pub fn set_offload(&mut self, offload: TlsOffloadConfig) {
        self.offload = Some(offload);
    }

    /// Disables CPU offload for data I/O.
    pub fn clear_offload(&mut self) {
        self.offload = None;
    }

    /// Returns counters for inline and offloaded TLS data operations.
    pub fn offload_stats(&self) -> TlsOffloadStats {
        self.offload_stats
    }

    /// Gets the OpenSSL `SslRef` for inspection.
    pub fn ssl(&self) -> &openssl::ssl::SslRef {
        let raw_ssl = self
            .ssl
            .as_ref()
            .expect("TLS handle is temporarily offloaded")
            .get_ssl_raw();
        // SAFETY: `raw_ssl` is owned by `self.ssl` and remains valid for `self`.
        unsafe { openssl::ssl::SslRef::from_ptr(raw_ssl as *mut _) }
    }

    /// Performs the client side of the TLS handshake.
    pub fn client_side_handshake(&mut self, cx: &RuntimeContext<'_>) -> Result<(), Errno> {
        loop {
            match self.ssl_mut()?.client_side_handshake() {
                Response::Success(_) => return self.flush_tls_write(cx),
                Response::Fail(e) => return Err(self.handle_tls_error(e)),
                Response::WantRead => self.fill_tls_read(cx)?,
                Response::WantWrite => self.flush_tls_write(cx)?,
                Response::Eof => return Ok(()),
            }
        }
    }

    /// Performs the server side of the TLS handshake.
    pub fn server_side_handshake(&mut self, cx: &RuntimeContext<'_>) -> Result<(), Errno> {
        loop {
            match self.ssl_mut()?.server_side_handshake() {
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
    pub fn read(&mut self, cx: &RuntimeContext<'_>, buf: &mut [u8]) -> Result<usize, Errno> {
        loop {
            match self.read_tls(cx, buf)? {
                Response::Success(amount) => return Ok(amount),
                Response::Fail(e) => return Err(self.handle_tls_error(e)),
                Response::WantRead => self.fill_tls_read(cx)?,
                Response::WantWrite => self.flush_tls_write(cx)?,
                Response::Eof => return Ok(0),
            }
        }
    }

    /// Fills `buf` with decrypted TLS plaintext unless EOF is reached first.
    pub fn read_exact_or_eof(
        &mut self,
        cx: &RuntimeContext<'_>,
        mut buf: &mut [u8],
    ) -> Result<usize, Errno> {
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
    pub fn write(&mut self, cx: &RuntimeContext<'_>, mut buf: &[u8]) -> Result<usize, Errno> {
        let requested = buf.len();
        while !buf.is_empty() {
            match self.write_tls(cx, buf)? {
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
    pub fn shutdown(&mut self, cx: &RuntimeContext<'_>) -> Result<(), Errno> {
        loop {
            match self.ssl_mut()?.shutdown() {
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
    pub fn shutdown_write(&self, cx: &RuntimeContext<'_>) -> Result<(), Errno> {
        let socket = self.socket.as_ref().ok_or(Errno::PIPE)?;
        cx.shutdown(socket, Shutdown::Write)
    }

    /// Closes the underlying socket through the stack runtime.
    pub fn close(mut self, cx: &RuntimeContext<'_>) -> Result<(), Errno> {
        let socket = self.socket.take().ok_or(Errno::PIPE)?;
        cx.close(socket)
    }

    fn fill_tls_read(&mut self, cx: &RuntimeContext<'_>) -> Result<(), Errno> {
        let socket = self.socket.as_ref().ok_or(Errno::PIPE)?;
        let ssl = self.ssl.as_mut().ok_or(Errno::PIPE)?;
        if let Some(buffer) = ssl.get_push_buffer() {
            let amount = cx.read(socket, buffer)?;
            if amount == 0 {
                return Err(Errno::PIPE);
            }
            ssl.use_push_buffer(amount);
        }
        Ok(())
    }

    fn flush_tls_write(&mut self, cx: &RuntimeContext<'_>) -> Result<(), Errno> {
        let socket = self.socket.as_ref().ok_or(Errno::PIPE)?;
        let ssl = self.ssl.as_mut().ok_or(Errno::PIPE)?;
        loop {
            let amount = match ssl.get_pull_buffer() {
                Some(buffer) => {
                    let amount = cx.write(socket, buffer)?;
                    if amount == 0 {
                        return Err(Errno::PIPE);
                    }
                    amount
                }
                None => return Ok(()),
            };
            ssl.use_pull_buffer(amount);
        }
    }

    fn read_tls(&mut self, cx: &RuntimeContext<'_>, buf: &mut [u8]) -> Result<Response, Errno> {
        if self.should_offload_read(buf.len()) {
            self.offload_read(cx, buf)
        } else {
            self.offload_stats.inline_reads += 1;
            Ok(self.ssl_mut()?.read(buf))
        }
    }

    fn write_tls(&mut self, cx: &RuntimeContext<'_>, buf: &[u8]) -> Result<Response, Errno> {
        if self.should_offload_write(buf.len()) {
            self.offload_write(cx, buf)
        } else {
            self.offload_stats.inline_writes += 1;
            Ok(self.ssl_mut()?.write(buf))
        }
    }

    fn should_offload_read(&self, len: usize) -> bool {
        len != 0
            && self
                .offload
                .as_ref()
                .is_some_and(|offload| len >= offload.min_read_size)
    }

    fn should_offload_write(&self, len: usize) -> bool {
        len != 0
            && self
                .offload
                .as_ref()
                .is_some_and(|offload| len >= offload.min_write_size)
    }

    fn offload_read(&mut self, cx: &RuntimeContext<'_>, buf: &mut [u8]) -> Result<Response, Errno> {
        let sender = self
            .offload
            .as_ref()
            .ok_or(Errno::PIPE)?
            .pool
            .inner
            .sender
            .clone();
        let ssl = self.take_ssl()?;
        let completion = Arc::new(OffloadState::new());
        let job = OffloadJob::Read {
            ssl,
            len: buf.len(),
            completion: Arc::clone(&completion),
        };

        if let Err(error) = sender.send(job) {
            if let OffloadJob::Read { ssl, .. } = error.0 {
                self.restore_ssl(ssl);
            }
            return Err(Errno::PIPE);
        }

        let result = completion.wait(cx);
        self.restore_ssl(result.ssl);
        self.offload_stats.offloaded_reads += 1;

        if let Response::Success(amount) = result.response {
            buf[..amount].copy_from_slice(&result.plaintext[..amount]);
            Ok(Response::Success(amount))
        } else {
            Ok(result.response)
        }
    }

    fn offload_write(&mut self, cx: &RuntimeContext<'_>, buf: &[u8]) -> Result<Response, Errno> {
        let sender = self
            .offload
            .as_ref()
            .ok_or(Errno::PIPE)?
            .pool
            .inner
            .sender
            .clone();
        let ssl = self.take_ssl()?;
        let completion = Arc::new(OffloadState::new());
        let job = OffloadJob::Write {
            ssl,
            plaintext: buf.to_vec(),
            completion: Arc::clone(&completion),
        };

        if let Err(error) = sender.send(job) {
            if let OffloadJob::Write { ssl, .. } = error.0 {
                self.restore_ssl(ssl);
            }
            return Err(Errno::PIPE);
        }

        let result = completion.wait(cx);
        self.restore_ssl(result.ssl);
        self.offload_stats.offloaded_writes += 1;
        Ok(result.response)
    }

    fn ssl_mut(&mut self) -> Result<&mut TlsServer, Errno> {
        self.ssl.as_mut().ok_or(Errno::PIPE)
    }

    fn take_ssl(&mut self) -> Result<TlsServer, Errno> {
        self.ssl.take().ok_or(Errno::PIPE)
    }

    fn restore_ssl(&mut self, ssl: TlsServer) {
        debug_assert!(self.ssl.is_none());
        self.ssl = Some(ssl);
    }

    fn handle_tls_error(&self, error: TlsServerError) -> Errno {
        if self.socket.is_some() {
            as_io_error(error)
        } else {
            Errno::PIPE
        }
    }
}

enum OffloadJob {
    Read {
        ssl: TlsServer,
        len: usize,
        completion: Arc<OffloadState<ReadOffloadResult>>,
    },
    Write {
        ssl: TlsServer,
        plaintext: Vec<u8>,
        completion: Arc<OffloadState<WriteOffloadResult>>,
    },
    Shutdown,
}

struct ReadOffloadResult {
    ssl: TlsServer,
    response: Response,
    plaintext: Vec<u8>,
}

struct WriteOffloadResult {
    ssl: TlsServer,
    response: Response,
}

struct OffloadState<T> {
    futex: AtomicU32,
    result: Mutex<Option<T>>,
}

impl<T> OffloadState<T> {
    fn new() -> Self {
        Self {
            futex: AtomicU32::new(OFFLOAD_PENDING),
            result: Mutex::new(None),
        }
    }

    fn complete(&self, result: T) {
        *self
            .result
            .lock()
            .expect("TLS offload result mutex poisoned") = Some(result);
        self.futex.store(OFFLOAD_DONE, Ordering::Release);
        let _ = futex::wake(&self.futex, FutexFlags::PRIVATE, 1);
    }

    fn wait(&self, cx: &RuntimeContext<'_>) -> T {
        if cx.supports_io_uring_opcode(rustix_uring::opcode::FutexWait::CODE) {
            while self.futex.load(Ordering::Acquire) == OFFLOAD_PENDING {
                match cx.futex_wait(
                    &self.futex,
                    OFFLOAD_PENDING.into(),
                    u64::from(u32::MAX),
                    FutexWaitFlags::SIZE_U32 | FutexWaitFlags::PRIVATE,
                ) {
                    Ok(()) | Err(Errno::AGAIN) | Err(Errno::INTR) => {}
                    Err(_) => self.yield_until_ready(cx),
                }
            }
        } else {
            self.yield_until_ready(cx);
        }

        self.result
            .lock()
            .expect("TLS offload result mutex poisoned")
            .take()
            .expect("TLS offload worker completed without a result")
    }

    fn yield_until_ready(&self, cx: &RuntimeContext<'_>) {
        while self.futex.load(Ordering::Acquire) == OFFLOAD_PENDING {
            cx.yield_now();
        }
    }
}

fn run_offload_worker(receiver: Arc<Mutex<mpsc::Receiver<OffloadJob>>>) {
    loop {
        let job = {
            let receiver = receiver
                .lock()
                .expect("TLS offload receiver mutex poisoned");
            receiver.recv()
        };

        match job {
            Ok(OffloadJob::Read {
                mut ssl,
                len,
                completion,
            }) => {
                let mut plaintext = vec![0_u8; len];
                let response = ssl.read(&mut plaintext);
                completion.complete(ReadOffloadResult {
                    ssl,
                    response,
                    plaintext,
                });
            }
            Ok(OffloadJob::Write {
                mut ssl,
                plaintext,
                completion,
            }) => {
                let response = ssl.write(&plaintext);
                completion.complete(WriteOffloadResult { ssl, response });
            }
            Ok(OffloadJob::Shutdown) | Err(_) => return,
        }
    }
}

fn as_io_error(error: TlsServerError) -> Errno {
    match error {
        TlsServerError::Errno(errno) => errno,
        TlsServerError::TlsError(_) => Errno::PROTO,
    }
}

#[cfg(test)]
mod tests {
    use std::mem::size_of;

    use kimojio_stack::Runtime;
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
                        .client(cx, TLS_BUFFER_SIZE, client_fd)
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

                server.join(cx).unwrap();
                let echoed = client.join(cx).unwrap();
                assert_eq!(echoed, message);
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
                        .client(cx, TLS_BUFFER_SIZE, client_fd)
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

                server.join(cx).unwrap();
                let received = client.join(cx).unwrap();
                assert_eq!(received, response);
            });
        });
    }

    #[test]
    fn tls_offload_always_round_trips_large_body() {
        let (server_ctx, client_ctx) = make_contexts();
        let (client_fd, server_fd) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::empty(),
            None,
        )
        .unwrap();

        let pool = TlsOffloadPool::new(2).unwrap();
        let server_offload = TlsOffloadConfig::always(pool.clone());
        let client_offload = TlsOffloadConfig::always(pool);
        let body = vec![0x42; 32 * 1024];
        let expected = body.clone();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut tls = server_ctx
                        .server_with_offload(cx, TLS_BUFFER_SIZE, server_fd, server_offload)
                        .expect("server TLS handshake failed");
                    let mut received = vec![0_u8; expected.len()];
                    let amount = tls
                        .read_exact_or_eof(cx, &mut received)
                        .expect("server TLS body read failed");
                    assert_eq!(amount, expected.len());
                    assert_eq!(received, expected);

                    tls.write(cx, &received)
                        .expect("server TLS echo write failed");
                    tls.shutdown(cx).expect("server TLS shutdown failed");
                    let stats = tls.offload_stats();
                    tls.close(cx).expect("server TLS close failed");
                    stats
                });

                let client = scope.spawn(move |cx| {
                    let mut tls = client_ctx
                        .client_with_offload(cx, TLS_BUFFER_SIZE, client_fd, client_offload)
                        .expect("client TLS handshake failed");
                    tls.write(cx, &body).expect("client TLS body write failed");

                    let mut echoed = vec![0_u8; body.len()];
                    let amount = tls
                        .read_exact_or_eof(cx, &mut echoed)
                        .expect("client TLS echo read failed");
                    assert_eq!(amount, body.len());
                    assert_eq!(echoed, body);

                    tls.shutdown(cx).expect("client TLS shutdown failed");
                    let stats = tls.offload_stats();
                    tls.close(cx).expect("client TLS close failed");
                    stats
                });

                let server_stats = server.join(cx).unwrap();
                let client_stats = client.join(cx).unwrap();

                assert!(server_stats.offloaded_reads > 0);
                assert!(server_stats.offloaded_writes > 0);
                assert!(client_stats.offloaded_reads > 0);
                assert!(client_stats.offloaded_writes > 0);
            });
        });
    }

    #[test]
    fn tls_offload_threshold_keeps_small_io_inline() {
        let (server_ctx, client_ctx) = make_contexts();
        let (client_fd, server_fd) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::empty(),
            None,
        )
        .unwrap();

        let pool = TlsOffloadPool::new(1).unwrap();
        let body = vec![0x24; 1024];
        let threshold = body.len() + 1;
        let server_offload = TlsOffloadConfig::new(pool.clone())
            .with_read_threshold(threshold)
            .with_write_threshold(threshold);
        let client_offload = TlsOffloadConfig::new(pool)
            .with_read_threshold(threshold)
            .with_write_threshold(threshold);
        let expected = body.clone();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut tls = server_ctx
                        .server_with_offload(cx, TLS_BUFFER_SIZE, server_fd, server_offload)
                        .expect("server TLS handshake failed");
                    let mut received = vec![0_u8; expected.len()];
                    let amount = tls
                        .read_exact_or_eof(cx, &mut received)
                        .expect("server TLS body read failed");
                    assert_eq!(amount, expected.len());
                    assert_eq!(received, expected);

                    tls.write(cx, &received)
                        .expect("server TLS echo write failed");
                    tls.shutdown(cx).expect("server TLS shutdown failed");
                    let stats = tls.offload_stats();
                    tls.close(cx).expect("server TLS close failed");
                    stats
                });

                let client = scope.spawn(move |cx| {
                    let mut tls = client_ctx
                        .client_with_offload(cx, TLS_BUFFER_SIZE, client_fd, client_offload)
                        .expect("client TLS handshake failed");
                    tls.write(cx, &body).expect("client TLS body write failed");

                    let mut echoed = vec![0_u8; body.len()];
                    let amount = tls
                        .read_exact_or_eof(cx, &mut echoed)
                        .expect("client TLS echo read failed");
                    assert_eq!(amount, body.len());
                    assert_eq!(echoed, body);

                    tls.shutdown(cx).expect("client TLS shutdown failed");
                    let stats = tls.offload_stats();
                    tls.close(cx).expect("client TLS close failed");
                    stats
                });

                let server_stats = server.join(cx).unwrap();
                let client_stats = client.join(cx).unwrap();

                assert_eq!(server_stats.offloaded_reads, 0);
                assert_eq!(server_stats.offloaded_writes, 0);
                assert_eq!(client_stats.offloaded_reads, 0);
                assert_eq!(client_stats.offloaded_writes, 0);
                assert!(server_stats.inline_reads > 0);
                assert!(server_stats.inline_writes > 0);
                assert!(client_stats.inline_reads > 0);
                assert!(client_stats.inline_writes > 0);
            });
        });
    }
}
