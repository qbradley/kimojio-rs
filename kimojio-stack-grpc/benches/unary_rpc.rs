// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::{
    hint::black_box,
    time::{Duration, Instant},
};

use criterion::{Criterion, criterion_group, criterion_main};
use http::{HeaderName, HeaderValue};
use kimojio_stack::{Runtime, RuntimeContext};
use kimojio_stack_grpc::{
    Metadata, UnaryClient, UnaryReply, UnaryServer, client::ClientConfig, server::ServerConfig,
};
use kimojio_stack_http::{
    HttpConfig, StackTransport, h2,
    tls::{self, HttpProtocol},
};
use kimojio_stack_tls::TlsContext;
use openssl::{
    asn1::Asn1Time,
    hash::MessageDigest,
    nid::Nid,
    pkey::{PKey, Private},
    rsa::Rsa,
    ssl::{AlpnError, SslAcceptor, SslConnector, SslMethod, SslVerifyMode, select_next_proto},
    x509::{X509, X509NameBuilder},
};
use prost::Message;
use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};

const SMALL_PAYLOAD: usize = 64;
const MODERATE_PAYLOAD: usize = 16 * 1024;
const TLS_BUFFER_SIZE: usize = 16 * 1024;
const METHOD: &str = "/bench.Bench/Unary";

#[derive(Clone, PartialEq, Message)]
struct BenchMessage {
    #[prost(string, tag = "1")]
    value: String,
    #[prost(bytes, tag = "2")]
    payload: Vec<u8>,
}

fn bench_grpc(c: &mut Criterion) {
    for size in [SMALL_PAYLOAD, MODERATE_PAYLOAD] {
        let label = if size == SMALL_PAYLOAD {
            "small_payload"
        } else {
            "moderate_payload"
        };
        c.bench_function(&format!("grpc/plaintext/{label}"), |b| {
            b.iter_custom(|iters| run_plaintext(iters, size));
        });
        c.bench_function(&format!("grpc/tls/{label}"), |b| {
            b.iter_custom(|iters| run_tls(iters, size));
        });
    }
}

fn run_plaintext(iters: u64, payload_len: usize) -> Duration {
    let (client, server) = socket_transport_pair();
    let mut runtime = Runtime::new();
    runtime.block_on(|cx| run_grpc_in_scope(cx, iters, payload_len, client, server))
}

fn run_tls(iters: u64, payload_len: usize) -> Duration {
    let contexts = tls_contexts();
    let (client_fd, server_fd) = socketpair_fds();
    let mut runtime = Runtime::new();
    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let server_ctx = contexts.server;
            let server = scope.spawn(move |cx| {
                tls::server_transport(
                    cx,
                    &server_ctx,
                    TLS_BUFFER_SIZE,
                    server_fd,
                    Some(HttpProtocol::Http2),
                )
                .unwrap()
            });
            let client_ctx = contexts.client;
            let client = tls::client_transport(
                cx,
                &client_ctx,
                TLS_BUFFER_SIZE,
                client_fd,
                "localhost",
                Some(HttpProtocol::Http2),
            )
            .unwrap();
            let server = server.join(cx).unwrap();
            run_grpc_in_scope(cx, iters, payload_len, client, server)
        })
    })
}

fn run_grpc_in_scope(
    cx: &RuntimeContext<'_>,
    iters: u64,
    payload_len: usize,
    client: StackTransport,
    server: StackTransport,
) -> Duration {
    let request = BenchMessage {
        value: "request".to_owned(),
        payload: vec![b'x'; payload_len],
    };
    let rounds = iters.saturating_add(1);

    cx.scope(|scope| {
        let server = scope.spawn(move |cx| {
            let mut http = h2::ServerConnection::new(server, HttpConfig::default());
            let mut grpc = UnaryServer::new(ServerConfig::default());
            grpc.add_unary::<BenchMessage, BenchMessage, _>(METHOD, |_cx, _metadata, request| {
                let mut reply = UnaryReply::new(BenchMessage {
                    value: request.value,
                    payload: request.payload,
                });
                reply
                    .metadata
                    .insert(
                        HeaderName::from_static("x-bench"),
                        HeaderValue::from_static("response"),
                    )
                    .unwrap();
                Ok(reply)
            });
            for _ in 0..rounds {
                grpc.serve_one(cx, &mut http).unwrap();
            }
            http.shutdown_write_and_close_after_peer(cx).unwrap();
        });

        let http = h2::ClientConnection::new(client, HttpConfig::default());
        let mut client = UnaryClient::new(http, ClientConfig::default());
        let metadata = metadata();
        black_box(
            client
                .call::<_, BenchMessage>(cx, METHOD, metadata.clone(), &request)
                .unwrap(),
        );
        let start = Instant::now();
        for _ in 0..iters {
            black_box(
                client
                    .call::<_, BenchMessage>(cx, METHOD, metadata.clone(), &request)
                    .unwrap(),
            );
        }
        let elapsed = start.elapsed();
        client.close(cx).unwrap();
        server.join(cx).unwrap();
        elapsed
    })
}

fn metadata() -> Metadata {
    let mut metadata = Metadata::new();
    metadata
        .insert(
            HeaderName::from_static("x-bench"),
            HeaderValue::from_static("request"),
        )
        .unwrap();
    metadata
}

fn socket_transport_pair() -> (StackTransport, StackTransport) {
    let (client, server) = socketpair_fds();
    (
        StackTransport::plaintext(client),
        StackTransport::plaintext(server),
    )
}

fn socketpair_fds() -> (rustix::fd::OwnedFd, rustix::fd::OwnedFd) {
    socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::empty(),
        None,
    )
    .unwrap()
}

struct TlsContexts {
    server: TlsContext,
    client: TlsContext,
}

fn tls_contexts() -> TlsContexts {
    let (cert, key) = self_signed_cert();
    let mut acceptor = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).unwrap();
    acceptor.set_private_key(&key).unwrap();
    acceptor.set_certificate(&cert).unwrap();
    acceptor.check_private_key().unwrap();
    acceptor.set_alpn_select_callback(|_ssl, client| {
        select_next_proto(b"\x02h2", client).ok_or(AlpnError::NOACK)
    });

    let mut connector = SslConnector::builder(SslMethod::tls()).unwrap();
    connector.set_verify(SslVerifyMode::NONE);
    connector.set_alpn_protos(b"\x02h2").unwrap();

    TlsContexts {
        server: TlsContext::from_openssl(acceptor.build().into_context()),
        client: TlsContext::from_openssl(connector.build().into_context()),
    }
}

fn self_signed_cert() -> (X509, PKey<Private>) {
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
    (cert.build(), key)
}

criterion_group!(
    name = benches;
    config = Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_millis(100))
        .measurement_time(Duration::from_millis(200));
    targets = bench_grpc
);
criterion_main!(benches);
