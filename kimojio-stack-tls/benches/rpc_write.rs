// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::{
    mem::size_of,
    sync::{Arc, Barrier},
    thread,
    time::{Duration, Instant},
};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use kimojio_stack::{Runtime, RuntimeContext};
use kimojio_stack_tls::{TlsContext, TlsOffloadConfig, TlsOffloadPool, TlsOffloadStats, TlsStream};
use openssl::{
    asn1::Asn1Time,
    hash::MessageDigest,
    nid::Nid,
    pkey::PKey,
    rsa::Rsa,
    ssl::{SslAcceptor, SslConnector, SslContext, SslMethod, SslVerifyMode},
    x509::{X509, X509NameBuilder},
};
use rustix::{
    fd::OwnedFd,
    net::{AddressFamily, SocketFlags, SocketType, socketpair},
};

const CONNECTIONS: usize = 3;
const TLS_BUFFER_SIZE: usize = 32 * 1024;
const OFFLOAD_THRESHOLD_8K: usize = 8 * 1024;
const OFFLOAD_THRESHOLD_24K: usize = 24 * 1024;
const OFFLOAD_WORKERS: usize = 3;
const RPC_HEADER_LEN: usize = 64;
const RPC_RESPONSE_LEN: usize = 64;
const BODY_SIZES: [usize; 5] = [8 * 1024, 16 * 1024, 23 * 1024, 24 * 1024, 32 * 1024];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OffloadMode {
    Inline,
    Threshold8k,
    Threshold24k,
    Always,
}

impl OffloadMode {
    fn name(self) -> &'static str {
        match self {
            Self::Inline => "inline",
            Self::Threshold8k => "offload_threshold_8k",
            Self::Threshold24k => "offload_threshold_24k",
            Self::Always => "offload_always",
        }
    }

    fn needs_pool(self) -> bool {
        self != Self::Inline
    }

    fn config(self, pool: &TlsOffloadPool) -> Option<TlsOffloadConfig> {
        match self {
            Self::Inline => None,
            Self::Threshold8k => Some(
                TlsOffloadConfig::new(pool.clone())
                    .with_read_threshold(OFFLOAD_THRESHOLD_8K)
                    .with_write_threshold(OFFLOAD_THRESHOLD_8K),
            ),
            Self::Threshold24k => Some(TlsOffloadConfig::large_records(pool.clone())),
            Self::Always => Some(TlsOffloadConfig::always(pool.clone())),
        }
    }

    fn threshold(self) -> Option<usize> {
        match self {
            Self::Threshold8k => Some(OFFLOAD_THRESHOLD_8K),
            Self::Threshold24k => Some(OFFLOAD_THRESHOLD_24K),
            Self::Inline | Self::Always => None,
        }
    }
}

struct SslContexts {
    server: SslContext,
    client: SslContext,
}

fn make_ssl_contexts() -> SslContexts {
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

    SslContexts {
        server: acceptor.build().into_context(),
        client: connector.build().into_context(),
    }
}

fn stack_tls_context(ctx: SslContext) -> TlsContext {
    TlsContext::from_openssl(ctx)
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

fn split_iters(iters: u64) -> [u64; CONNECTIONS] {
    let base = iters / CONNECTIONS as u64;
    let mut counts = [base; CONNECTIONS];
    for count in counts
        .iter_mut()
        .take((iters % CONNECTIONS as u64) as usize)
    {
        *count += 1;
    }
    counts
}

fn make_socketpairs() -> ([OwnedFd; CONNECTIONS], [OwnedFd; CONNECTIONS]) {
    let mut client_fds = Vec::with_capacity(CONNECTIONS);
    let mut server_fds = Vec::with_capacity(CONNECTIONS);

    for _ in 0..CONNECTIONS {
        let (client_fd, server_fd) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::empty(),
            None,
        )
        .unwrap();
        client_fds.push(client_fd);
        server_fds.push(server_fd);
    }

    (
        client_fds.try_into().ok().unwrap(),
        server_fds.try_into().ok().unwrap(),
    )
}

fn client_stream(
    cx: &RuntimeContext<'_>,
    ctx: &TlsContext,
    socket: OwnedFd,
    offload: Option<TlsOffloadConfig>,
) -> TlsStream {
    match offload {
        Some(offload) => ctx
            .client_with_offload(cx, TLS_BUFFER_SIZE, socket, offload)
            .unwrap(),
        None => ctx.client(cx, TLS_BUFFER_SIZE, socket).unwrap(),
    }
}

fn server_stream(
    cx: &RuntimeContext<'_>,
    ctx: &TlsContext,
    socket: OwnedFd,
    offload: Option<TlsOffloadConfig>,
) -> TlsStream {
    match offload {
        Some(offload) => ctx
            .server_with_offload(cx, TLS_BUFFER_SIZE, socket, offload)
            .unwrap(),
        None => ctx.server(cx, TLS_BUFFER_SIZE, socket).unwrap(),
    }
}

fn client_rpc_loop(
    cx: &RuntimeContext<'_>,
    tls: &mut TlsStream,
    count: u64,
    header: &[u8; RPC_HEADER_LEN],
    body: &[u8],
) {
    let mut response = [0_u8; RPC_RESPONSE_LEN];
    for _ in 0..count {
        tls.write(cx, header).unwrap();
        tls.write(cx, body).unwrap();
        let amount = tls.read_exact_or_eof(cx, &mut response).unwrap();
        assert_eq!(amount, RPC_RESPONSE_LEN);
    }
}

fn server_rpc_loop(
    cx: &RuntimeContext<'_>,
    tls: &mut TlsStream,
    count: u64,
    header: &mut [u8; RPC_HEADER_LEN],
    body: &mut Vec<u8>,
) {
    let response = [0x7b; RPC_RESPONSE_LEN];
    for _ in 0..count {
        let amount = tls.read_exact_or_eof(cx, header).unwrap();
        assert_eq!(amount, RPC_HEADER_LEN);

        let body_len = rpc_body_len(header);
        if body.len() < body_len {
            body.resize(body_len, 0);
        }
        let amount = tls.read_exact_or_eof(cx, &mut body[..body_len]).unwrap();
        assert_eq!(amount, body_len);

        tls.write(cx, &response).unwrap();
    }
}

fn run_server_connection(server_ctx: SslContext, socket: OwnedFd, count: u64, start: Arc<Barrier>) {
    let server_ctx = stack_tls_context(server_ctx);
    let mut runtime = Runtime::new();

    runtime.block_on(|cx| {
        let mut tls = server_stream(cx, &server_ctx, socket, None);
        start.wait();

        let mut header = [0_u8; RPC_HEADER_LEN];
        let mut body = Vec::new();
        server_rpc_loop(cx, &mut tls, count, &mut header, &mut body);
        tls.close(cx).unwrap();
    });
}

fn run_client_fanout(iters: u64, body_len: usize, mode: OffloadMode) -> Duration {
    let contexts = make_ssl_contexts();
    let (client_fds, server_fds) = make_socketpairs();
    let counts = split_iters(iters);
    let start = Arc::new(Barrier::new(CONNECTIONS + 1));

    let servers = server_fds
        .into_iter()
        .zip(counts)
        .map(|(server_fd, count)| {
            let server_ctx = contexts.server.clone();
            let start = Arc::clone(&start);
            thread::spawn(move || run_server_connection(server_ctx, server_fd, count, start))
        })
        .collect::<Vec<_>>();

    let client_ctx = stack_tls_context(contexts.client.clone());
    let client_pool = mode
        .needs_pool()
        .then(|| TlsOffloadPool::new(OFFLOAD_WORKERS).unwrap());
    let header = make_rpc_header(body_len);
    let bodies = (0..CONNECTIONS)
        .map(|_| vec![0x5a; body_len])
        .collect::<Vec<_>>();
    let mut runtime = Runtime::new();

    let duration = runtime.block_on(|cx| {
        let streams = client_fds
            .into_iter()
            .map(|fd| {
                let offload = client_pool.as_ref().and_then(|pool| mode.config(pool));
                client_stream(cx, &client_ctx, fd, offload)
            })
            .collect::<Vec<_>>();

        start.wait();
        let started = Instant::now();
        let stats = cx.scope(|scope| {
            let mut handles = Vec::with_capacity(CONNECTIONS);
            for ((mut tls, count), body) in streams.into_iter().zip(counts).zip(bodies) {
                handles.push(scope.spawn(move |cx| {
                    client_rpc_loop(cx, &mut tls, count, &header, &body);
                    let stats = tls.offload_stats();
                    tls.close(cx).unwrap();
                    stats
                }));
            }

            handles
                .into_iter()
                .map(|handle| handle.join(cx).unwrap())
                .collect::<Vec<_>>()
        });
        let duration = started.elapsed();

        assert_client_fanout_stats(mode, body_len, &stats);
        duration
    });

    for server in servers {
        server.join().unwrap();
    }

    duration
}

fn run_client_connection(
    client_ctx: SslContext,
    socket: OwnedFd,
    count: u64,
    body_len: usize,
    start: Arc<Barrier>,
) {
    let client_ctx = stack_tls_context(client_ctx);
    let header = make_rpc_header(body_len);
    let body = vec![0x5a; body_len];
    let mut runtime = Runtime::new();

    runtime.block_on(|cx| {
        let mut tls = client_stream(cx, &client_ctx, socket, None);
        start.wait();

        client_rpc_loop(cx, &mut tls, count, &header, &body);
        tls.close(cx).unwrap();
    });
}

fn run_server_fanin(iters: u64, body_len: usize, mode: OffloadMode) -> Duration {
    let contexts = make_ssl_contexts();
    let (client_fds, server_fds) = make_socketpairs();
    let counts = split_iters(iters);
    let start = Arc::new(Barrier::new(CONNECTIONS + 1));

    let clients = client_fds
        .into_iter()
        .zip(counts)
        .map(|(client_fd, count)| {
            let client_ctx = contexts.client.clone();
            let start = Arc::clone(&start);
            thread::spawn(move || {
                run_client_connection(client_ctx, client_fd, count, body_len, start)
            })
        })
        .collect::<Vec<_>>();

    let server_ctx = stack_tls_context(contexts.server.clone());
    let server_pool = mode
        .needs_pool()
        .then(|| TlsOffloadPool::new(OFFLOAD_WORKERS).unwrap());
    let mut runtime = Runtime::new();

    let duration = runtime.block_on(|cx| {
        let streams = server_fds
            .into_iter()
            .map(|fd| {
                let offload = server_pool.as_ref().and_then(|pool| mode.config(pool));
                server_stream(cx, &server_ctx, fd, offload)
            })
            .collect::<Vec<_>>();

        start.wait();
        let started = Instant::now();
        let stats = cx.scope(|scope| {
            let mut handles = Vec::with_capacity(CONNECTIONS);
            for (mut tls, count) in streams.into_iter().zip(counts) {
                handles.push(scope.spawn(move |cx| {
                    let mut header = [0_u8; RPC_HEADER_LEN];
                    let mut body = Vec::new();
                    server_rpc_loop(cx, &mut tls, count, &mut header, &mut body);
                    let stats = tls.offload_stats();
                    tls.close(cx).unwrap();
                    stats
                }));
            }

            handles
                .into_iter()
                .map(|handle| handle.join(cx).unwrap())
                .collect::<Vec<_>>()
        });
        let duration = started.elapsed();

        assert_server_fanin_stats(mode, body_len, &stats);
        duration
    });

    for client in clients {
        client.join().unwrap();
    }

    duration
}

fn sum_stats(stats: &[TlsOffloadStats]) -> TlsOffloadStats {
    stats
        .iter()
        .fold(TlsOffloadStats::default(), |mut total, stats| {
            total.inline_reads += stats.inline_reads;
            total.inline_writes += stats.inline_writes;
            total.offloaded_reads += stats.offloaded_reads;
            total.offloaded_writes += stats.offloaded_writes;
            total
        })
}

fn assert_client_fanout_stats(mode: OffloadMode, body_len: usize, stats: &[TlsOffloadStats]) {
    let stats = sum_stats(stats);
    match mode {
        OffloadMode::Inline => {
            assert_eq!(stats.offloaded_reads, 0);
            assert_eq!(stats.offloaded_writes, 0);
        }
        OffloadMode::Threshold8k | OffloadMode::Threshold24k => {
            if body_len >= mode.threshold().unwrap() {
                assert!(stats.offloaded_writes > 0);
            } else {
                assert_eq!(stats.offloaded_writes, 0);
            }
            assert_eq!(stats.offloaded_reads, 0);
        }
        OffloadMode::Always => {
            assert!(stats.offloaded_reads > 0);
            assert!(stats.offloaded_writes > 0);
        }
    }
}

fn assert_server_fanin_stats(mode: OffloadMode, body_len: usize, stats: &[TlsOffloadStats]) {
    let stats = sum_stats(stats);
    match mode {
        OffloadMode::Inline => {
            assert_eq!(stats.offloaded_reads, 0);
            assert_eq!(stats.offloaded_writes, 0);
        }
        OffloadMode::Threshold8k | OffloadMode::Threshold24k => {
            if body_len >= mode.threshold().unwrap() {
                assert!(stats.offloaded_reads > 0);
            } else {
                assert_eq!(stats.offloaded_reads, 0);
            }
            assert_eq!(stats.offloaded_writes, 0);
        }
        OffloadMode::Always => {
            assert!(stats.offloaded_reads > 0);
            assert!(stats.offloaded_writes > 0);
        }
    }
}

fn bench_client_fanout(c: &mut Criterion) {
    let mut group = c.benchmark_group("rpc_write/client_fanout");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(2));

    for body_len in BODY_SIZES {
        group.throughput(Throughput::Bytes(
            (RPC_HEADER_LEN + body_len + RPC_RESPONSE_LEN) as u64,
        ));
        for mode in [
            OffloadMode::Inline,
            OffloadMode::Threshold8k,
            OffloadMode::Threshold24k,
            OffloadMode::Always,
        ] {
            group.bench_with_input(
                BenchmarkId::new(mode.name(), body_len),
                &(body_len, mode),
                |b, &(body_len, mode)| {
                    b.iter_custom(|iters| run_client_fanout(iters, body_len, mode));
                },
            );
        }
    }

    group.finish();
}

fn bench_server_fanin(c: &mut Criterion) {
    let mut group = c.benchmark_group("rpc_write/server_fanin");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(2));

    for body_len in BODY_SIZES {
        group.throughput(Throughput::Bytes(
            (RPC_HEADER_LEN + body_len + RPC_RESPONSE_LEN) as u64,
        ));
        for mode in [
            OffloadMode::Inline,
            OffloadMode::Threshold8k,
            OffloadMode::Threshold24k,
            OffloadMode::Always,
        ] {
            group.bench_with_input(
                BenchmarkId::new(mode.name(), body_len),
                &(body_len, mode),
                |b, &(body_len, mode)| {
                    b.iter_custom(|iters| run_server_fanin(iters, body_len, mode));
                },
            );
        }
    }

    group.finish();
}

criterion_group!(benches, bench_client_fanout, bench_server_fanin);
criterion_main!(benches);
