mod support;

use std::thread;

use kimojio_tls_pool::{PlacementMode, PoolConfig};
use support::{
    RPC_HEADER_LEN, RPC_RESPONSE_LEN, background_config, immediate_config, make_rpc_header,
    read_exact_blocking, rpc_body_len, stream_pair, write_blocking,
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
