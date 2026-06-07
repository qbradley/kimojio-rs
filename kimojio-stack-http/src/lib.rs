// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Stackful HTTP foundations for `kimojio-stack`.
//!
//! This crate provides low-level transport, buffering, error, and protocol
//! boundary types used by the HTTP/1.1 and HTTP/2 implementations.

pub mod body;
pub mod client;
pub mod config;
pub mod error;
pub mod h2;
pub mod headers;
pub mod http1;
pub mod server;
pub mod tls;
pub mod transport;

pub use body::{Body, BodyBuilder, BodyLimits};
pub use config::HttpConfig;
pub use error::{Error, ErrorKind, LimitKind};
pub use headers::{Headers, Trailers};
pub use transport::StackTransport;

#[cfg(test)]
mod tests {
    use http::{HeaderName, HeaderValue, header::CONTENT_TYPE};
    use kimojio_stack::Runtime;
    use kimojio_stack_tls::TlsContext;
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

    use super::{Body, BodyLimits, Headers, StackTransport, Trailers};

    const TLS_BUFFER_SIZE: usize = 16 * 1024;

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
    fn body_limits_reject_oversized_payloads() {
        let limits = BodyLimits::new(4);

        let error = Body::copy_from_slice(b"hello", limits).unwrap_err();

        assert_eq!(error.kind(), super::ErrorKind::SizeLimit);
    }

    #[test]
    fn headers_and_trailers_wrap_header_maps() {
        let mut headers = Headers::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/grpc"));
        assert_eq!(
            headers.get(&CONTENT_TYPE),
            Some(&HeaderValue::from_static("application/grpc"))
        );

        let grpc_status = HeaderName::from_static("grpc-status");
        let mut trailers = Trailers::new();
        trailers.insert(grpc_status.clone(), HeaderValue::from_static("0"));
        assert_eq!(
            trailers.get(&grpc_status),
            Some(&HeaderValue::from_static("0"))
        );
    }

    #[test]
    fn plaintext_transport_moves_bytes_over_socketpair() {
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
                    let mut transport = StackTransport::plaintext(server_fd);
                    let mut received = [0_u8; 5];
                    let amount = transport.read_exact_or_eof(cx, &mut received).unwrap();
                    assert_eq!(amount, received.len());
                    assert_eq!(&received, b"hello");
                    transport.write_all(cx, b"world").unwrap();
                    transport.shutdown(cx).unwrap();
                    transport.close(cx).unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let mut transport = StackTransport::plaintext(client_fd);
                    transport.write_all(cx, b"hello").unwrap();
                    let mut received = [0_u8; 5];
                    let amount = transport.read_exact_or_eof(cx, &mut received).unwrap();
                    assert_eq!(amount, received.len());
                    transport.shutdown(cx).unwrap();
                    transport.close(cx).unwrap();
                    received
                });

                server.join(cx).unwrap();
                let received = client.join(cx).unwrap();
                assert_eq!(&received, b"world");
            });
        });
    }

    #[test]
    fn tls_transport_moves_bytes_over_socketpair() {
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
                    let tls = server_ctx
                        .server(cx, TLS_BUFFER_SIZE, server_fd)
                        .expect("server TLS handshake failed");
                    let mut transport = StackTransport::tls(tls);
                    let mut received = [0_u8; 5];
                    let amount = transport.read_exact_or_eof(cx, &mut received).unwrap();
                    assert_eq!(amount, received.len());
                    assert_eq!(&received, b"hello");
                    transport.write_all(cx, b"world").unwrap();
                    transport.shutdown(cx).unwrap();
                    transport.close(cx).unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let tls = client_ctx
                        .client(cx, TLS_BUFFER_SIZE, client_fd)
                        .expect("client TLS handshake failed");
                    let mut transport = StackTransport::tls(tls);
                    transport.write_all(cx, b"hello").unwrap();
                    let mut received = [0_u8; 5];
                    let amount = transport.read_exact_or_eof(cx, &mut received).unwrap();
                    assert_eq!(amount, received.len());
                    transport.shutdown(cx).unwrap();
                    transport.close(cx).unwrap();
                    received
                });

                server.join(cx).unwrap();
                let received = client.join(cx).unwrap();
                assert_eq!(&received, b"world");
            });
        });
    }
}
