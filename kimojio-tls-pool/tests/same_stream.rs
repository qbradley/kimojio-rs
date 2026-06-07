mod support;

use std::sync::mpsc;
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
    let server_thread = thread::spawn(move || {
        let received = read_exact_blocking(&server, "firstsecond".len()).unwrap();
        assert_eq!(received, b"firstsecond");
        write_blocking(&server, b"ok").unwrap();
    });

    let (sender, receiver) = mpsc::channel();
    let first_sender = sender.clone();
    client
        .write(
            b"first".to_vec(),
            Box::new(move |result| {
                first_sender.send(result.unwrap()).unwrap();
            }),
        )
        .unwrap();
    client
        .write(
            b"second".to_vec(),
            Box::new(move |result| {
                sender.send(result.unwrap()).unwrap();
            }),
        )
        .unwrap();

    assert_eq!(receiver.recv().unwrap(), 5);
    assert_eq!(receiver.recv().unwrap(), 6);
    let response = read_exact_blocking(&client, 2).unwrap();
    assert_eq!(response, b"ok");
    server_thread.join().unwrap();
}
