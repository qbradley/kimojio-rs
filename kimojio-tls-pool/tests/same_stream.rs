mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;

use kimojio_tls_pool::{PlacementMode, PoolConfig};
use support::{
    background_config, immediate_config, read_exact_blocking, stream_pair, write_blocking,
};

#[test]
fn same_stream_writes_are_ordered_immediate() {
    run_same_stream_ordering(
        immediate_config(),
        immediate_config(),
        b"first".to_vec(),
        b"second".to_vec(),
    );
}

#[test]
fn same_stream_writes_are_ordered_background() {
    run_same_stream_ordering(
        background_config(2),
        background_config(1),
        b"first".to_vec(),
        b"second".to_vec(),
    );
}

#[test]
fn same_stream_writes_are_ordered_adaptive() {
    run_same_stream_ordering(
        PoolConfig::new(2).with_placement_mode(PlacementMode::Adaptive),
        immediate_config(),
        vec![b'a'; 24 * 1024],
        vec![b'b'; 24 * 1024],
    );
}

fn run_same_stream_ordering(
    client_config: PoolConfig,
    server_config: PoolConfig,
    first_payload: Vec<u8>,
    second_payload: Vec<u8>,
) {
    let mut expected = first_payload.clone();
    expected.extend_from_slice(&second_payload);
    let first_len = first_payload.len();
    let second_len = second_payload.len();
    let (client, server) = stream_pair(client_config, server_config);
    let callback_count = Arc::new(AtomicUsize::new(0));
    let server_thread = thread::spawn(move || {
        let received = read_exact_blocking(&server, expected.len()).unwrap();
        assert_eq!(received, expected);
        write_blocking(&server, b"ok").unwrap();
    });

    let (sender, receiver) = mpsc::channel();
    let first_sender = sender.clone();
    let first_count = Arc::clone(&callback_count);
    client
        .write(
            first_payload,
            Box::new(move |result| {
                first_count.fetch_add(1, Ordering::SeqCst);
                first_sender.send(result.unwrap()).unwrap();
            }),
        )
        .unwrap();
    let second_count = Arc::clone(&callback_count);
    client
        .write(
            second_payload,
            Box::new(move |result| {
                second_count.fetch_add(1, Ordering::SeqCst);
                sender.send(result.unwrap()).unwrap();
            }),
        )
        .unwrap();

    assert_eq!(receiver.recv().unwrap(), first_len);
    assert_eq!(receiver.recv().unwrap(), second_len);
    assert_eq!(callback_count.load(Ordering::SeqCst), 2);
    let stats = client.stats();
    assert_eq!(stats.completed, 2);
    assert_eq!(stats.failed, 0);
    let response = read_exact_blocking(&client, 2).unwrap();
    assert_eq!(response, b"ok");
    assert_eq!(client.stats().completed, 3);
    let stream_stats = client.stream_stats();
    assert_eq!(stream_stats.max_active, 1);
    assert_eq!(stream_stats.active, 0);
    server_thread.join().unwrap();
}
