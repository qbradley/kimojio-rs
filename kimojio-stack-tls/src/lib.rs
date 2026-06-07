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

use std::{ffi::c_void, mem};

use foreign_types_shared::{ForeignType, ForeignTypeRef};
use kimojio_stack::{Errno, RuntimeContext};
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
}

/// A TLS stream driven by `kimojio-stack` I/O.
pub struct TlsStream {
    ssl: TlsServer,
    socket: Option<OwnedFd>,
}

impl TlsStream {
    /// Creates a TLS stream from an initialized TLS handle and connected socket.
    pub fn new(ssl: TlsServer, socket: OwnedFd) -> Self {
        Self {
            ssl,
            socket: Some(socket),
        }
    }

    /// Gets the OpenSSL `SslRef` for inspection.
    pub fn ssl(&self) -> &openssl::ssl::SslRef {
        let raw_ssl = self.ssl.get_ssl_raw();
        // SAFETY: `raw_ssl` is owned by `self.ssl` and remains valid for `self`.
        unsafe { openssl::ssl::SslRef::from_ptr(raw_ssl as *mut _) }
    }

    /// Performs the client side of the TLS handshake.
    pub fn client_side_handshake(&mut self, cx: &RuntimeContext<'_>) -> Result<(), Errno> {
        loop {
            match self.ssl.client_side_handshake() {
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
    pub fn read(&mut self, cx: &RuntimeContext<'_>, buf: &mut [u8]) -> Result<usize, Errno> {
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
    pub fn shutdown(&mut self, cx: &RuntimeContext<'_>) -> Result<(), Errno> {
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
        if let Some(buffer) = self.ssl.get_push_buffer() {
            let amount = cx.read(socket, buffer)?;
            if amount == 0 {
                return Err(Errno::PIPE);
            }
            self.ssl.use_push_buffer(amount);
        }
        Ok(())
    }

    fn flush_tls_write(&mut self, cx: &RuntimeContext<'_>) -> Result<(), Errno> {
        let socket = self.socket.as_ref().ok_or(Errno::PIPE)?;
        loop {
            let amount = match self.ssl.get_pull_buffer() {
                Some(buffer) => {
                    let amount = cx.write(socket, buffer)?;
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
        if self.socket.is_some() {
            as_io_error(error)
        } else {
            Errno::PIPE
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
}
