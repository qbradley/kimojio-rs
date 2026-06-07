// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Direct OpenSSL TLS streams for `kimojio-stack`.
//!
//! This crate supplies a specialized OpenSSL BIO directly over `kimojio-stack`
//! I/O, so OpenSSL's BIO callbacks call [`RuntimeContext::read`] and
//! [`RuntimeContext::write`] without a separate memory BIO staging buffer.
//!
//! The runtime context is stored explicitly in the BIO state when the TLS stream
//! is created. A thread-local context lookup would be fragile here because BIO
//! callbacks can park the current stackful coroutine while another coroutine runs
//! and performs its own TLS operation.

use std::{
    ffi::CStr,
    marker::PhantomData,
    os::raw::{c_char, c_int, c_long, c_void},
    ptr, slice,
    sync::OnceLock,
};

use foreign_types_shared::ForeignType;
use kimojio_stack::{Errno, RuntimeContext};
use openssl::ssl::{Ssl, SslContext, SslRef};
use openssl_sys as ffi;
use rustix::{fd::OwnedFd, net::Shutdown};

/// A TLS context backed by an OpenSSL `SSL_CTX`.
///
/// Unlike `kimojio-stack-tls`, this crate keeps the Rust OpenSSL context and
/// attaches a specialized BIO wrapper that calls the stackful runtime directly.
pub struct TlsContext {
    ssl_ctx: SslContext,
}

impl TlsContext {
    /// Creates a stack TLS2 context from an OpenSSL crate context.
    pub fn from_openssl(ctx: SslContext) -> Self {
        Self { ssl_ctx: ctx }
    }

    /// Performs a server-side TLS handshake over `socket`.
    pub fn server<'io, 'cx>(
        &self,
        cx: &'io RuntimeContext<'cx>,
        socket: OwnedFd,
    ) -> Result<TlsStream<'io, 'cx>, Errno> {
        let ssl = Ssl::new(&self.ssl_ctx).map_err(|_| Errno::PROTO)?;
        let mut stream = TlsStream::new(ssl, cx, socket)?;
        stream.accept()?;
        Ok(stream)
    }

    /// Performs a client-side TLS handshake over `socket`.
    pub fn client<'io, 'cx>(
        &self,
        cx: &'io RuntimeContext<'cx>,
        socket: OwnedFd,
    ) -> Result<TlsStream<'io, 'cx>, Errno> {
        let ssl = Ssl::new(&self.ssl_ctx).map_err(|_| Errno::PROTO)?;
        let mut stream = TlsStream::new(ssl, cx, socket)?;
        stream.connect()?;
        Ok(stream)
    }
}

/// A TLS stream whose OpenSSL BIO callbacks use `kimojio-stack` I/O directly.
pub struct TlsStream<'io, 'cx> {
    ssl: Ssl,
    state: *mut StackBioState,
    _context: PhantomData<&'io RuntimeContext<'cx>>,
}

impl<'io, 'cx> TlsStream<'io, 'cx> {
    fn new(ssl: Ssl, cx: &'io RuntimeContext<'cx>, socket: OwnedFd) -> Result<Self, Errno> {
        let method = stack_bio_method()?;
        let state = Box::new(StackBioState::new(cx, socket));
        let state = Box::into_raw(state);

        // SAFETY: `method` has process lifetime. `bio` takes ownership of the
        // boxed state after `BIO_set_data`; `SSL_set_bio` transfers BIO
        // ownership to `ssl`, which invokes `stack_bio_destroy` on drop.
        unsafe {
            let bio = ffi::BIO_new(method);
            if bio.is_null() {
                let _ = Box::from_raw(state);
                return Err(Errno::PROTO);
            }

            ffi::BIO_set_data(bio, state.cast());
            ffi::BIO_set_init(bio, 1);
            ffi::SSL_set_bio(ssl.as_ptr(), bio, bio);
        }

        Ok(Self {
            ssl,
            state,
            _context: PhantomData,
        })
    }

    /// Gets the OpenSSL `SslRef` for inspection.
    pub fn ssl(&self) -> &SslRef {
        &self.ssl
    }

    /// Reads decrypted TLS plaintext into `buf`.
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
        if buf.is_empty() {
            return Ok(0);
        }

        loop {
            let mut read = 0;
            let result = unsafe {
                ffi::SSL_read_ex(
                    self.ssl.as_ptr(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    &mut read,
                )
            };
            if result > 0 {
                return Ok(read);
            }

            match self.ssl_error(result) {
                SslError::Retry => continue,
                SslError::Eof => return Ok(0),
                SslError::Err(errno) => return Err(errno),
            }
        }
    }

    /// Fills `buf` with decrypted TLS plaintext unless EOF is reached first.
    pub fn read_exact_or_eof(&mut self, mut buf: &mut [u8]) -> Result<usize, Errno> {
        let requested = buf.len();
        while !buf.is_empty() {
            let amount = self.read(buf)?;
            if amount == 0 {
                break;
            }
            buf = &mut buf[amount..];
        }
        Ok(requested - buf.len())
    }

    /// Writes plaintext from `buf`.
    pub fn write(&mut self, buf: &[u8]) -> Result<usize, Errno> {
        if buf.is_empty() {
            return Ok(0);
        }

        loop {
            let mut written = 0;
            let result = unsafe {
                ffi::SSL_write_ex(
                    self.ssl.as_ptr(),
                    buf.as_ptr().cast(),
                    buf.len(),
                    &mut written,
                )
            };
            if result > 0 {
                return Ok(written);
            }

            match self.ssl_error(result) {
                SslError::Retry => continue,
                SslError::Eof => return Ok(0),
                SslError::Err(errno) => return Err(errno),
            }
        }
    }

    /// Writes all plaintext from `buf`.
    pub fn write_all(&mut self, mut buf: &[u8]) -> Result<(), Errno> {
        while !buf.is_empty() {
            let amount = self.write(buf)?;
            if amount == 0 {
                return Err(Errno::PIPE);
            }
            buf = &buf[amount..];
        }
        Ok(())
    }

    /// Flushes the OpenSSL stream.
    pub fn flush(&mut self) -> Result<(), Errno> {
        Ok(())
    }

    /// Sends and receives TLS close-notify.
    pub fn shutdown(&mut self) -> Result<(), Errno> {
        loop {
            let result = unsafe { ffi::SSL_shutdown(self.ssl.as_ptr()) };
            match result {
                1 => return Ok(()),
                0 => continue,
                _ => match self.ssl_error(result) {
                    SslError::Retry => continue,
                    SslError::Eof => return Ok(()),
                    SslError::Err(errno) => return Err(errno),
                },
            }
        }
    }

    /// Shuts down the underlying socket write half.
    pub fn shutdown_write(&self) -> Result<(), Errno> {
        self.state_ref().shutdown_write()
    }

    /// Closes the underlying socket through the stack runtime.
    pub fn close(mut self) -> Result<(), Errno> {
        self.state_mut().close()
    }

    fn accept(&mut self) -> Result<(), Errno> {
        loop {
            let result = unsafe { ffi::SSL_accept(self.ssl.as_ptr()) };
            if result == 1 {
                return Ok(());
            }

            match self.ssl_error(result) {
                SslError::Retry => continue,
                SslError::Eof => return Err(Errno::PROTO),
                SslError::Err(errno) => return Err(errno),
            }
        }
    }

    fn connect(&mut self) -> Result<(), Errno> {
        loop {
            let result = unsafe { ffi::SSL_connect(self.ssl.as_ptr()) };
            if result == 1 {
                return Ok(());
            }

            match self.ssl_error(result) {
                SslError::Retry => continue,
                SslError::Eof => return Err(Errno::PROTO),
                SslError::Err(errno) => return Err(errno),
            }
        }
    }

    fn ssl_error(&mut self, result: c_int) -> SslError {
        let code = unsafe { ffi::SSL_get_error(self.ssl.as_ptr(), result) };
        match code {
            ffi::SSL_ERROR_ZERO_RETURN => SslError::Eof,
            ffi::SSL_ERROR_WANT_READ | ffi::SSL_ERROR_WANT_WRITE | ffi::SSL_ERROR_SYSCALL => {
                match self.state_mut().take_errno() {
                    Some(errno) => SslError::Err(errno),
                    None if code == ffi::SSL_ERROR_SYSCALL => SslError::Eof,
                    None => SslError::Retry,
                }
            }
            ffi::SSL_ERROR_SSL => SslError::Err(Errno::PROTO),
            _ => SslError::Err(Errno::PROTO),
        }
    }

    fn state_ref(&self) -> &StackBioState {
        // SAFETY: `self.state` is allocated in `new` and remains owned by the
        // BIO attached to `self.ssl` for the lifetime of this stream.
        unsafe { &*self.state }
    }

    fn state_mut(&mut self) -> &mut StackBioState {
        // SAFETY: `&mut self` guarantees exclusive access to the OpenSSL stream
        // and its BIO state.
        unsafe { &mut *self.state }
    }
}

enum SslError {
    Retry,
    Eof,
    Err(Errno),
}

struct StackBioState {
    cx: *const RuntimeContext<'static>,
    socket: Option<OwnedFd>,
    errno: Option<Errno>,
}

impl StackBioState {
    fn new<'io, 'cx>(cx: &'io RuntimeContext<'cx>, socket: OwnedFd) -> Self {
        Self {
            cx: (cx as *const RuntimeContext<'cx>).cast(),
            socket: Some(socket),
            errno: None,
        }
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
        let socket = self.socket.as_ref().ok_or(Errno::PIPE)?;
        self.cx().read(socket, buf)
    }

    fn write(&mut self, buf: &[u8]) -> Result<usize, Errno> {
        let socket = self.socket.as_ref().ok_or(Errno::PIPE)?;
        self.cx().write(socket, buf)
    }

    fn shutdown_write(&self) -> Result<(), Errno> {
        let socket = self.socket.as_ref().ok_or(Errno::PIPE)?;
        self.cx().shutdown(socket, Shutdown::Write)
    }

    fn close(&mut self) -> Result<(), Errno> {
        let socket = self.socket.take().ok_or(Errno::PIPE)?;
        self.cx().close(socket)
    }

    fn take_errno(&mut self) -> Option<Errno> {
        self.errno.take()
    }

    fn cx(&self) -> &RuntimeContext<'_> {
        // SAFETY: `TlsStream` carries a phantom borrow tying this erased pointer
        // to the `RuntimeContext` lifetime supplied at construction.
        unsafe { &*self.cx }
    }
}

fn stack_bio_method() -> Result<*const ffi::BIO_METHOD, Errno> {
    static METHOD: OnceLock<Result<usize, Errno>> = OnceLock::new();
    match *METHOD.get_or_init(create_stack_bio_method) {
        Ok(method) => Ok(method as *const ffi::BIO_METHOD),
        Err(errno) => Err(errno),
    }
}

fn create_stack_bio_method() -> Result<usize, Errno> {
    // SAFETY: Constructs a process-lifetime BIO method whose callbacks all have
    // the signatures OpenSSL expects.
    unsafe {
        let method = ffi::BIO_meth_new(ffi::BIO_TYPE_NONE, c"kimojio-stack-tls2".as_ptr());
        if method.is_null() {
            return Err(Errno::PROTO);
        }

        let ok = ffi::BIO_meth_set_write__fixed_rust(method, Some(stack_bio_write)) == 1
            && ffi::BIO_meth_set_read__fixed_rust(method, Some(stack_bio_read)) == 1
            && ffi::BIO_meth_set_puts__fixed_rust(method, Some(stack_bio_puts)) == 1
            && ffi::BIO_meth_set_ctrl__fixed_rust(method, Some(stack_bio_ctrl)) == 1
            && ffi::BIO_meth_set_create__fixed_rust(method, Some(stack_bio_create)) == 1
            && ffi::BIO_meth_set_destroy__fixed_rust(method, Some(stack_bio_destroy)) == 1;

        if ok {
            Ok(method as usize)
        } else {
            ffi::BIO_meth_free(method);
            Err(Errno::PROTO)
        }
    }
}

unsafe extern "C" fn stack_bio_write(bio: *mut ffi::BIO, buf: *const c_char, len: c_int) -> c_int {
    if len <= 0 {
        return 0;
    }

    let state = unsafe { stack_bio_state(bio) };
    let buf = unsafe { slice::from_raw_parts(buf.cast::<u8>(), len as usize) };
    match state.write(buf) {
        Ok(amount) => amount as c_int,
        Err(errno) => {
            state.errno = Some(errno);
            -1
        }
    }
}

unsafe extern "C" fn stack_bio_read(bio: *mut ffi::BIO, buf: *mut c_char, len: c_int) -> c_int {
    if len <= 0 {
        return 0;
    }

    let state = unsafe { stack_bio_state(bio) };
    let buf = unsafe { slice::from_raw_parts_mut(buf.cast::<u8>(), len as usize) };
    match state.read(buf) {
        Ok(amount) => amount as c_int,
        Err(errno) => {
            state.errno = Some(errno);
            -1
        }
    }
}

unsafe extern "C" fn stack_bio_puts(bio: *mut ffi::BIO, s: *const c_char) -> c_int {
    if s.is_null() {
        return 0;
    }

    let bytes = unsafe { CStr::from_ptr(s).to_bytes() };
    let len = usize::min(bytes.len(), c_int::MAX as usize) as c_int;
    unsafe { stack_bio_write(bio, s, len) }
}

unsafe extern "C" fn stack_bio_ctrl(
    _bio: *mut ffi::BIO,
    cmd: c_int,
    _num: c_long,
    _ptr: *mut c_void,
) -> c_long {
    if cmd == ffi::BIO_CTRL_FLUSH { 1 } else { 0 }
}

unsafe extern "C" fn stack_bio_create(bio: *mut ffi::BIO) -> c_int {
    unsafe {
        ffi::BIO_set_init(bio, 0);
        ffi::BIO_set_data(bio, ptr::null_mut());
        ffi::BIO_clear_flags(bio, ffi::BIO_FLAGS_RWS | ffi::BIO_FLAGS_SHOULD_RETRY);
    }
    1
}

unsafe extern "C" fn stack_bio_destroy(bio: *mut ffi::BIO) -> c_int {
    if bio.is_null() {
        return 0;
    }

    let data = unsafe { ffi::BIO_get_data(bio) };
    if !data.is_null() {
        let _ = unsafe { Box::<StackBioState>::from_raw(data.cast()) };
    }
    unsafe {
        ffi::BIO_set_data(bio, ptr::null_mut());
        ffi::BIO_set_init(bio, 0);
    }
    1
}

unsafe fn stack_bio_state<'a>(bio: *mut ffi::BIO) -> &'a mut StackBioState {
    let data = unsafe { ffi::BIO_get_data(bio) };
    debug_assert!(!data.is_null());
    unsafe { &mut *data.cast::<StackBioState>() }
}

#[cfg(test)]
mod tests {
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

        let message = b"hello from stack tls2; this uses direct OpenSSL BIO callbacks";
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut tls = server_ctx
                        .server(cx, server_fd)
                        .expect("server TLS handshake failed");
                    let mut buf = [0_u8; 7];
                    let mut read = 0;
                    while read < message.len() {
                        let amount = tls.read(&mut buf).expect("server TLS read failed");
                        assert_ne!(amount, 0);
                        read += amount;
                        tls.write_all(&buf[..amount])
                            .expect("server TLS write failed");
                    }
                    tls.shutdown().expect("server TLS shutdown failed");
                    tls.close().expect("server TLS close failed");
                });

                let client = scope.spawn(move |cx| {
                    let mut tls = client_ctx
                        .client(cx, client_fd)
                        .expect("client TLS handshake failed");
                    tls.write_all(message).expect("client TLS write failed");

                    let mut echoed = Vec::new();
                    let mut buf = [0_u8; 5];
                    while echoed.len() < message.len() {
                        let amount = tls.read(&mut buf).expect("client TLS read failed");
                        if amount == 0 {
                            break;
                        }
                        echoed.extend_from_slice(&buf[..amount]);
                    }
                    tls.shutdown().expect("client TLS shutdown failed");
                    tls.close().expect("client TLS close failed");
                    echoed
                });

                server.join(cx).unwrap();
                let echoed = client.join(cx).unwrap();
                assert_eq!(echoed, message);
            });
        });
    }
}
