mod support;

use kimojio_fsm_http2::Config;
use support::{Pair, request, response};

fn main() {
    let mut pair = Pair::new(Config::default());
    let stream = pair.client.request(&request(b"GET"), true).unwrap();
    pair.pump(7);
    assert_eq!(pair.server_ports.heads.len(), 1);
    pair.server
        .respond(stream, &response(b"200"), false)
        .unwrap();
    pair.pump(7);
    let permit = pair.server_ports.permits.pop_front().unwrap();
    pair.server.send(permit, b"hello".to_vec(), true).unwrap();
    pair.pump(7);
    let body = pair.client_ports.bodies.pop_front().unwrap();
    assert_eq!(body.bytes(), b"hello");
    pair.client.release_body(body.release()).unwrap();
    pair.pump(7);
    assert_eq!(pair.client_ports.retired.len(), 1);
    assert_eq!(pair.server_ports.retired.len(), 1);
    pair.server.shutdown().unwrap();
    pair.pump(7);
    pair.server
        .advance_time(std::time::Duration::from_secs(1))
        .unwrap();
    pair.pump(7);
    println!("request, fragmented body, release, retirement, and shutdown passed");
}
