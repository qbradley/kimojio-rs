// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(all(feature = "http", feature = "tls"))]

use std::cell::{Cell, RefCell};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpStream};
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};

use kimojio::http::{
    AlpnProtocol, Body, Client, ClientConfig, Error as HttpError, ProtocolErrorKind, Response,
    ServeError, Server, TlsClientConfig, TlsServerConfig, Version,
};
use kimojio::tlscontext::TlsContext;
use kimojio::{CancellationToken, operations};
use openssl::asn1::Asn1Time;
use openssl::bn::{BigNum, MsbOption};
use openssl::hash::MessageDigest;
use openssl::pkey::PKey;
use openssl::rsa::Rsa;
use openssl::ssl::{
    AlpnError, SslConnector, SslContextBuilder, SslMethod, SslVerifyMode, SslVersion,
};
use openssl::x509::extension::SubjectAlternativeName;
use openssl::x509::{X509, X509NameBuilder};

const WAIT: Duration = Duration::from_secs(10);
const UNSUPPORTED_ALPN: &[u8] = b"driver-test";

fn test_tls_server_builder() -> SslContextBuilder {
    let key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
    let mut name = X509NameBuilder::new().unwrap();
    name.append_entry_by_text("CN", "127.0.0.1").unwrap();
    let name = name.build();
    let mut serial = BigNum::new().unwrap();
    serial.rand(128, MsbOption::MAYBE_ZERO, false).unwrap();
    let serial = serial.to_asn1_integer().unwrap();
    let mut certificate = X509::builder().unwrap();
    certificate.set_version(2).unwrap();
    certificate.set_serial_number(&serial).unwrap();
    certificate.set_subject_name(&name).unwrap();
    certificate.set_issuer_name(&name).unwrap();
    certificate.set_pubkey(&key).unwrap();
    certificate
        .set_not_before(Asn1Time::days_from_now(0).unwrap().as_ref())
        .unwrap();
    certificate
        .set_not_after(Asn1Time::days_from_now(1).unwrap().as_ref())
        .unwrap();
    let context = certificate.x509v3_context(None, None);
    let subject_alt_name = SubjectAlternativeName::new()
        .ip("127.0.0.1")
        .build(&context)
        .unwrap();
    certificate.append_extension(subject_alt_name).unwrap();
    certificate.sign(&key, MessageDigest::sha256()).unwrap();
    let certificate = certificate.build();

    let mut builder = SslContextBuilder::new(SslMethod::tls_server()).unwrap();
    builder.set_private_key(&key).unwrap();
    builder.set_certificate(&certificate).unwrap();
    builder.check_private_key().unwrap();
    builder
        .set_min_proto_version(Some(SslVersion::TLS1_3))
        .unwrap();
    builder
        .set_max_proto_version(Some(SslVersion::TLS1_3))
        .unwrap();
    builder
}

fn test_tls_server_config(protocols: &[AlpnProtocol]) -> TlsServerConfig {
    TlsServerConfig::new(test_tls_server_builder(), protocols).unwrap()
}

fn test_tls_client_config(protocols: &[AlpnProtocol]) -> TlsClientConfig {
    let mut builder = SslContextBuilder::new(SslMethod::tls_client()).unwrap();
    builder.set_verify(SslVerifyMode::NONE);
    builder
        .set_min_proto_version(Some(SslVersion::TLS1_3))
        .unwrap();
    builder
        .set_max_proto_version(Some(SslVersion::TLS1_3))
        .unwrap();
    TlsClientConfig::new(builder, protocols).unwrap()
}

fn select_unsupported_alpn(offered: &[u8]) -> Result<&[u8], AlpnError> {
    let mut remaining = offered;
    while let Some((&length, rest)) = remaining.split_first() {
        let length = usize::from(length);
        if length == 0 || rest.len() < length {
            return Err(AlpnError::ALERT_FATAL);
        }
        let (protocol, tail) = rest.split_at(length);
        if protocol == UNSUPPORTED_ALPN {
            return Ok(protocol);
        }
        remaining = tail;
    }
    Err(AlpnError::ALERT_FATAL)
}

#[kimojio::test]
async fn tls_alpn_serves_http2_and_http1() {
    let server = Server::builder((Ipv4Addr::LOCALHOST, 0).into())
        .tls(test_tls_server_config(&[
            AlpnProtocol::H2,
            AlpnProtocol::Http11,
        ]))
        .bind()
        .await
        .unwrap();
    let address = server.local_addr();
    let cancellation = Rc::new(CancellationToken::new());
    let cancel_from_handler = Rc::clone(&cancellation);
    let versions = Rc::new(RefCell::new(Vec::new()));
    let observed_versions = Rc::clone(&versions);
    let serve_task = operations::spawn_task(server.serve(
        move |request| {
            observed_versions.borrow_mut().push(request.version());
            if observed_versions.borrow().len() == 2 {
                cancel_from_handler.cancel();
            }
            let body = match request.version() {
                Version::HTTP_2 => "h2",
                Version::HTTP_11 => "http/1.1",
                version => panic!("unexpected negotiated HTTP version: {version:?}"),
            };
            async move { Response::new(Body::from(body)) }
        },
        cancellation,
    ));

    let h2_client = Client::with_config(
        ClientConfig::new().set_tls(test_tls_client_config(&[AlpnProtocol::H2])),
    )
    .unwrap();
    let h2_response = operations::timeout_at(
        Instant::now() + WAIT,
        h2_client
            .get(format!("https://{address}/h2"))
            .version(Version::HTTP_2)
            .send(),
    )
    .await
    .expect("HTTP/2 TLS request timed out")
    .unwrap();
    assert_eq!(h2_response.version(), Version::HTTP_2);
    assert_eq!(h2_response.body().as_bytes(), b"h2");

    let http1_client = Client::with_config(
        ClientConfig::new().set_tls(test_tls_client_config(&[AlpnProtocol::Http11])),
    )
    .unwrap();
    let http1_response = operations::timeout_at(
        Instant::now() + WAIT,
        http1_client
            .get(format!("https://{address}/http1"))
            .version(Version::HTTP_11)
            .send(),
    )
    .await
    .expect("HTTP/1.1 TLS request timed out")
    .unwrap();
    assert_eq!(http1_response.version(), Version::HTTP_11);
    assert_eq!(http1_response.body().as_bytes(), b"http/1.1");

    drop(http1_response);
    drop(http1_client);
    drop(h2_response);
    drop(h2_client);
    operations::timeout_at(Instant::now() + WAIT, serve_task)
        .await
        .expect("TLS server did not shut down")
        .unwrap()
        .unwrap();
    assert_eq!(
        *versions.borrow(),
        [Version::HTTP_2, Version::HTTP_11],
        "the handler must observe the protocol selected by each TLS handshake"
    );
}

#[kimojio::test]
async fn unsupported_negotiated_alpn_fails_before_http_dispatch() {
    let mut builder = test_tls_server_builder();
    builder.set_alpn_select_callback(|_, offered| select_unsupported_alpn(offered));
    let tls = TlsServerConfig::from_context(TlsContext::from_openssl(builder.build()));
    let server = Server::builder((Ipv4Addr::LOCALHOST, 0).into())
        .tls(tls)
        .bind()
        .await
        .unwrap();
    let address = server.local_addr();
    let cancellation = Rc::new(CancellationToken::new());
    let cancel_from_handler = Rc::clone(&cancellation);
    let cancel_from_error = Rc::clone(&cancellation);
    let handler_called = Rc::new(Cell::new(false));
    let observed_handler = Rc::clone(&handler_called);
    let unsupported_reported = Rc::new(Cell::new(false));
    let observed_error = Rc::clone(&unsupported_reported);
    let serve_task = operations::spawn_task(server.serve_with_error_handler(
        move |_request| {
            observed_handler.set(true);
            cancel_from_handler.cancel();
            async { Response::new(Body::from("unexpected")) }
        },
        cancellation,
        move |error| match error {
            ServeError::Connection(HttpError::Protocol(error))
                if error.kind() == ProtocolErrorKind::UnsupportedFeature =>
            {
                observed_error.set(true);
                cancel_from_error.cancel();
            }
            other => panic!("unexpected server error: {other}"),
        },
    ));

    let peer = thread::spawn(move || {
        let mut builder = SslConnector::builder(SslMethod::tls_client()).unwrap();
        builder.set_verify(SslVerifyMode::NONE);
        builder
            .set_min_proto_version(Some(SslVersion::TLS1_3))
            .unwrap();
        builder
            .set_max_proto_version(Some(SslVersion::TLS1_3))
            .unwrap();
        builder.set_alpn_protos(b"\x0bdriver-test").unwrap();
        let connector = builder.build();
        let socket = TcpStream::connect(address).unwrap();
        socket.set_read_timeout(Some(WAIT)).unwrap();
        socket.set_write_timeout(Some(WAIT)).unwrap();
        let mut stream = connector
            .configure()
            .unwrap()
            .verify_hostname(false)
            .connect("127.0.0.1", socket)
            .unwrap();
        assert_eq!(
            stream.ssl().selected_alpn_protocol(),
            Some(UNSUPPORTED_ALPN),
            "the TLS handshake must actually select the unsupported identifier"
        );
        let _ = stream.write_all(
            b"GET /must-not-dispatch HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n",
        );
        let mut byte = [0; 1];
        match stream.read(&mut byte) {
            Ok(0) | Err(_) => {}
            Ok(read) => panic!("server emitted {read} HTTP bytes after unsupported ALPN"),
        }
    });

    operations::timeout_at(Instant::now() + WAIT, serve_task)
        .await
        .expect("unsupported-ALPN server did not shut down")
        .unwrap()
        .unwrap();
    peer.join().expect("unsupported-ALPN peer panicked");
    assert!(unsupported_reported.get());
    assert!(!handler_called.get());
}
