// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::{
    hint::black_box,
    time::{Duration, Instant},
};

use criterion::{Criterion, criterion_group, criterion_main};
use kimojio_stack::{Runtime, RuntimeContext};
use openssl::{
    asn1::Asn1Time,
    hash::MessageDigest,
    nid::Nid,
    pkey::{PKey, Private},
    rsa::Rsa,
    ssl::{SslAcceptor, SslConnector, SslContext, SslMethod, SslVerifyMode},
    x509::{X509, X509NameBuilder},
};
use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};

const TLS_BUFFER_SIZE: usize = 16 * 1024;

fn run_stackful(f: impl FnOnce(&RuntimeContext<'_>) -> Duration) -> Duration {
    let mut runtime = Runtime::new();
    runtime.block_on(f)
}

fn make_cert() -> (X509, PKey<Private>) {
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

fn make_openssl_contexts() -> (SslContext, SslContext) {
    let (cert, key) = make_cert();

    let mut acceptor = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).unwrap();
    acceptor.set_private_key(&key).unwrap();
    acceptor.set_certificate(&cert).unwrap();
    acceptor.check_private_key().unwrap();

    let mut connector = SslConnector::builder(SslMethod::tls()).unwrap();
    connector.set_verify(SslVerifyMode::NONE);

    (
        acceptor.build().into_context(),
        connector.build().into_context(),
    )
}

fn make_memory_bio_contexts() -> (kimojio_stack_tls::TlsContext, kimojio_stack_tls::TlsContext) {
    let (server, client) = make_openssl_contexts();
    (
        kimojio_stack_tls::TlsContext::from_openssl(server),
        kimojio_stack_tls::TlsContext::from_openssl(client),
    )
}

fn make_direct_bio_contexts() -> (
    kimojio_stack_tls2::TlsContext,
    kimojio_stack_tls2::TlsContext,
) {
    let (server, client) = make_openssl_contexts();
    (
        kimojio_stack_tls2::TlsContext::from_openssl(server),
        kimojio_stack_tls2::TlsContext::from_openssl(client),
    )
}

fn bench_memory_bio_echo(iters: u64, message_len: usize) -> Duration {
    let (server_ctx, client_ctx) = make_memory_bio_contexts();
    let message = vec![0x5a; message_len];

    run_stackful(|cx| {
        let (client_fd, server_fd) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::empty(),
            None,
        )
        .unwrap();

        cx.scope(|scope| {
            let server = scope.spawn(move |cx| {
                let mut tls = server_ctx
                    .server(cx, TLS_BUFFER_SIZE, server_fd)
                    .expect("memory BIO server handshake failed");
                let mut buf = vec![0_u8; message_len];
                for _ in 0..iters {
                    let amount = tls
                        .read_exact_or_eof(cx, &mut buf)
                        .expect("memory BIO server read failed");
                    assert_eq!(amount, message_len);
                    tls.write(cx, &buf).expect("memory BIO server write failed");
                }
                tls.shutdown(cx).expect("memory BIO server shutdown failed");
                tls.close(cx).expect("memory BIO server close failed");
            });

            let mut tls = client_ctx
                .client(cx, TLS_BUFFER_SIZE, client_fd)
                .expect("memory BIO client handshake failed");
            let mut buf = vec![0_u8; message_len];

            let start = Instant::now();
            for _ in 0..iters {
                tls.write(cx, &message)
                    .expect("memory BIO client write failed");
                let amount = tls
                    .read_exact_or_eof(cx, &mut buf)
                    .expect("memory BIO client read failed");
                assert_eq!(amount, message_len);
                black_box(buf[0]);
            }
            let elapsed = start.elapsed();

            tls.shutdown(cx).expect("memory BIO client shutdown failed");
            tls.close(cx).expect("memory BIO client close failed");
            server.join(cx).unwrap();
            elapsed
        })
    })
}

fn bench_direct_bio_echo(iters: u64, message_len: usize) -> Duration {
    let (server_ctx, client_ctx) = make_direct_bio_contexts();
    let message = vec![0x5a; message_len];

    run_stackful(|cx| {
        let (client_fd, server_fd) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::empty(),
            None,
        )
        .unwrap();

        cx.scope(|scope| {
            let server = scope.spawn(move |cx| {
                let mut tls = server_ctx
                    .server(cx, server_fd)
                    .expect("direct BIO server handshake failed");
                let mut buf = vec![0_u8; message_len];
                for _ in 0..iters {
                    let amount = tls
                        .read_exact_or_eof(&mut buf)
                        .expect("direct BIO server read failed");
                    assert_eq!(amount, message_len);
                    tls.write_all(&buf).expect("direct BIO server write failed");
                }
                tls.shutdown().expect("direct BIO server shutdown failed");
                tls.close().expect("direct BIO server close failed");
            });

            let mut tls = client_ctx
                .client(cx, client_fd)
                .expect("direct BIO client handshake failed");
            let mut buf = vec![0_u8; message_len];

            let start = Instant::now();
            for _ in 0..iters {
                tls.write_all(&message)
                    .expect("direct BIO client write failed");
                let amount = tls
                    .read_exact_or_eof(&mut buf)
                    .expect("direct BIO client read failed");
                assert_eq!(amount, message_len);
                black_box(buf[0]);
            }
            let elapsed = start.elapsed();

            tls.shutdown().expect("direct BIO client shutdown failed");
            tls.close().expect("direct BIO client close failed");
            server.join(cx).unwrap();
            elapsed
        })
    })
}

fn bench_direct_bio_fragments(iters: u64, body_len: usize, corked: bool) -> Duration {
    let (server_ctx, client_ctx) = make_direct_bio_contexts();
    let body = vec![0x5a; body_len];
    let expected_len = b"header-a ".len() + b"header-b ".len() + body_len.max(b"small-tail".len());

    run_stackful(|cx| {
        let (client_fd, server_fd) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::empty(),
            None,
        )
        .unwrap();

        cx.scope(|scope| {
            let server = scope.spawn(move |cx| {
                let mut tls = server_ctx
                    .server(cx, server_fd)
                    .expect("direct BIO server handshake failed");
                let mut buf = [0_u8; 1024];
                for _ in 0..iters {
                    let mut read = 0;
                    while read < expected_len {
                        let amount = tls.read(&mut buf).expect("direct BIO server read failed");
                        assert_ne!(amount, 0);
                        read += amount;
                        tls.write_all(&buf[..amount])
                            .expect("direct BIO server write failed");
                    }
                }
                tls.shutdown().expect("direct BIO server shutdown failed");
                tls.close().expect("direct BIO server close failed");
            });

            let mut tls = client_ctx
                .client(cx, client_fd)
                .expect("direct BIO client handshake failed");
            let mut buf = vec![0_u8; expected_len];

            let start = Instant::now();
            for _ in 0..iters {
                if corked {
                    tls.write_corked(|tls| {
                        tls.write_buffered(b"header-a ")?;
                        tls.write_buffered(b"header-b ")?;
                        if body.is_empty() {
                            tls.write_buffered(b"small-tail")?;
                        } else {
                            tls.write_all(&body)?;
                        }
                        Ok(())
                    })
                    .expect("direct BIO corked client write failed");
                } else {
                    tls.write_all(b"header-a ")
                        .expect("direct BIO client header write failed");
                    tls.write_all(b"header-b ")
                        .expect("direct BIO client header write failed");
                    if body.is_empty() {
                        tls.write_all(b"small-tail")
                            .expect("direct BIO client tail write failed");
                    } else {
                        tls.write_all(&body)
                            .expect("direct BIO client body write failed");
                    }
                }

                let amount = tls
                    .read_exact_or_eof(&mut buf)
                    .expect("direct BIO client read failed");
                assert_eq!(amount, expected_len);
                black_box(buf[0]);
            }
            let elapsed = start.elapsed();

            tls.shutdown().expect("direct BIO client shutdown failed");
            tls.close().expect("direct BIO client close failed");
            server.join(cx).unwrap();
            elapsed
        })
    })
}

fn benchmark(c: &mut Criterion) {
    for message_len in [64, 1024, 16 * 1024] {
        c.bench_function(&format!("tls/memory_bio_echo_{message_len}b"), |b| {
            b.iter_custom(|iters| bench_memory_bio_echo(iters, message_len));
        });

        c.bench_function(&format!("tls/direct_bio_echo_{message_len}b"), |b| {
            b.iter_custom(|iters| bench_direct_bio_echo(iters, message_len));
        });
    }

    c.bench_function("tls/direct_bio_uncorked_fragments_64b", |b| {
        b.iter_custom(|iters| bench_direct_bio_fragments(iters, 0, false));
    });
    c.bench_function("tls/direct_bio_corked_fragments_64b", |b| {
        b.iter_custom(|iters| bench_direct_bio_fragments(iters, 0, true));
    });
    c.bench_function("tls/direct_bio_uncorked_header_body_16384b", |b| {
        b.iter_custom(|iters| bench_direct_bio_fragments(iters, 16 * 1024, false));
    });
    c.bench_function("tls/direct_bio_corked_header_body_16384b", |b| {
        b.iter_custom(|iters| bench_direct_bio_fragments(iters, 16 * 1024, true));
    });
}

criterion_group!(
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(4));
    targets = benchmark
);
criterion_main!(benches);
