// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::io::{Read, Write};
use std::mem::size_of;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::Ordering;
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
    ssl::{HandshakeError, SslAcceptor, SslConnector, SslMethod, SslStream, SslVerifyMode},
    x509::{X509, X509NameBuilder},
};

const CONNECTIONS: usize = 3;
const THROUGHPUT_CONNECTIONS: usize = 16;
const EXECUTOR_COUNTS: [usize; 4] = [1, 2, 4, 8];
const RPC_HEADER_LEN: usize = 64;
const RPC_RESPONSE_LEN: usize = 64;
const BODY_SIZES: [usize; 5] = [4 * 1024, 8 * 1024, 16 * 1024, 24 * 1024, 32 * 1024];
const THROUGHPUT_BODY_LEN: usize = 32 * 1024;
const THROUGHPUT_MESSAGES_PER_ITER: u64 = 512;
const SATURATED_MESSAGES_PER_EXECUTOR: u64 = 2048;

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

fn stream_pair_with_discarding_client(
    client_pool: &TlsPool,
    server_pool: &TlsPool,
) -> (TlsStream, TlsStream, Arc<std::sync::atomic::AtomicBool>) {
    let (acceptor, connector) = make_contexts();
    let (client_io, server_io) = UnixStream::pair().unwrap();
    let discard = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let client_transport = DiscardAfterHandshake {
        inner: client_io,
        discard: Arc::clone(&discard),
    };
    let server_pool = server_pool.clone();
    let server_thread = thread::spawn(move || server_pool.server(&acceptor, server_io).unwrap());

    let client = client_pool
        .client(&connector, "localhost", client_transport)
        .unwrap();
    let server = server_thread.join().unwrap();
    discard.store(true, Ordering::Release);
    (client, server, discard)
}

trait BenchTlsTransport: Read + Write + AsFd + Send {}

impl<T> BenchTlsTransport for T where T: Read + Write + AsFd + Send {}

fn direct_boxed_discarding_client() -> (
    SslStream<Box<dyn BenchTlsTransport>>,
    SslStream<UnixStream>,
    Arc<std::sync::atomic::AtomicBool>,
) {
    let (acceptor, connector) = make_contexts();
    let (client_io, server_io) = UnixStream::pair().unwrap();
    let discard = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let client_transport = DiscardAfterHandshake {
        inner: client_io,
        discard: Arc::clone(&discard),
    };
    let server_thread = thread::spawn(move || acceptor.accept(server_io).unwrap());

    let client = connector
        .connect(
            "localhost",
            Box::new(client_transport) as Box<dyn BenchTlsTransport>,
        )
        .map_err(|error| match error {
            HandshakeError::SetupFailure(error) => error.to_string(),
            HandshakeError::Failure(error) => error.error().to_string(),
            HandshakeError::WouldBlock(error) => error.error().to_string(),
        })
        .unwrap();
    let server = server_thread.join().unwrap();
    discard.store(true, Ordering::Release);
    (client, server, discard)
}

#[derive(Debug)]
struct DiscardAfterHandshake {
    inner: UnixStream,
    discard: Arc<std::sync::atomic::AtomicBool>,
}

impl Read for DiscardAfterHandshake {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Write for DiscardAfterHandshake {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.discard.load(Ordering::Acquire) {
            Ok(buf.len())
        } else {
            self.inner.write(buf)
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.discard.load(Ordering::Acquire) {
            Ok(())
        } else {
            self.inner.flush()
        }
    }
}

impl AsFd for DiscardAfterHandshake {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.inner.as_fd()
    }
}

fn ssl_write_all<S>(stream: &mut SslStream<S>, bytes: &[u8])
where
    S: Read + Write,
{
    let mut written = 0;
    while written < bytes.len() {
        let amount = stream.ssl_write(&bytes[written..]).unwrap();
        assert_ne!(amount, 0);
        written += amount;
    }
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

fn client_write_batch(client: &TlsStream, count: u64, body_len: usize) {
    let body: Arc<[u8]> = Arc::from(vec![0x6d; body_len].into_boxed_slice());
    let chunks = (0..count).map(|_| Arc::clone(&body)).collect::<Vec<_>>();
    let (sender, receiver) = mpsc::channel();
    client
        .write_batch(
            chunks,
            Box::new(move |result| {
                result.unwrap();
                sender.send(()).unwrap();
            }),
        )
        .unwrap();
    receiver.recv().unwrap();
}

fn client_write_per_operation(client: &TlsStream, count: u64, body_len: usize) {
    let body: Arc<[u8]> = Arc::from(vec![0x6d; body_len].into_boxed_slice());
    for _ in 0..count {
        let (sender, receiver) = mpsc::channel();
        client
            .write_shared(
                Arc::clone(&body),
                Box::new(move |result| {
                    sender.send(result.unwrap()).unwrap();
                }),
            )
            .unwrap();
        assert_eq!(receiver.recv().unwrap(), body_len);
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
    let client_config =
        PoolConfig::new(executor_count).with_placement_mode(PlacementMode::BackgroundOnly);
    let server_config = PoolConfig::new(1).with_placement_mode(PlacementMode::ImmediateOnly);
    let client_pool = TlsPool::new(client_config).unwrap();
    let server_pool = TlsPool::new(server_config).unwrap();
    let mut clients = Vec::with_capacity(THROUGHPUT_CONNECTIONS);
    let mut servers = Vec::with_capacity(THROUGHPUT_CONNECTIONS);

    for count in counts {
        let (client, server) = stream_pair_with_pools(&client_pool, &server_pool);
        let client_barrier = Arc::clone(&barrier);
        let server_barrier = Arc::clone(&barrier);
        clients.push(thread::spawn(move || {
            client_barrier.wait();
            client_write_per_operation(&client, count, THROUGHPUT_BODY_LEN);
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

fn run_batched_write_throughput_scaling(iters: u64, executor_count: usize) -> Duration {
    let counts = split_iters_across(iters, THROUGHPUT_CONNECTIONS);
    let barrier = Arc::new(Barrier::new(THROUGHPUT_CONNECTIONS * 2 + 1));
    let client_config =
        PoolConfig::new(executor_count).with_placement_mode(PlacementMode::BackgroundOnly);
    let server_config = PoolConfig::new(1).with_placement_mode(PlacementMode::ImmediateOnly);
    let client_pool = TlsPool::new(client_config).unwrap();
    let server_pool = TlsPool::new(server_config).unwrap();
    let mut clients = Vec::with_capacity(THROUGHPUT_CONNECTIONS);
    let mut servers = Vec::with_capacity(THROUGHPUT_CONNECTIONS);

    for count in counts {
        let (client, server) = stream_pair_with_pools(&client_pool, &server_pool);
        let client_barrier = Arc::clone(&barrier);
        let server_barrier = Arc::clone(&barrier);
        clients.push(thread::spawn(move || {
            client_barrier.wait();
            client_write_batch(&client, count, THROUGHPUT_BODY_LEN);
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

fn run_encrypt_throughput_scaling(iters: u64, executor_count: usize) -> Duration {
    run_encrypt_with_connections(iters, executor_count, THROUGHPUT_CONNECTIONS, "encrypt")
}

fn run_saturated_encrypt_throughput_scaling(iters: u64, executor_count: usize) -> Duration {
    run_encrypt_with_connections(
        iters * SATURATED_MESSAGES_PER_EXECUTOR * executor_count as u64,
        executor_count,
        executor_count,
        "saturated_encrypt",
    )
}

fn run_encrypt_with_connections(
    message_count: u64,
    executor_count: usize,
    connection_count: usize,
    stats_label: &str,
) -> Duration {
    let counts = split_iters_across(message_count, connection_count);
    let barrier = Arc::new(Barrier::new(connection_count + 1));
    let client_config =
        PoolConfig::new(executor_count).with_placement_mode(PlacementMode::BackgroundOnly);
    let server_config = PoolConfig::new(1).with_placement_mode(PlacementMode::ImmediateOnly);
    let client_pool = TlsPool::new(client_config).unwrap();
    let server_pool = TlsPool::new(server_config).unwrap();
    let mut clients = Vec::with_capacity(connection_count);
    let mut servers = Vec::with_capacity(connection_count);
    let mut discard_flags = Vec::with_capacity(connection_count);

    for count in counts {
        let (client, server, discard) =
            stream_pair_with_discarding_client(&client_pool, &server_pool);
        let client_barrier = Arc::clone(&barrier);
        clients.push(thread::spawn(move || {
            client_barrier.wait();
            client_write_batch(&client, count, THROUGHPUT_BODY_LEN);
        }));
        servers.push(server);
        discard_flags.push(discard);
    }

    barrier.wait();
    let start = Instant::now();
    for client in clients {
        client.join().unwrap();
    }
    print_pool_stats(stats_label, executor_count, &client_pool);
    drop(discard_flags);
    drop(servers);
    start.elapsed()
}

fn run_pool_single_encrypt(iters: u64, executor_count: usize) -> Duration {
    let client_config =
        PoolConfig::new(executor_count).with_placement_mode(PlacementMode::BackgroundOnly);
    let server_config = PoolConfig::new(1).with_placement_mode(PlacementMode::ImmediateOnly);
    let client_pool = TlsPool::new(client_config).unwrap();
    let server_pool = TlsPool::new(server_config).unwrap();
    let (client, server, discard) = stream_pair_with_discarding_client(&client_pool, &server_pool);

    let start = Instant::now();
    client_write_batch(&client, iters, THROUGHPUT_BODY_LEN);
    print_pool_stats("pool_single_encrypt", executor_count, &client_pool);
    drop(discard);
    drop(server);
    start.elapsed()
}

fn run_direct_boxed_encrypt_throughput_scaling(iters: u64, thread_count: usize) -> Duration {
    let counts = split_iters_across(iters, thread_count);
    let barrier = Arc::new(Barrier::new(thread_count + 1));
    let body = Arc::<[u8]>::from(vec![0x6d; THROUGHPUT_BODY_LEN].into_boxed_slice());
    let mut clients = Vec::with_capacity(thread_count);
    let mut servers = Vec::with_capacity(thread_count);
    let mut discard_flags = Vec::with_capacity(thread_count);

    for count in counts {
        let (mut client, server, discard) = direct_boxed_discarding_client();
        let client_barrier = Arc::clone(&barrier);
        let body = Arc::clone(&body);
        clients.push(thread::spawn(move || {
            client_barrier.wait();
            for _ in 0..count {
                ssl_write_all(&mut client, &body);
            }
        }));
        servers.push(server);
        discard_flags.push(discard);
    }

    barrier.wait();
    let start = Instant::now();
    for client in clients {
        client.join().unwrap();
    }
    drop(discard_flags);
    drop(servers);
    start.elapsed()
}

fn run_direct_boxed_saturated_encrypt(iters: u64, thread_count: usize) -> Duration {
    run_direct_boxed_encrypt_throughput_scaling(
        iters * SATURATED_MESSAGES_PER_EXECUTOR * thread_count as u64,
        thread_count,
    )
}

fn print_pool_stats(label: &str, executor_count: usize, pool: &TlsPool) {
    if std::env::var_os("KIMOJIO_TLS_POOL_PRINT_STATS").is_none() {
        return;
    }

    let stats = pool.stats();
    let completed = stats
        .executors
        .iter()
        .map(|executor| executor.completed.to_string())
        .collect::<Vec<_>>()
        .join(",");
    eprintln!(
        "{label}/{executor_count}: submitted={} background={} queued={} completed=[{completed}]",
        stats.submitted, stats.background_routed, stats.executor_queued,
    );
}

fn print_sample_summary(label: &str, mut run_once: impl FnMut() -> Duration) {
    let mut samples = (0..5).map(|_| run_once()).collect::<Vec<_>>();
    samples.sort_unstable();
    let median = samples[samples.len() / 2];
    let high = samples[samples.len() - 1];
    println!(
        "{label}: sample_median={}us sample_high={}us samples={}",
        median.as_micros(),
        high.as_micros(),
        samples.len(),
    );
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
            print_sample_summary(&label, || run_single_pair(1, body_len, mode));
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
            print_sample_summary(&label, || run_three_pair(3, body_len, mode));
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
            print_sample_summary(&label, || run_three_pair_shared_pool(3, body_len, mode));
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
        (RPC_HEADER_LEN + THROUGHPUT_BODY_LEN + RPC_RESPONSE_LEN) as u64
            * THROUGHPUT_MESSAGES_PER_ITER,
    ));

    for executor_count in EXECUTOR_COUNTS {
        let label = format!("rpc_write/throughput_scaling/{executor_count}_executors");
        print_sample_summary(&label, || {
            run_throughput_scaling(THROUGHPUT_CONNECTIONS as u64, executor_count)
        });
        group.bench_with_input(
            BenchmarkId::new("executors", executor_count),
            &executor_count,
            |b, &executor_count| {
                b.iter_custom(|iters| {
                    run_throughput_scaling(iters * THROUGHPUT_MESSAGES_PER_ITER, executor_count)
                });
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
    group.throughput(Throughput::Bytes(
        THROUGHPUT_BODY_LEN as u64 * THROUGHPUT_MESSAGES_PER_ITER,
    ));

    for executor_count in EXECUTOR_COUNTS {
        let label = format!("tls_write/throughput_scaling/{executor_count}_executors");
        print_sample_summary(&label, || {
            run_write_throughput_scaling(THROUGHPUT_CONNECTIONS as u64, executor_count)
        });
        group.bench_with_input(
            BenchmarkId::new("executors", executor_count),
            &executor_count,
            |b, &executor_count| {
                b.iter_custom(|iters| {
                    run_write_throughput_scaling(
                        iters * THROUGHPUT_MESSAGES_PER_ITER,
                        executor_count,
                    )
                });
            },
        );
    }

    group.finish();
}

fn bench_batched_write_throughput_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("tls_write/batched_throughput_scaling");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(4));
    group.throughput(Throughput::Bytes(
        THROUGHPUT_BODY_LEN as u64 * THROUGHPUT_MESSAGES_PER_ITER,
    ));

    for executor_count in EXECUTOR_COUNTS {
        let label = format!("tls_write/batched_throughput_scaling/{executor_count}_executors");
        print_sample_summary(&label, || {
            run_batched_write_throughput_scaling(THROUGHPUT_CONNECTIONS as u64, executor_count)
        });
        group.bench_with_input(
            BenchmarkId::new("executors", executor_count),
            &executor_count,
            |b, &executor_count| {
                b.iter_custom(|iters| {
                    run_batched_write_throughput_scaling(
                        iters * THROUGHPUT_MESSAGES_PER_ITER,
                        executor_count,
                    )
                });
            },
        );
    }

    group.finish();
}

fn bench_encrypt_throughput_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("tls_write/encrypt_throughput_scaling");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(4));
    group.throughput(Throughput::Bytes(
        THROUGHPUT_BODY_LEN as u64 * THROUGHPUT_MESSAGES_PER_ITER,
    ));

    for executor_count in EXECUTOR_COUNTS {
        let label = format!("tls_write/encrypt_throughput_scaling/{executor_count}_executors");
        print_sample_summary(&label, || {
            run_encrypt_throughput_scaling(THROUGHPUT_CONNECTIONS as u64, executor_count)
        });
        group.bench_with_input(
            BenchmarkId::new("executors", executor_count),
            &executor_count,
            |b, &executor_count| {
                b.iter_custom(|iters| {
                    run_encrypt_throughput_scaling(
                        iters * THROUGHPUT_MESSAGES_PER_ITER,
                        executor_count,
                    )
                });
            },
        );
    }

    group.finish();
}

fn bench_saturated_encrypt_throughput_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("tls_write/saturated_encrypt_scaling");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(4));

    for executor_count in EXECUTOR_COUNTS {
        let bytes =
            THROUGHPUT_BODY_LEN as u64 * SATURATED_MESSAGES_PER_EXECUTOR * executor_count as u64;
        group.throughput(Throughput::Bytes(bytes));
        let label = format!("tls_write/saturated_encrypt_scaling/{executor_count}_executors");
        print_sample_summary(&label, || {
            run_saturated_encrypt_throughput_scaling(1, executor_count)
        });
        group.bench_with_input(
            BenchmarkId::new("executors", executor_count),
            &executor_count,
            |b, &executor_count| {
                b.iter_custom(|iters| {
                    run_saturated_encrypt_throughput_scaling(iters, executor_count)
                });
            },
        );
    }

    group.finish();
}

fn bench_pool_single_encrypt(c: &mut Criterion) {
    let mut group = c.benchmark_group("tls_write/single_stream_encrypt");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(4));
    group.throughput(Throughput::Bytes(
        THROUGHPUT_BODY_LEN as u64 * THROUGHPUT_MESSAGES_PER_ITER,
    ));

    let executor_count = 1;
    print_sample_summary("tls_write/single_stream_encrypt/1_executor", || {
        run_pool_single_encrypt(THROUGHPUT_MESSAGES_PER_ITER, executor_count)
    });
    group.bench_function("1_executor", |b| {
        b.iter_custom(|iters| {
            run_pool_single_encrypt(iters * THROUGHPUT_MESSAGES_PER_ITER, executor_count)
        });
    });

    group.finish();
}

fn bench_direct_boxed_saturated_encrypt(c: &mut Criterion) {
    let mut group = c.benchmark_group("openssl_direct_boxed/saturated_encrypt_scaling");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(4));

    for thread_count in EXECUTOR_COUNTS {
        let bytes =
            THROUGHPUT_BODY_LEN as u64 * SATURATED_MESSAGES_PER_EXECUTOR * thread_count as u64;
        group.throughput(Throughput::Bytes(bytes));
        let label =
            format!("openssl_direct_boxed/saturated_encrypt_scaling/{thread_count}_threads");
        print_sample_summary(&label, || {
            run_direct_boxed_saturated_encrypt(1, thread_count)
        });
        group.bench_with_input(
            BenchmarkId::new("threads", thread_count),
            &thread_count,
            |b, &thread_count| {
                b.iter_custom(|iters| run_direct_boxed_saturated_encrypt(iters, thread_count));
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_rpc_write,
    bench_throughput_scaling,
    bench_write_throughput_scaling,
    bench_batched_write_throughput_scaling,
    bench_encrypt_throughput_scaling,
    bench_saturated_encrypt_throughput_scaling,
    bench_pool_single_encrypt,
    bench_direct_boxed_saturated_encrypt
);
criterion_main!(benches);
