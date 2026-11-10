// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//! TlsStream brings together uringtls library and an
//! asynchronous stream to implement TLS over the stream.
//!
//! `TlsStream::server` creates a TLS "server" socket.
//! `TlsStream::client` creates a TLS "client" socket.
//!
//! TODO: implement client authentication
//!
use crate::tlscontext::as_io_error;
use crate::{
    AsyncLock, AsyncStreamRead, AsyncStreamWrite, BufferResult, CanceledError, Errno, OwnedFd,
    SplittableStream, operations, try_clone_owned_fd,
};
use kimojio_tls::{TlsServer, TlsServerShared};
use std::borrow::Borrow;
use std::rc::Rc;
use std::time::Instant;

/// A TLS stream bound to one runtime thread.
pub struct TlsStream {
    ssl: TlsServerShared,
    socket: Option<Rc<OwnedFd>>,
}

trait SocketPair {
    fn read_socket(
        &self,
    ) -> impl Future<Output = Result<impl Borrow<Option<Rc<OwnedFd>>>, CanceledError>>;
    fn write_socket(
        &self,
    ) -> impl Future<Output = Result<impl Borrow<Option<Rc<OwnedFd>>>, CanceledError>>;
}

impl SocketPair for Option<Rc<OwnedFd>> {
    async fn read_socket(&self) -> Result<impl Borrow<Option<Rc<OwnedFd>>>, CanceledError> {
        Ok(self)
    }

    async fn write_socket(&self) -> Result<impl Borrow<Option<Rc<OwnedFd>>>, CanceledError> {
        Ok(self)
    }
}

struct SharedSocketPair {
    read_socket: AsyncLock<Option<Rc<OwnedFd>>>,
    write_socket: AsyncLock<Option<Rc<OwnedFd>>>,
}

impl SocketPair for Rc<SharedSocketPair> {
    fn read_socket(
        &self,
    ) -> impl Future<Output = Result<impl Borrow<Option<Rc<OwnedFd>>>, CanceledError>> {
        self.read_socket.lock()
    }

    fn write_socket(
        &self,
    ) -> impl Future<Output = Result<impl Borrow<Option<Rc<OwnedFd>>>, CanceledError>> {
        self.write_socket.lock()
    }
}

pub struct TlsReadStream {
    ssl: TlsServerShared,
    socket: Rc<SharedSocketPair>,
}

pub struct TlsWriteStream {
    ssl: TlsServerShared,
    socket: Rc<SharedSocketPair>,
}

impl TlsStream {
    /// Creates an instance of TlsStream.
    pub fn new_tlsstream(ssl: TlsServer, socket: OwnedFd) -> TlsStream {
        let socket = Some(Rc::new(socket));
        let (ssl, _) = ssl.split();
        TlsStream { ssl, socket }
    }

    /// Performs the client side of TLS handshake.
    pub async fn client_side_handshake(&mut self, deadline: Option<Instant>) -> Result<(), Errno> {
        loop {
            let response = self.ssl.client_side_handshake();
            match response {
                kimojio_tls::Response::Success(_) => return Ok(()),
                kimojio_tls::Response::Fail(e) => {
                    handle_tls_error(&self.socket, "client_side_handshake", e).await?
                }
                kimojio_tls::Response::WantRead => {
                    try_read(&mut self.ssl, &self.socket, deadline).await?
                }
                kimojio_tls::Response::WantWrite => {
                    try_write(&mut self.ssl, &self.socket, deadline).await?
                }
                kimojio_tls::Response::Eof => return Ok(()),
            }
        }
    }

    /// Performs the server side of TLS handshake.
    pub async fn server_side_handshake(&mut self, deadline: Option<Instant>) -> Result<(), Errno> {
        loop {
            let response = self.ssl.server_side_handshake();
            match response {
                kimojio_tls::Response::Success(_) => return Ok(()),
                kimojio_tls::Response::Fail(e) => {
                    handle_tls_error(&self.socket, "server_side_handshake", e).await?
                }
                kimojio_tls::Response::WantRead => {
                    try_read(&mut self.ssl, &self.socket, deadline).await?
                }
                kimojio_tls::Response::WantWrite => {
                    try_write(&mut self.ssl, &self.socket, deadline).await?
                }
                kimojio_tls::Response::Eof => {
                    return Err(Errno::from_raw_os_error(crate::EPROTO));
                }
            }
        }
    }

    /// Gets the SSL object reference.
    pub fn get_ssl(&self) -> &openssl::ssl::SslRef {
        self.ssl.get_ssl()
    }
}

impl SplittableStream for TlsStream {
    type ReadStream = TlsReadStream;
    type WriteStream = TlsWriteStream;

    async fn split(self) -> Result<(TlsReadStream, TlsWriteStream), Errno> {
        let read_socket = if let Some(socket) = self.socket {
            let read_socket = Rc::new(try_clone_owned_fd(&socket)?);
            Rc::new(SharedSocketPair {
                read_socket: AsyncLock::new(Some(read_socket)),
                write_socket: AsyncLock::new(Some(socket)),
            })
        } else {
            Rc::new(SharedSocketPair {
                read_socket: AsyncLock::new(None),
                write_socket: AsyncLock::new(None),
            })
        };
        let write_socket = read_socket.clone();
        let read_ssl = self.ssl.clone();
        let write_ssl = self.ssl;
        Ok((
            TlsReadStream {
                ssl: read_ssl,
                socket: read_socket,
            },
            TlsWriteStream {
                ssl: write_ssl,
                socket: write_socket,
            },
        ))
    }
}

async fn try_read(
    ssl: &mut TlsServerShared,
    socket: &impl SocketPair,
    deadline: Option<Instant>,
) -> Result<(), Errno> {
    let socket = socket.read_socket().await?;
    if let Some(socket) = socket.borrow() {
        let lease = loop {
            match ssl.owned_write_lease() {
                Ok(lease) => break lease,
                Err(Errno::BUSY) => operations::yield_io().await,
                Err(error) => return Err(error),
            }
        };
        if let Some(lease) = lease {
            let (result, lease) =
                operations::read_owned_with_deadline(socket.clone(), lease, deadline).await;
            let amount = result?;
            if amount == 0 {
                return Err(Errno::from_raw_os_error(libc::EPIPE));
            }
            lease.commit(amount);
        }
        Ok(())
    } else {
        Err(Errno::from_raw_os_error(libc::EPIPE))
    }
}

async fn try_write(
    ssl: &mut TlsServerShared,
    socket: &impl SocketPair,
    deadline: Option<Instant>,
) -> Result<(), Errno> {
    let socket = socket.write_socket().await?;
    if let Some(socket) = socket.borrow() {
        // Keep writing until the pull buffer is exhausted. write_internal() advances
        // buffer by the length of the full buffer copied into the BIO rather than
        // the amount of data written to the socket here. Need to fully exhaust
        // the BIO here to keep the bookkeeping in sync
        loop {
            let lease = match ssl.owned_read_lease() {
                Ok(Some(lease)) => lease,
                Ok(None) => break,
                Err(Errno::BUSY) => {
                    operations::yield_io().await;
                    continue;
                }
                Err(error) => return Err(error),
            };
            let (result, lease) =
                operations::write_owned_with_deadline(socket.clone(), lease, deadline).await;
            let amount = result?;
            if amount == 0 {
                return Err(Errno::from_raw_os_error(libc::EPIPE));
            }
            lease.consume(amount);
        }
        Ok(())
    } else {
        Err(Errno::from_raw_os_error(libc::EPIPE))
    }
}

// write_internal does not flush the encrypted buffer.  This allows
// multiple write_internal calls to accumulate data for a single write
// to the wire.
async fn write_internal(
    ssl: &mut TlsServerShared,
    socket: &impl SocketPair,
    mut buffer: &[u8],
    deadline: Option<Instant>,
) -> Result<(), Errno> {
    while !buffer.is_empty() {
        match ssl.write(buffer) {
            kimojio_tls::Response::Success(amount) => {
                buffer = &buffer[amount..];
            }
            kimojio_tls::Response::Fail(e) => handle_tls_error(socket, "write_internal", e).await?,
            kimojio_tls::Response::WantRead => try_read(ssl, socket, deadline).await?,
            kimojio_tls::Response::WantWrite => try_write(ssl, socket, deadline).await?,
            kimojio_tls::Response::Eof => return Err(Errno::from_raw_os_error(libc::EPIPE)),
        }
    }
    Ok(())
}

pub fn tls_overhead(buffer_size: usize) -> usize {
    // The minimum frame size is zero, and maximum is 16k. Observed value
    // seems to average 6k in some cases. We will use 1024 as a likely lower
    // bound as a tradeoff between keeping the buffer trim and avoiding
    // needing to split writes.
    const TLS_FRAME_LENGTH: usize = 1024;
    const MAX_TLS_HEADER_LENGTH: usize = 40;
    buffer_size.div_ceil(TLS_FRAME_LENGTH) * MAX_TLS_HEADER_LENGTH
}

async fn handle_tls_error(
    socket: &impl SocketPair,
    _message: &str,
    e: kimojio_tls::TlsServerError,
) -> Result<(), Errno> {
    let read_socket = socket.read_socket().await?;
    let write_socket = socket.write_socket().await?;

    if read_socket.borrow().is_some() && write_socket.borrow().is_some() {
        Err(as_io_error(e))
    } else {
        Err(Errno::from_raw_os_error(libc::EPIPE))
    }
}

async fn try_read_impl(
    ssl: &mut TlsServerShared,
    socket: &impl SocketPair,
    buffer: &mut [u8],
    deadline: Option<Instant>,
) -> Result<usize, Errno> {
    loop {
        match ssl.read(buffer) {
            kimojio_tls::Response::Success(amount) => return Ok(amount),
            kimojio_tls::Response::Fail(e) => handle_tls_error(socket, "try_read", e).await?,
            kimojio_tls::Response::WantRead => try_read(ssl, socket, deadline).await?,
            kimojio_tls::Response::WantWrite => try_write(ssl, socket, deadline).await?,
            kimojio_tls::Response::Eof => return Ok(0),
        }
    }
}

async fn read_impl(
    ssl: &mut TlsServerShared,
    socket: &impl SocketPair,
    mut buffer: &mut [u8],
    deadline: Option<Instant>,
) -> Result<(), Errno> {
    while !buffer.is_empty() {
        let amount = try_read_impl(ssl, socket, buffer, deadline).await?;
        if amount == 0 {
            return Err(Errno::PIPE);
        }
        buffer = &mut buffer[amount..];
    }
    Ok(())
}

async fn writev_impl<B: AsRef<[u8]>>(
    ssl: &mut TlsServerShared,
    socket: &impl SocketPair,
    buffers: &[B],
    deadline: Option<Instant>,
) -> Result<(), Errno> {
    for buffer in buffers {
        write_internal(ssl, socket, buffer.as_ref(), deadline).await?;
    }
    // flush
    try_write(ssl, socket, deadline).await
}

async fn write_impl(
    ssl: &mut TlsServerShared,
    socket: &impl SocketPair,
    buffer: &[u8],
    deadline: Option<Instant>,
) -> Result<(), Errno> {
    write_internal(ssl, socket, buffer, deadline).await?;
    // flush
    try_write(ssl, socket, deadline).await
}

async fn shutdown_impl(ssl: &mut TlsServerShared, socket: &impl SocketPair) -> Result<(), Errno> {
    loop {
        match ssl.shutdown() {
            kimojio_tls::Response::Success(_) => return Ok(()),
            kimojio_tls::Response::Fail(e) => handle_tls_error(socket, "read", e).await?,
            kimojio_tls::Response::WantRead => {
                // Graceful shutdown returns 0 byte read on EOF.
                if let Err(Errno::PIPE) = try_read(ssl, socket, None).await {
                    return Ok(());
                }
            }
            kimojio_tls::Response::WantWrite => try_write(ssl, socket, None).await?,
            kimojio_tls::Response::Eof => return Ok(()),
        }
    }
}

async fn close_impl<T>(
    socket: &mut Option<Rc<OwnedFd>>,
    cause: Result<T, Errno>,
) -> Result<T, Errno> {
    if let Some(socket) = socket.take() {
        operations::close_shared_fd(socket).await?;
        cause
    } else {
        Err(Errno::from_raw_os_error(libc::EPIPE))
    }
}

impl AsyncStreamRead for TlsStream {
    async fn try_read<B: AsMut<[u8]> + 'static>(
        &mut self,
        mut buffer: B,
        deadline: Option<Instant>,
    ) -> BufferResult<usize, B> {
        let result = try_read_impl(&mut self.ssl, &self.socket, buffer.as_mut(), deadline).await;
        let result = if result.is_err() {
            close_impl(&mut self.socket, result).await
        } else {
            result
        };
        (result, buffer)
    }

    async fn read<B: AsMut<[u8]> + 'static>(
        &mut self,
        mut buffer: B,
        deadline: Option<Instant>,
    ) -> BufferResult<(), B> {
        let result = read_impl(&mut self.ssl, &self.socket, buffer.as_mut(), deadline).await;
        let result = if result.is_err() {
            close_impl(&mut self.socket, result).await
        } else {
            result
        };
        (result, buffer)
    }
}

impl AsyncStreamRead for TlsReadStream {
    async fn try_read<B: AsMut<[u8]> + 'static>(
        &mut self,
        mut buffer: B,
        deadline: Option<Instant>,
    ) -> BufferResult<usize, B> {
        let result = try_read_impl(&mut self.ssl, &self.socket, buffer.as_mut(), deadline).await;
        let result = if result.is_err() {
            close_read_because(&self.socket, result).await
        } else {
            result
        };
        (result, buffer)
    }

    async fn read<B: AsMut<[u8]> + 'static>(
        &mut self,
        mut buffer: B,
        deadline: Option<Instant>,
    ) -> BufferResult<(), B> {
        let result = read_impl(&mut self.ssl, &self.socket, buffer.as_mut(), deadline).await;
        let result = if result.is_err() {
            close_read_because(&self.socket, result).await
        } else {
            result
        };
        (result, buffer)
    }
}

impl AsyncStreamWrite for TlsStream {
    async fn writev<B: AsRef<[u8]> + 'static>(
        &mut self,
        buffers: Vec<B>,
        deadline: Option<Instant>,
    ) -> BufferResult<(), Vec<B>> {
        let result = writev_impl(&mut self.ssl, &self.socket, &buffers, deadline).await;
        let result = if result.is_err() {
            close_impl(&mut self.socket, result).await
        } else {
            result
        };
        (result, buffers)
    }

    async fn write<B: AsRef<[u8]> + 'static>(
        &mut self,
        buffer: B,
        deadline: Option<Instant>,
    ) -> BufferResult<(), B> {
        let result = write_impl(&mut self.ssl, &self.socket, buffer.as_ref(), deadline).await;
        let result = if result.is_err() {
            close_impl(&mut self.socket, result).await
        } else {
            result
        };
        (result, buffer)
    }

    async fn shutdown(&mut self) -> Result<(), Errno> {
        if let Err(e) = shutdown_impl(&mut self.ssl, &self.socket).await {
            close_impl(&mut self.socket, Err(e)).await?;
            Err(e)
        } else {
            Ok(())
        }
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Errno>> {
        close_impl(&mut self.socket, Ok(()))
    }
}

async fn close_read_because<T>(
    socket: &SharedSocketPair,
    cause: Result<T, Errno>,
) -> Result<T, Errno> {
    let read_socket = socket.read_socket.lock().await?.take();

    if let Some(read_socket) = read_socket {
        operations::close_shared_fd(read_socket).await?;
        cause
    } else {
        Err(Errno::from_raw_os_error(libc::EPIPE))
    }
}

async fn close_write_because<T>(
    socket: &SharedSocketPair,
    cause: Result<T, Errno>,
) -> Result<T, Errno> {
    let write_socket = socket.write_socket.lock().await?.take();

    if let Some(write_socket) = write_socket {
        operations::close_shared_fd(write_socket).await?;
        cause
    } else {
        Err(Errno::from_raw_os_error(libc::EPIPE))
    }
}

impl AsyncStreamWrite for TlsWriteStream {
    async fn writev<B: AsRef<[u8]> + 'static>(
        &mut self,
        buffers: Vec<B>,
        deadline: Option<Instant>,
    ) -> BufferResult<(), Vec<B>> {
        let result = writev_impl(&mut self.ssl, &self.socket, &buffers, deadline).await;
        let result = if result.is_err() {
            close_write_because(&self.socket, result).await
        } else {
            result
        };
        (result, buffers)
    }

    async fn write<B: AsRef<[u8]> + 'static>(
        &mut self,
        buffer: B,
        deadline: Option<Instant>,
    ) -> BufferResult<(), B> {
        let result = write_impl(&mut self.ssl, &self.socket, buffer.as_ref(), deadline).await;
        let result = if result.is_err() {
            close_write_because(&self.socket, result).await
        } else {
            result
        };
        (result, buffer)
    }

    async fn shutdown(&mut self) -> Result<(), Errno> {
        let result = shutdown_impl(&mut self.ssl, &self.socket).await;
        if result.is_err() {
            close_write_because(&self.socket, result).await
        } else {
            result
        }
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Errno>> {
        close_write_because(&self.socket, Ok(()))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::tlscontext::test::test_utils;

    async fn tls_pair() -> (TlsStream, TlsStream) {
        let certs = test_utils::setup_default_certs().unwrap();
        let server_context = test_utils::make_test_server_ctx(
            certs.server_cert_name.as_c_str(),
            certs.server_key_name.as_c_str(),
            certs.ca_cert_name.as_c_str(),
            None,
        )
        .unwrap();
        let client_context = test_utils::make_test_client_ctx(
            certs.client_cert_name.as_c_str(),
            certs.client_key_name.as_c_str(),
            certs.ca_cert_name.as_c_str(),
            None,
        )
        .unwrap();
        let (client, server) = crate::pipe::bipipe();
        let server = operations::spawn_task(async move {
            server_context.server(16384, server, None).await.unwrap()
        });
        let mut client = client_context.client(16384, client, None).await.unwrap();
        // Flush the client Finished flight before waiting for the server.
        client.write(Vec::<u8>::new(), None).await.0.unwrap();
        (client, server.await.unwrap())
    }

    #[crate::test]
    async fn exact_read_eof_returns_the_buffer_and_closes_the_read_descriptor() {
        for split in [false, true] {
            let (mut client, mut server) = tls_pair().await;
            let shutdown = operations::spawn_task(async move {
                server.shutdown().await.unwrap();
            });
            if split {
                let (mut reader, mut writer) = client.split().await.unwrap();
                let descriptor = {
                    let socket = reader.socket.read_socket.lock().await.unwrap();
                    Rc::downgrade(socket.as_ref().unwrap())
                };
                let (result, buffer) = reader.read([7; 1], None).await;
                assert_eq!(result, Err(Errno::PIPE));
                assert_eq!(buffer, [7; 1]);
                assert!(descriptor.upgrade().is_none());
                writer.close().await.unwrap();
            } else {
                let descriptor = Rc::downgrade(client.socket.as_ref().unwrap());
                let (result, buffer) = client.read([7; 1], None).await;
                assert_eq!(result, Err(Errno::PIPE));
                assert_eq!(buffer, [7; 1]);
                assert!(descriptor.upgrade().is_none());
            }
            shutdown.await.unwrap();
        }
    }
}
