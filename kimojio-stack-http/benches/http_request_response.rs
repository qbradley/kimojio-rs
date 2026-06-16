// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::{
    hint::black_box,
    num::NonZeroUsize,
    time::{Duration, Instant},
};

use criterion::{Criterion, criterion_group, criterion_main};
use http::{Request, Response, StatusCode};
use kimojio_stack::{Runtime, RuntimeContext, SocketIoRuntime};
use kimojio_stack_http::{
    Body, BodyLimits, HttpConfig, RuntimeStackTransport, StackTransport, h2, http1,
    tls::{self, HttpProtocol},
};
use kimojio_stack_steal::{
    Runtime as StealRuntime, RuntimeConfig as StealRuntimeConfig, StealPolicy,
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
use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};

const SMALL_BODY: usize = 64;
const LARGE_BODY: usize = 16 * 1024;
const TLS_BUFFER_SIZE: usize = 16 * 1024;
const BENCH_DEADLINE: Duration = Duration::from_secs(60);

#[derive(Clone, Copy)]
enum StealPath {
    WorkerLocal,
    SharedRoot,
}

fn bench_http(c: &mut Criterion) {
    for size in [SMALL_BODY, LARGE_BODY] {
        let label = if size == SMALL_BODY {
            "small_body"
        } else {
            "large_body"
        };

        c.bench_function(
            &format!("stack-core/http1/plaintext/request_response/{label}"),
            |b| {
                b.iter_custom(|iters| run_http1_plaintext(iters, size));
            },
        );
        c.bench_function(
            &format!("stack-core/http1/plaintext/deadline/request_response/{label}"),
            |b| {
                b.iter_custom(|iters| run_http1_plaintext_with_deadline(iters, size));
            },
        );
        c.bench_function(
            &format!("stack-core/http1/tls/request_response/{label}"),
            |b| {
                b.iter_custom(|iters| run_http1_tls(iters, size));
            },
        );
        c.bench_function(
            &format!("stack-steal/worker-local/http1/plaintext/request_response/{label}"),
            |b| {
                b.iter_custom(|iters| {
                    run_http1_steal_plaintext(iters, size, StealPath::WorkerLocal)
                });
            },
        );
        c.bench_function(
            &format!("stack-steal/shared-root/http1/plaintext/request_response/{label}"),
            |b| {
                b.iter_custom(|iters| {
                    run_http1_steal_plaintext(iters, size, StealPath::SharedRoot)
                });
            },
        );

        c.bench_function(
            &format!("stack-core/h2/plaintext/request_response/{label}"),
            |b| {
                b.iter_custom(|iters| run_h2_plaintext(iters, size));
            },
        );
        c.bench_function(
            &format!("stack-core/h2/plaintext/deadline/request_response/{label}"),
            |b| {
                b.iter_custom(|iters| run_h2_plaintext_with_deadline(iters, size));
            },
        );
        c.bench_function(
            &format!("stack-core/h2/tls/request_response/{label}"),
            |b| {
                b.iter_custom(|iters| run_h2_tls(iters, size));
            },
        );
        c.bench_function(
            &format!("stack-steal/worker-local/h2/plaintext/request_response/{label}"),
            |b| {
                b.iter_custom(|iters| run_h2_steal_plaintext(iters, size, StealPath::WorkerLocal));
            },
        );
        c.bench_function(
            &format!("stack-steal/shared-root/h2/plaintext/request_response/{label}"),
            |b| {
                b.iter_custom(|iters| run_h2_steal_plaintext(iters, size, StealPath::SharedRoot));
            },
        );
    }
}

fn run_http1_plaintext(iters: u64, body_len: usize) -> Duration {
    let (client, server) = socket_transport_pair();
    run_http1(iters, body_len, client, server, None)
}

fn run_http1_plaintext_with_deadline(iters: u64, body_len: usize) -> Duration {
    let (client, server) = socket_transport_pair();
    run_http1(iters, body_len, client, server, Some(BENCH_DEADLINE))
}

fn run_http1_tls(iters: u64, body_len: usize) -> Duration {
    let contexts = tls_contexts(Some(b"\x08http/1.1"));
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
                    Some(HttpProtocol::Http1),
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
                Some(HttpProtocol::Http1),
            )
            .unwrap();
            let server = server.join(cx);
            run_http1_in_scope(cx, iters, body_len, client, server, None)
        })
    })
}

fn run_http1(
    iters: u64,
    body_len: usize,
    client: StackTransport,
    server: StackTransport,
    deadline: Option<Duration>,
) -> Duration {
    let mut runtime = Runtime::new();
    runtime.block_on(|cx| run_http1_in_scope(cx, iters, body_len, client, server, deadline))
}

fn run_http1_in_scope(
    cx: &RuntimeContext<'_>,
    iters: u64,
    body_len: usize,
    client: StackTransport,
    server: StackTransport,
    deadline: Option<Duration>,
) -> Duration {
    let request = request("/bench-http1", body_len);
    let response = response(body_len);
    let rounds = iters.saturating_add(1);

    cx.scope(|scope| {
        let server = scope.spawn(move |cx| {
            let mut server = http1::ServerConnection::new(server, HttpConfig::default());
            for _ in 0..rounds {
                let request = server.read_request(cx).unwrap().unwrap();
                black_box(request.body().len());
                server.write_response(cx, &response).unwrap();
            }
            server.close(cx).unwrap();
        });

        if let Some(deadline) = deadline {
            let client = scope.spawn(move |cx| {
                let mut client = http1::ClientConnection::new(client, HttpConfig::default());
                client.set_io_deadline(Some(Instant::now() + deadline));
                black_box(client.send(cx, &request).unwrap());
                let start = Instant::now();
                for _ in 0..iters {
                    black_box(client.send(cx, &request).unwrap());
                }
                let elapsed = start.elapsed();
                client.close(cx).unwrap();
                elapsed
            });
            server.join(cx);
            client.join(cx)
        } else {
            let mut client = http1::ClientConnection::new(client, HttpConfig::default());
            black_box(client.send(cx, &request).unwrap());
            let start = Instant::now();
            for _ in 0..iters {
                black_box(client.send(cx, &request).unwrap());
            }
            let elapsed = start.elapsed();
            client.close(cx).unwrap();
            server.join(cx);
            elapsed
        }
    })
}

fn run_h2_plaintext(iters: u64, body_len: usize) -> Duration {
    let (client, server) = socket_transport_pair();
    run_h2(iters, body_len, client, server, None)
}

fn run_h2_plaintext_with_deadline(iters: u64, body_len: usize) -> Duration {
    let (client, server) = socket_transport_pair();
    run_h2(iters, body_len, client, server, Some(BENCH_DEADLINE))
}

fn run_h2_tls(iters: u64, body_len: usize) -> Duration {
    let contexts = tls_contexts(Some(b"\x02h2"));
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
            let server = server.join(cx);
            run_h2_in_scope(cx, iters, body_len, client, server, None)
        })
    })
}

fn run_h2(
    iters: u64,
    body_len: usize,
    client: StackTransport,
    server: StackTransport,
    deadline: Option<Duration>,
) -> Duration {
    let mut runtime = Runtime::new();
    runtime.block_on(|cx| run_h2_in_scope(cx, iters, body_len, client, server, deadline))
}

fn run_h2_in_scope(
    cx: &RuntimeContext<'_>,
    iters: u64,
    body_len: usize,
    client: StackTransport,
    server: StackTransport,
    deadline: Option<Duration>,
) -> Duration {
    let request = request("/bench-h2", body_len);
    let response = response(body_len);
    let rounds = iters.saturating_add(1);

    cx.scope(|scope| {
        let server = scope.spawn(move |cx| {
            let mut server = h2::ServerConnection::new(server, HttpConfig::default());
            for _ in 0..rounds {
                let incoming = server.accept(cx).unwrap().unwrap();
                server
                    .send_response(cx, incoming.stream_id, &response)
                    .unwrap();
            }
            server.shutdown_write_and_close_after_peer(cx).unwrap();
        });

        if let Some(deadline) = deadline {
            let client = scope.spawn(move |cx| {
                let mut client = h2::ClientConnection::new(client, HttpConfig::default());
                client.set_io_deadline(Some(Instant::now() + deadline));
                black_box(client.send(cx, &request).unwrap());
                let start = Instant::now();
                for _ in 0..iters {
                    black_box(client.send(cx, &request).unwrap());
                }
                let elapsed = start.elapsed();
                client.close(cx).unwrap();
                elapsed
            });
            server.join(cx);
            client.join(cx)
        } else {
            let mut client = h2::ClientConnection::new(client, HttpConfig::default());
            black_box(client.send(cx, &request).unwrap());
            let start = Instant::now();
            for _ in 0..iters {
                black_box(client.send(cx, &request).unwrap());
            }
            let elapsed = start.elapsed();
            client.close(cx).unwrap();
            server.join(cx);
            elapsed
        }
    })
}

fn run_http1_steal_plaintext(iters: u64, body_len: usize, path: StealPath) -> Duration {
    let (client_fd, server_fd) = socketpair_fds();
    let mut runtime = steal_runtime();
    runtime.block_on(|cx| {
        cx.scope(|scope| match path {
            StealPath::WorkerLocal => {
                let server = scope.spawn_stealable(move |cx| {
                    let server_fd = cx.socket_from_owned_fd(server_fd).unwrap();
                    let server = RuntimeStackTransport::plaintext_socket(server_fd);
                    run_http1_steal_server(cx, iters, body_len, server);
                });
                let client = scope.spawn_stealable(move |cx| {
                    let client_fd = cx.socket_from_owned_fd(client_fd).unwrap();
                    let client = RuntimeStackTransport::plaintext_socket(client_fd);
                    run_http1_steal_client(cx, iters, body_len, client)
                });
                server.join(cx);
                client.join(cx)
            }
            StealPath::SharedRoot => {
                let server = scope.spawn(move |cx| {
                    let server_fd = cx.socket_from_owned_fd(server_fd).unwrap();
                    let server = RuntimeStackTransport::plaintext_socket(server_fd);
                    run_http1_steal_server(cx, iters, body_len, server);
                });
                let client = scope.spawn(move |cx| {
                    let client_fd = cx.socket_from_owned_fd(client_fd).unwrap();
                    let client = RuntimeStackTransport::plaintext_socket(client_fd);
                    run_http1_steal_client(cx, iters, body_len, client)
                });
                server.join(cx);
                client.join(cx)
            }
        })
    })
}

fn run_http1_steal_server(
    cx: &kimojio_stack_steal::RuntimeContext<'_>,
    iters: u64,
    body_len: usize,
    server: RuntimeStackTransport<kimojio_stack_steal::RingFd>,
) {
    let response = response(body_len);
    let rounds = iters.saturating_add(1);
    let mut server = http1::RuntimeServerConnection::new(server, HttpConfig::default());
    for _ in 0..rounds {
        let request = server.read_request(cx).unwrap().unwrap();
        black_box(request.body().len());
        server.write_response(cx, &response).unwrap();
    }
    server.close(cx).unwrap();
}

fn run_http1_steal_client(
    cx: &kimojio_stack_steal::RuntimeContext<'_>,
    iters: u64,
    body_len: usize,
    client: RuntimeStackTransport<kimojio_stack_steal::RingFd>,
) -> Duration {
    let request = request("/bench-http1-steal", body_len);
    let mut client = http1::RuntimeClientConnection::new(client, HttpConfig::default());
    black_box(client.send(cx, &request).unwrap());
    let start = Instant::now();
    for _ in 0..iters {
        black_box(client.send(cx, &request).unwrap());
    }
    let elapsed = start.elapsed();
    client.close(cx).unwrap();
    elapsed
}

fn run_h2_steal_plaintext(iters: u64, body_len: usize, path: StealPath) -> Duration {
    let (client_fd, server_fd) = socketpair_fds();
    let mut runtime = steal_runtime();
    runtime.block_on(|cx| {
        cx.scope(|scope| match path {
            StealPath::WorkerLocal => {
                let server = scope.spawn_stealable(move |cx| {
                    let server_fd = cx.socket_from_owned_fd(server_fd).unwrap();
                    let server = RuntimeStackTransport::plaintext_socket(server_fd);
                    run_h2_steal_server(cx, iters, body_len, server);
                });
                let client = scope.spawn_stealable(move |cx| {
                    let client_fd = cx.socket_from_owned_fd(client_fd).unwrap();
                    let client = RuntimeStackTransport::plaintext_socket(client_fd);
                    run_h2_steal_client(cx, iters, body_len, client)
                });
                server.join(cx);
                client.join(cx)
            }
            StealPath::SharedRoot => {
                let server = scope.spawn(move |cx| {
                    let server_fd = cx.socket_from_owned_fd(server_fd).unwrap();
                    let server = RuntimeStackTransport::plaintext_socket(server_fd);
                    run_h2_steal_server(cx, iters, body_len, server);
                });
                let client = scope.spawn(move |cx| {
                    let client_fd = cx.socket_from_owned_fd(client_fd).unwrap();
                    let client = RuntimeStackTransport::plaintext_socket(client_fd);
                    run_h2_steal_client(cx, iters, body_len, client)
                });
                server.join(cx);
                client.join(cx)
            }
        })
    })
}

fn run_h2_steal_server(
    cx: &kimojio_stack_steal::RuntimeContext<'_>,
    iters: u64,
    body_len: usize,
    server: RuntimeStackTransport<kimojio_stack_steal::RingFd>,
) {
    let response = response(body_len);
    let rounds = iters.saturating_add(1);
    let mut server = h2::RuntimeServerConnection::new(server, HttpConfig::default());
    for _ in 0..rounds {
        let incoming = server.accept(cx).unwrap().unwrap();
        server
            .send_response(cx, incoming.stream_id, &response)
            .unwrap();
    }
    server.shutdown_write_and_close_after_peer(cx).unwrap();
}

fn run_h2_steal_client(
    cx: &kimojio_stack_steal::RuntimeContext<'_>,
    iters: u64,
    body_len: usize,
    client: RuntimeStackTransport<kimojio_stack_steal::RingFd>,
) -> Duration {
    let request = request("/bench-h2-steal", body_len);
    let mut client = h2::RuntimeClientConnection::new(client, HttpConfig::default());
    black_box(client.send(cx, &request).unwrap());
    let start = Instant::now();
    for _ in 0..iters {
        black_box(client.send(cx, &request).unwrap());
    }
    let elapsed = start.elapsed();
    client.close(cx).unwrap();
    elapsed
}

fn steal_runtime() -> StealRuntime {
    StealRuntime::with_config(StealRuntimeConfig {
        workers: NonZeroUsize::new(2).unwrap(),
        steal_policy: StealPolicy::steal_one(),
        ..StealRuntimeConfig::default()
    })
}

fn request(uri: &str, body_len: usize) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("x-bench", "request")
        .body(body(body_len))
        .unwrap()
}

fn response(body_len: usize) -> Response<Body> {
    Response::builder()
        .status(StatusCode::OK)
        .header("x-bench", "response")
        .body(body(body_len))
        .unwrap()
}

fn body(len: usize) -> Body {
    Body::from_bytes(bytes::Bytes::from(vec![b'x'; len]), BodyLimits::new(len)).unwrap()
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

fn tls_contexts(alpn: Option<&'static [u8]>) -> TlsContexts {
    let (cert, key) = self_signed_cert();
    let mut acceptor = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).unwrap();
    acceptor.set_private_key(&key).unwrap();
    acceptor.set_certificate(&cert).unwrap();
    acceptor.check_private_key().unwrap();
    if let Some(protos) = alpn {
        acceptor.set_alpn_select_callback(move |_ssl, client| {
            select_next_proto(protos, client).ok_or(AlpnError::NOACK)
        });
    }

    let mut connector = SslConnector::builder(SslMethod::tls()).unwrap();
    connector.set_verify(SslVerifyMode::NONE);
    if let Some(protos) = alpn {
        connector.set_alpn_protos(protos).unwrap();
    }

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
    targets = bench_http
);
criterion_main!(benches);
