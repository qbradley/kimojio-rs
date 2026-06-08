// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::{
    hint::black_box,
    time::{Duration, Instant},
};

use criterion::{Criterion, criterion_group, criterion_main};
use kimojio_stack::{IoReadBuffer, Runtime, RuntimeContext};
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

const TLS_BUFFER_SIZE: usize = 16 * 1024;
const SMALL_PAYLOAD: usize = 1024;
const LARGE_PAYLOAD: usize = 16 * 1024;

fn bench_tls_read_write(c: &mut Criterion) {
    for size in [SMALL_PAYLOAD, LARGE_PAYLOAD] {
        let label = if size == SMALL_PAYLOAD {
            "1KiB"
        } else {
            "16KiB"
        };

        c.bench_function(&format!("tls/read/{label}"), |b| {
            b.iter_custom(|iters| run_tls_read(iters, size));
        });
        c.bench_function(&format!("tls/read_async/{label}"), |b| {
            b.iter_custom(|iters| run_tls_read_async(iters, size));
        });
        c.bench_function(&format!("tls/write/{label}"), |b| {
            b.iter_custom(|iters| run_tls_write(iters, size));
        });
        c.bench_function(&format!("tls/write_async/{label}"), |b| {
            b.iter_custom(|iters| run_tls_write_async(iters, size));
        });
    }
}

fn run_tls_read(iters: u64, size: usize) -> Duration {
    let (server_ctx, client_ctx) = make_contexts();
    let (client_fd, server_fd) = socketpair_fds();
    let mut runtime = Runtime::new();

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let server = scope.spawn(move |cx| {
                let mut tls = server_ctx
                    .server(cx, TLS_BUFFER_SIZE, server_fd)
                    .expect("server TLS handshake failed");
                let payload = vec![0x5a; size];
                for _ in 0..iters {
                    assert_eq!(tls.write(cx, &payload).unwrap(), size);
                }
                tls.shutdown(cx).unwrap();
                tls.close(cx).unwrap();
            });

            let mut tls = client_ctx
                .client(cx, TLS_BUFFER_SIZE, client_fd, "localhost")
                .expect("client TLS handshake failed");
            let mut buffer = vec![0_u8; size];

            let start = Instant::now();
            for _ in 0..iters {
                let amount = tls.read_exact_or_eof(cx, &mut buffer).unwrap();
                assert_eq!(amount, size);
                black_box(&buffer);
            }
            let elapsed = start.elapsed();

            tls.shutdown(cx).unwrap();
            tls.close(cx).unwrap();
            server.join(cx).unwrap();
            elapsed
        })
    })
}

fn run_tls_read_async(iters: u64, size: usize) -> Duration {
    let (server_ctx, client_ctx) = make_contexts();
    let (client_fd, server_fd) = socketpair_fds();
    let mut runtime = Runtime::new();

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let server = scope.spawn(move |cx| {
                let mut tls = server_ctx
                    .server(cx, TLS_BUFFER_SIZE, server_fd)
                    .expect("server TLS handshake failed");
                let payload = vec![0x5a; size];
                for _ in 0..iters {
                    assert_eq!(tls.write(cx, &payload).unwrap(), size);
                }
                tls.shutdown(cx).unwrap();
                tls.close(cx).unwrap();
            });

            let mut tls = client_ctx
                .client(cx, TLS_BUFFER_SIZE, client_fd, "localhost")
                .expect("client TLS handshake failed");
            let mut buffer = vec![0_u8; size];

            let start = Instant::now();
            for _ in 0..iters {
                buffer = read_exact_async(cx, &mut tls, buffer, size);
                black_box(&buffer);
            }
            let elapsed = start.elapsed();

            tls.shutdown(cx).unwrap();
            tls.close(cx).unwrap();
            server.join(cx).unwrap();
            elapsed
        })
    })
}

fn run_tls_write(iters: u64, size: usize) -> Duration {
    let (server_ctx, client_ctx) = make_contexts();
    let (client_fd, server_fd) = socketpair_fds();
    let mut runtime = Runtime::new();

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let server = scope.spawn(move |cx| {
                let mut tls = server_ctx
                    .server(cx, TLS_BUFFER_SIZE, server_fd)
                    .expect("server TLS handshake failed");
                let mut buffer = vec![0_u8; size];
                for _ in 0..iters {
                    let amount = tls.read_exact_or_eof(cx, &mut buffer).unwrap();
                    assert_eq!(amount, size);
                    black_box(&buffer);
                }
                tls.shutdown(cx).unwrap();
                tls.close(cx).unwrap();
            });

            let mut tls = client_ctx
                .client(cx, TLS_BUFFER_SIZE, client_fd, "localhost")
                .expect("client TLS handshake failed");
            let payload = vec![0x5a; size];

            let start = Instant::now();
            for _ in 0..iters {
                assert_eq!(tls.write(cx, &payload).unwrap(), size);
                black_box(&payload);
            }
            let elapsed = start.elapsed();

            tls.shutdown(cx).unwrap();
            tls.close(cx).unwrap();
            server.join(cx).unwrap();
            elapsed
        })
    })
}

fn run_tls_write_async(iters: u64, size: usize) -> Duration {
    let (server_ctx, client_ctx) = make_contexts();
    let (client_fd, server_fd) = socketpair_fds();
    let mut runtime = Runtime::new();

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let server = scope.spawn(move |cx| {
                let mut tls = server_ctx
                    .server(cx, TLS_BUFFER_SIZE, server_fd)
                    .expect("server TLS handshake failed");
                let mut buffer = vec![0_u8; size];
                for _ in 0..iters {
                    let amount = tls.read_exact_or_eof(cx, &mut buffer).unwrap();
                    assert_eq!(amount, size);
                    black_box(&buffer);
                }
                tls.shutdown(cx).unwrap();
                tls.close(cx).unwrap();
            });

            let mut tls = client_ctx
                .client(cx, TLS_BUFFER_SIZE, client_fd, "localhost")
                .expect("client TLS handshake failed");
            let mut payload = vec![0x5a; size];

            let start = Instant::now();
            for _ in 0..iters {
                let output = tls.write_async(cx, payload).get(cx).unwrap();
                assert_eq!(output.bytes, size);
                payload = output.buffer;
                black_box(&payload);
            }
            let elapsed = start.elapsed();

            tls.shutdown(cx).unwrap();
            tls.close(cx).unwrap();
            server.join(cx).unwrap();
            elapsed
        })
    })
}

fn read_exact_async(
    cx: &RuntimeContext<'_>,
    tls: &mut kimojio_stack_tls::TlsStream,
    buffer: Vec<u8>,
    size: usize,
) -> Vec<u8> {
    let mut window = ReadWindow {
        buffer,
        offset: 0,
        len: size,
    };

    while window.len != 0 {
        let output = tls.read_async(cx, window).get(cx).unwrap();
        assert_ne!(output.bytes, 0);
        window = output.buffer;
        assert!(output.bytes <= window.len);
        window.offset += output.bytes;
        window.len -= output.bytes;
    }

    window.buffer
}

struct ReadWindow {
    buffer: Vec<u8>,
    offset: usize,
    len: usize,
}

unsafe impl IoReadBuffer for ReadWindow {
    fn io_buffer_mut_ptr(&mut self) -> *mut u8 {
        assert!(self.offset + self.len <= self.buffer.len());
        self.buffer.as_mut_ptr().wrapping_add(self.offset)
    }

    fn io_buffer_len(&self) -> usize {
        assert!(self.offset + self.len <= self.buffer.len());
        self.len
    }
}

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

fn socketpair_fds() -> (rustix::fd::OwnedFd, rustix::fd::OwnedFd) {
    socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::empty(),
        None,
    )
    .unwrap()
}

fn criterion_config() -> Criterion {
    Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(2))
}

criterion_group! {
    name = benches;
    config = criterion_config();
    targets = bench_tls_read_write
}
criterion_main!(benches);
