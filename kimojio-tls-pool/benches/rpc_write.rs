// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::mem::size_of;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Barrier, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use kimojio_tls_pool::{
    CompletionCallback, OperationResult, PlacementMode, PoolConfig, TlsPool, TlsStream,
};
use openssl::{
    asn1::Asn1Time,
    hash::MessageDigest,
    nid::Nid,
    pkey::PKey,
    rsa::Rsa,
    ssl::{SslAcceptor, SslConnector, SslMethod, SslVerifyMode},
    x509::{X509, X509NameBuilder},
};

const CONNECTIONS: usize = 3;
const THROUGHPUT_CONNECTIONS: usize = 8;
const EXECUTOR_COUNTS: [usize; 4] = [1, 2, 4, 8];
const RPC_HEADER_LEN: usize = 64;
const RPC_RESPONSE_LEN: usize = 64;
const BODY_SIZES: [usize; 5] = [4 * 1024, 8 * 1024, 16 * 1024, 24 * 1024, 32 * 1024];
const THROUGHPUT_BODY_LEN: usize = 32 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Immediate,
    Background,
    Adaptive,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Self::Immediate => "immediate",
            Self::Background => "background",
            Self::Adaptive => "adaptive",
        }
    }

    fn config(self) -> PoolConfig {
        match self {
            Self::Immediate => PoolConfig::new(1).with_placement_mode(PlacementMode::ImmediateOnly),
            Self::Background => {
                PoolConfig::new(3).with_placement_mode(PlacementMode::BackgroundOnly)
            }
            Self::Adaptive => PoolConfig::new(3).with_placement_mode(PlacementMode::Adaptive),
        }
    }
}

fn make_contexts() -> (SslAcceptor, SslConnector) {
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

    (acceptor.build(), connector.build())
}

fn stream_pair(mode: Mode) -> (TlsStream, TlsStream) {
    let (acceptor, connector) = make_contexts();
    let (client_io, server_io) = UnixStream::pair().unwrap();
    let server_config = mode.config();
    let server_thread = thread::spawn(move || {
        let pool = TlsPool::new(server_config).unwrap();
        pool.server(&acceptor, server_io).unwrap()
    });

    let client_pool = TlsPool::new(mode.config()).unwrap();
    let client = client_pool
        .client(&connector, "localhost", client_io)
        .unwrap();
    let server = server_thread.join().unwrap();
    (client, server)
}

fn stream_pair_with_pools(client_pool: &TlsPool, server_pool: &TlsPool) -> (TlsStream, TlsStream) {
    let (acceptor, connector) = make_contexts();
    let (client_io, server_io) = UnixStream::pair().unwrap();
    let server_pool = server_pool.clone();
    let server_thread = thread::spawn(move || server_pool.server(&acceptor, server_io).unwrap());

    let client = client_pool
        .client(&connector, "localhost", client_io)
        .unwrap();
    let server = server_thread.join().unwrap();
    (client, server)
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

fn write_blocking(stream: &TlsStream, bytes: &[u8]) -> OperationResult<usize> {
    let (sender, receiver) = mpsc::channel();
    stream.write(bytes.to_vec(), callback(sender))?;
    receiver.recv().unwrap()
}

fn read_once_blocking(stream: &TlsStream, len: usize) -> OperationResult<Vec<u8>> {
    let (sender, receiver) = mpsc::channel();
    stream.read(len, callback(sender))?;
    receiver.recv().unwrap()
}

fn read_exact_blocking(stream: &TlsStream, len: usize) -> OperationResult<Vec<u8>> {
    let mut output = Vec::with_capacity(len);
    while output.len() < len {
        let chunk = read_once_blocking(stream, len - output.len())?;
        assert!(!chunk.is_empty());
        output.extend_from_slice(&chunk);
    }
    Ok(output)
}

fn callback<T>(sender: mpsc::Sender<OperationResult<T>>) -> CompletionCallback<T>
where
    T: Send + 'static,
{
    Box::new(move |result| {
        sender.send(result).unwrap();
    })
}

fn client_rpc_loop(client: &TlsStream, count: u64, body_len: usize) {
    let header = make_rpc_header(body_len);
    let body = vec![0x5a; body_len];
    for _ in 0..count {
        write_blocking(client, &header).unwrap();
        write_blocking(client, &body).unwrap();
        let response = read_exact_blocking(client, RPC_RESPONSE_LEN).unwrap();
        assert_eq!(response, [0x7b; RPC_RESPONSE_LEN]);
    }
}

fn server_rpc_loop(server: &TlsStream, count: u64) {
    for _ in 0..count {
        let header = read_exact_blocking(server, RPC_HEADER_LEN).unwrap();
        let header: [u8; RPC_HEADER_LEN] = header.try_into().unwrap();
        let body_len = rpc_body_len(&header);
        let body = read_exact_blocking(server, body_len).unwrap();
        assert!(body.iter().all(|&byte| byte == 0x5a));
        write_blocking(server, &[0x7b; RPC_RESPONSE_LEN]).unwrap();
    }
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

fn split_iters_across(iters: u64, connections: usize) -> Vec<u64> {
    let base = iters / connections as u64;
    let mut counts = vec![base; connections];
    for count in counts
        .iter_mut()
        .take((iters % connections as u64) as usize)
    {
        *count += 1;
    }
    counts
}

fn run_single_pair(iters: u64, body_len: usize, mode: Mode) -> Duration {
    let (client, server) = stream_pair(mode);
    let barrier = Arc::new(Barrier::new(2));
    let server_barrier = Arc::clone(&barrier);
    let server_thread = thread::spawn(move || {
        server_barrier.wait();
        server_rpc_loop(&server, iters);
    });

    barrier.wait();
    let start = Instant::now();
    client_rpc_loop(&client, iters, body_len);
    let duration = start.elapsed();
    server_thread.join().unwrap();
    duration
}

fn run_three_pair(iters: u64, body_len: usize, mode: Mode) -> Duration {
    let counts = split_iters(iters);
    let barrier = Arc::new(Barrier::new(CONNECTIONS * 2 + 1));
    let mut clients = Vec::with_capacity(CONNECTIONS);
    let mut servers = Vec::with_capacity(CONNECTIONS);

    for count in counts {
        let (client, server) = stream_pair(mode);
        let client_barrier = Arc::clone(&barrier);
        let server_barrier = Arc::clone(&barrier);
        clients.push(thread::spawn(move || {
            client_barrier.wait();
            client_rpc_loop(&client, count, body_len);
        }));
        servers.push(thread::spawn(move || {
            server_barrier.wait();
            server_rpc_loop(&server, count);
        }));
    }

    barrier.wait();
    let start = Instant::now();
    for client in clients {
        client.join().unwrap();
    }
    for server in servers {
        server.join().unwrap();
    }
    start.elapsed()
}

fn run_three_pair_shared_pool(iters: u64, body_len: usize, mode: Mode) -> Duration {
    let counts = split_iters(iters);
    let barrier = Arc::new(Barrier::new(CONNECTIONS * 2 + 1));
    let client_pool = TlsPool::new(mode.config()).unwrap();
    let server_pool = TlsPool::new(mode.config()).unwrap();
    let mut clients = Vec::with_capacity(CONNECTIONS);
    let mut servers = Vec::with_capacity(CONNECTIONS);

    for count in counts {
        let (client, server) = stream_pair_with_pools(&client_pool, &server_pool);
        let client_barrier = Arc::clone(&barrier);
        let server_barrier = Arc::clone(&barrier);
        clients.push(thread::spawn(move || {
            client_barrier.wait();
            client_rpc_loop(&client, count, body_len);
        }));
        servers.push(thread::spawn(move || {
            server_barrier.wait();
            server_rpc_loop(&server, count);
        }));
    }

    barrier.wait();
    let start = Instant::now();
    for client in clients {
        client.join().unwrap();
    }
    for server in servers {
        server.join().unwrap();
    }
    start.elapsed()
}

fn run_throughput_scaling(iters: u64, executor_count: usize) -> Duration {
    let counts = split_iters_across(iters, THROUGHPUT_CONNECTIONS);
    let barrier = Arc::new(Barrier::new(THROUGHPUT_CONNECTIONS * 2 + 1));
    let config = PoolConfig::new(executor_count).with_placement_mode(PlacementMode::BackgroundOnly);
    let client_pool = TlsPool::new(config.clone()).unwrap();
    let server_pool = TlsPool::new(config).unwrap();
    let mut clients = Vec::with_capacity(THROUGHPUT_CONNECTIONS);
    let mut servers = Vec::with_capacity(THROUGHPUT_CONNECTIONS);

    for count in counts {
        let (client, server) = stream_pair_with_pools(&client_pool, &server_pool);
        let client_barrier = Arc::clone(&barrier);
        let server_barrier = Arc::clone(&barrier);
        clients.push(thread::spawn(move || {
            client_barrier.wait();
            client_rpc_loop(&client, count, THROUGHPUT_BODY_LEN);
        }));
        servers.push(thread::spawn(move || {
            server_barrier.wait();
            server_rpc_loop(&server, count);
        }));
    }

    barrier.wait();
    let start = Instant::now();
    for client in clients {
        client.join().unwrap();
    }
    for server in servers {
        server.join().unwrap();
    }
    start.elapsed()
}

fn client_write_loop(client: &TlsStream, count: u64, body_len: usize) {
    let body = vec![0x6d; body_len];
    for _ in 0..count {
        write_blocking(client, &body).unwrap();
    }
}

fn server_drain_loop(server: &TlsStream, count: u64, body_len: usize) {
    for _ in 0..count {
        let body = read_exact_blocking(server, body_len).unwrap();
        assert!(body.iter().all(|&byte| byte == 0x6d));
    }
}

fn run_write_throughput_scaling(iters: u64, executor_count: usize) -> Duration {
    let counts = split_iters_across(iters, THROUGHPUT_CONNECTIONS);
    let barrier = Arc::new(Barrier::new(THROUGHPUT_CONNECTIONS * 2 + 1));
    let config = PoolConfig::new(executor_count).with_placement_mode(PlacementMode::BackgroundOnly);
    let client_pool = TlsPool::new(config.clone()).unwrap();
    let server_pool = TlsPool::new(config).unwrap();
    let mut clients = Vec::with_capacity(THROUGHPUT_CONNECTIONS);
    let mut servers = Vec::with_capacity(THROUGHPUT_CONNECTIONS);

    for count in counts {
        let (client, server) = stream_pair_with_pools(&client_pool, &server_pool);
        let client_barrier = Arc::clone(&barrier);
        let server_barrier = Arc::clone(&barrier);
        clients.push(thread::spawn(move || {
            client_barrier.wait();
            client_write_loop(&client, count, THROUGHPUT_BODY_LEN);
        }));
        servers.push(thread::spawn(move || {
            server_barrier.wait();
            server_drain_loop(&server, count, THROUGHPUT_BODY_LEN);
        }));
    }

    barrier.wait();
    let start = Instant::now();
    for client in clients {
        client.join().unwrap();
    }
    for server in servers {
        server.join().unwrap();
    }
    start.elapsed()
}

fn print_latency_summary(label: &str, mut run_once: impl FnMut() -> Duration) {
    let mut samples = (0..5).map(|_| run_once()).collect::<Vec<_>>();
    samples.sort_unstable();
    let p50 = percentile(&samples, 50);
    let p95 = percentile(&samples, 95);
    let p99 = percentile(&samples, 99);
    println!(
        "{label}: p50={}us p95={}us p99={}us",
        p50.as_micros(),
        p95.as_micros(),
        p99.as_micros()
    );
}

fn percentile(samples: &[Duration], percentile: usize) -> Duration {
    let index = ((samples.len() - 1) * percentile).div_ceil(100);
    samples[index]
}

fn bench_rpc_write(c: &mut Criterion) {
    let mut single = c.benchmark_group("rpc_write/single_pair");
    single.warm_up_time(Duration::from_secs(1));
    single.measurement_time(Duration::from_secs(4));

    for body_len in BODY_SIZES {
        single.throughput(Throughput::Bytes(
            (RPC_HEADER_LEN + body_len + RPC_RESPONSE_LEN) as u64,
        ));
        for mode in [Mode::Immediate, Mode::Background, Mode::Adaptive] {
            let label = format!("rpc_write/single_pair/{}/{body_len}", mode.name());
            print_latency_summary(&label, || run_single_pair(1, body_len, mode));
            single.bench_with_input(
                BenchmarkId::new(mode.name(), body_len),
                &(body_len, mode),
                |b, &(body_len, mode)| {
                    b.iter_custom(|iters| run_single_pair(iters, body_len, mode));
                },
            );
        }
    }
    single.finish();

    let mut three = c.benchmark_group("rpc_write/three_pair");
    three.warm_up_time(Duration::from_secs(1));
    three.measurement_time(Duration::from_secs(4));

    for body_len in BODY_SIZES {
        three.throughput(Throughput::Bytes(
            (RPC_HEADER_LEN + body_len + RPC_RESPONSE_LEN) as u64,
        ));
        for mode in [Mode::Immediate, Mode::Background, Mode::Adaptive] {
            let label = format!("rpc_write/three_pair/{}/{body_len}", mode.name());
            print_latency_summary(&label, || run_three_pair(3, body_len, mode));
            three.bench_with_input(
                BenchmarkId::new(mode.name(), body_len),
                &(body_len, mode),
                |b, &(body_len, mode)| {
                    b.iter_custom(|iters| run_three_pair(iters, body_len, mode));
                },
            );
        }
    }
    three.finish();

    let mut shared = c.benchmark_group("rpc_write/three_pair_shared_pool");
    shared.warm_up_time(Duration::from_secs(1));
    shared.measurement_time(Duration::from_secs(4));

    for body_len in BODY_SIZES {
        shared.throughput(Throughput::Bytes(
            (RPC_HEADER_LEN + body_len + RPC_RESPONSE_LEN) as u64,
        ));
        for mode in [Mode::Immediate, Mode::Background, Mode::Adaptive] {
            let label = format!(
                "rpc_write/three_pair_shared_pool/{}/{body_len}",
                mode.name()
            );
            print_latency_summary(&label, || run_three_pair_shared_pool(3, body_len, mode));
            shared.bench_with_input(
                BenchmarkId::new(mode.name(), body_len),
                &(body_len, mode),
                |b, &(body_len, mode)| {
                    b.iter_custom(|iters| run_three_pair_shared_pool(iters, body_len, mode));
                },
            );
        }
    }
    shared.finish();
}

fn bench_throughput_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("rpc_write/throughput_scaling");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(4));
    group.throughput(Throughput::Bytes(
        (RPC_HEADER_LEN + THROUGHPUT_BODY_LEN + RPC_RESPONSE_LEN) as u64,
    ));

    for executor_count in EXECUTOR_COUNTS {
        let label = format!("rpc_write/throughput_scaling/{executor_count}_executors");
        print_latency_summary(&label, || {
            run_throughput_scaling(THROUGHPUT_CONNECTIONS as u64, executor_count)
        });
        group.bench_with_input(
            BenchmarkId::new("executors", executor_count),
            &executor_count,
            |b, &executor_count| {
                b.iter_custom(|iters| run_throughput_scaling(iters, executor_count));
            },
        );
    }

    group.finish();
}

fn bench_write_throughput_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("tls_write/throughput_scaling");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(4));
    group.throughput(Throughput::Bytes(THROUGHPUT_BODY_LEN as u64));

    for executor_count in EXECUTOR_COUNTS {
        let label = format!("tls_write/throughput_scaling/{executor_count}_executors");
        print_latency_summary(&label, || {
            run_write_throughput_scaling(THROUGHPUT_CONNECTIONS as u64, executor_count)
        });
        group.bench_with_input(
            BenchmarkId::new("executors", executor_count),
            &executor_count,
            |b, &executor_count| {
                b.iter_custom(|iters| run_write_throughput_scaling(iters, executor_count));
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_rpc_write,
    bench_throughput_scaling,
    bench_write_throughput_scaling
);
criterion_main!(benches);
