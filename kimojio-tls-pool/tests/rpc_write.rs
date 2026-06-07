mod support;

use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use kimojio_tls_pool::{PlacementMode, PoolConfig, TlsPool};
use support::{
    RPC_HEADER_LEN, RPC_RESPONSE_LEN, background_config, controlled_stream_pair, immediate_config,
    make_rpc_header, read_exact_blocking, rpc_body_len, stream_pair, stream_pair_with_pools,
    write_blocking,
};

#[test]
fn single_pair_rpc_immediate() {
    run_single_pair_rpc(immediate_config(), immediate_config(), 8 * 1024);
}

#[test]
fn single_pair_rpc_background() {
    run_single_pair_rpc(background_config(1), background_config(1), 24 * 1024);
}

#[test]
fn single_pair_rpc_near_maximum_background() {
    run_single_pair_rpc(background_config(2), background_config(2), 32 * 1024);
}

#[test]
fn three_pair_rpc_background() {
    let mut servers = Vec::new();
    let mut clients = Vec::new();

    for _ in 0..3 {
        let (client, server) = stream_pair(background_config(2), background_config(2));
        servers.push(thread::spawn(move || server_rpc(server, 16 * 1024)));
        clients.push(thread::spawn(move || client_rpc(client, 16 * 1024)));
    }

    for client in clients {
        client.join().unwrap();
    }
    for server in servers {
        server.join().unwrap();
    }
}

#[test]
fn handshake_failure_is_reported() {
    let (_acceptor, connector) = support::tls_contexts();
    let (client_io, server_io) = std::os::unix::net::UnixStream::pair().unwrap();
    drop(server_io);
    let pool = kimojio_tls_pool::TlsPool::new(
        PoolConfig::new(1).with_placement_mode(PlacementMode::ImmediateOnly),
    )
    .unwrap();

    assert!(pool.client(&connector, "localhost", client_io).is_err());
}

#[test]
fn operation_error_is_reported_after_peer_drop() {
    let (client, _server, client_fail, _server_fail) =
        controlled_stream_pair(immediate_config(), immediate_config());
    client_fail.store(true, std::sync::atomic::Ordering::Release);

    assert!(write_blocking(&client, b"fail").is_err());
}

#[test]
fn pending_read_does_not_occupy_only_client_executor() {
    let client_pool = TlsPool::new(background_config(1)).unwrap();
    let server_pool = TlsPool::new(background_config(1)).unwrap();
    let (waiting_client, waiting_server) = stream_pair_with_pools(&client_pool, &server_pool);
    let (active_client, active_server) = stream_pair_with_pools(&client_pool, &server_pool);
    let (read_sender, read_receiver) = mpsc::channel();

    waiting_client
        .read(
            2,
            Box::new(move |result| {
                read_sender.send(result.unwrap()).unwrap();
            }),
        )
        .unwrap();

    let active_server_thread = thread::spawn(move || {
        let received = read_exact_blocking(&active_server, 2).unwrap();
        assert_eq!(received, b"ok");
    });

    assert_eq!(write_blocking(&active_client, b"ok").unwrap(), 2);
    active_server_thread.join().unwrap();

    assert_eq!(write_blocking(&waiting_server, b"go").unwrap(), 2);
    assert_eq!(
        read_receiver.recv_timeout(Duration::from_secs(5)).unwrap(),
        b"go"
    );
}

#[test]
fn buffered_plaintext_read_does_not_wait_for_new_socket_readiness() {
    let (client, server) = stream_pair(immediate_config(), immediate_config());
    let payload = vec![0x33; 64];
    write_blocking(&server, &payload).unwrap();

    let first = support::read_once_blocking(&client, 1).unwrap();
    assert_eq!(first, vec![0x33]);

    let rest = support::read_once_blocking(&client, 63).unwrap();
    assert_eq!(rest, vec![0x33; 63]);
}

#[test]
fn shared_write_sends_payload_without_per_call_payload_copy() {
    let (client, server) = stream_pair(background_config(1), immediate_config());
    let payload: Arc<[u8]> = Arc::from(vec![0x44; 32 * 1024].into_boxed_slice());
    let expected = Arc::clone(&payload);
    let server_thread = thread::spawn(move || {
        let received = read_exact_blocking(&server, expected.len()).unwrap();
        assert_eq!(&received[..], &expected[..]);
    });

    let (sender, receiver) = mpsc::channel();
    client
        .write_shared(
            payload,
            Box::new(move |result| {
                sender.send(result.unwrap()).unwrap();
            }),
        )
        .unwrap();

    assert_eq!(
        receiver.recv_timeout(Duration::from_secs(5)).unwrap(),
        32 * 1024
    );
    server_thread.join().unwrap();
}

#[test]
fn shared_write_batch_sends_all_chunks_with_one_callback() {
    let (client, server) = stream_pair(background_config(1), immediate_config());
    let chunk: Arc<[u8]> = Arc::from(vec![0x45; 8 * 1024].into_boxed_slice());
    let chunks = vec![Arc::clone(&chunk), Arc::clone(&chunk), Arc::clone(&chunk)];
    let server_thread = thread::spawn(move || {
        let received = read_exact_blocking(&server, 24 * 1024).unwrap();
        assert_eq!(received, vec![0x45; 24 * 1024]);
    });

    let (sender, receiver) = mpsc::channel();
    client
        .write_batch(
            chunks,
            Box::new(move |result| {
                sender.send(result.unwrap()).unwrap();
            }),
        )
        .unwrap();

    assert_eq!(
        receiver.recv_timeout(Duration::from_secs(5)).unwrap(),
        24 * 1024
    );
    server_thread.join().unwrap();
}

fn run_single_pair_rpc(client_config: PoolConfig, server_config: PoolConfig, body_len: usize) {
    let (client, server) = stream_pair(client_config, server_config);
    let server_thread = thread::spawn(move || server_rpc(server, body_len));
    client_rpc(client, body_len);
    server_thread.join().unwrap();
}

fn client_rpc(client: kimojio_tls_pool::TlsStream, body_len: usize) {
    let header = make_rpc_header(body_len);
    let body = vec![0x5a; body_len];
    write_blocking(&client, &header).unwrap();
    write_blocking(&client, &body).unwrap();

    let response = read_exact_blocking(&client, RPC_RESPONSE_LEN).unwrap();
    assert_eq!(response, [0x7b; RPC_RESPONSE_LEN]);
}

fn server_rpc(server: kimojio_tls_pool::TlsStream, expected_body_len: usize) {
    let header = read_exact_blocking(&server, RPC_HEADER_LEN).unwrap();
    let header: [u8; RPC_HEADER_LEN] = header.try_into().unwrap();
    let body_len = rpc_body_len(&header);
    assert_eq!(body_len, expected_body_len);

    let body = read_exact_blocking(&server, body_len).unwrap();
    assert!(body.iter().all(|&byte| byte == 0x5a));
    write_blocking(&server, &[0x7b; RPC_RESPONSE_LEN]).unwrap();
}
