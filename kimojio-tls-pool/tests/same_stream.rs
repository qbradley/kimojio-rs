mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;

use support::{
    background_config, immediate_config, read_exact_blocking, stream_pair, write_blocking,
};

#[test]
fn same_stream_writes_are_ordered_immediate() {
    run_same_stream_ordering(immediate_config(), immediate_config());
}

#[test]
fn same_stream_writes_are_ordered_background() {
    run_same_stream_ordering(background_config(2), background_config(1));
}

fn run_same_stream_ordering(
    client_config: kimojio_tls_pool::PoolConfig,
    server_config: kimojio_tls_pool::PoolConfig,
) {
    let (client, server) = stream_pair(client_config, server_config);
    let callback_count = Arc::new(AtomicUsize::new(0));
    let server_thread = thread::spawn(move || {
        let received = read_exact_blocking(&server, "firstsecond".len()).unwrap();
        assert_eq!(received, b"firstsecond");
        write_blocking(&server, b"ok").unwrap();
    });

    let (sender, receiver) = mpsc::channel();
    let first_sender = sender.clone();
    let first_count = Arc::clone(&callback_count);
    client
        .write(
            b"first".to_vec(),
            Box::new(move |result| {
                first_count.fetch_add(1, Ordering::SeqCst);
                first_sender.send(result.unwrap()).unwrap();
            }),
        )
        .unwrap();
    let second_count = Arc::clone(&callback_count);
    client
        .write(
            b"second".to_vec(),
            Box::new(move |result| {
                second_count.fetch_add(1, Ordering::SeqCst);
                sender.send(result.unwrap()).unwrap();
            }),
        )
        .unwrap();

    assert_eq!(receiver.recv().unwrap(), 5);
    assert_eq!(receiver.recv().unwrap(), 6);
    assert_eq!(callback_count.load(Ordering::SeqCst), 2);
    let stats = client.stats();
    assert_eq!(stats.completed, 2);
    assert_eq!(stats.failed, 0);
    let response = read_exact_blocking(&client, 2).unwrap();
    assert_eq!(response, b"ok");
    assert_eq!(client.stats().completed, 3);
    server_thread.join().unwrap();
}
