use super::*;
#[path = "../examples/support/mod.rs"]
mod support;
use support::{MemoryPorts, Pair, request, response, step};

#[test]
fn stream_identifier_exhaustion_does_not_wrap_or_reuse() {
    let mut client = Client::<Vec<u8>>::new(Config::default(), Duration::ZERO).unwrap();
    let Protocol::Client(role) = &mut client.0.protocol else {
        unreachable!()
    };
    role.next_stream_id = 0x7fff_ffff;
    let last = client.request(&request(b"GET"), true).unwrap();
    assert_eq!(last.get(), 0x7fff_ffff);
    let pending = client.controls.len();
    assert!(client.request(&request(b"GET"), true).is_err());
    assert_eq!(client.controls.len(), pending);
    let Protocol::Client(role) = &client.0.protocol else {
        unreachable!()
    };
    assert_eq!(role.next_stream_id, 0x8000_0001);
}

#[test]
fn operation_identifier_exhaustion_uses_reserved_settlement_identities() {
    let mut client = Client::<Vec<u8>>::new(Config::default(), Duration::ZERO).unwrap();
    client.request(&request(b"GET"), true).unwrap();
    client.sequence = u64::MAX - 8;
    let mut ports = MemoryPorts::default();
    let mut input = VecDeque::new();
    let mut output = VecDeque::new();
    for _ in 0..20 {
        if !step(&mut client, &mut ports, &mut input, &mut output, 32_768) {
            break;
        }
    }
    assert_eq!(ports.closed, [ConnectionResult::ResourceExhausted]);
    assert_eq!(ports.retired.len(), 1);
    assert!(output.is_empty());
    assert!(client.sequence > u64::MAX - 8);
    assert!(client.sequence < u64::MAX);
    assert!(ports.read.is_none());
}

#[test]
fn retired_sources_do_not_accumulate_behind_one_held_permit() {
    let mut pair = Pair::new(Config {
        max_send_capacity: 1024,
        max_send_buffer_capacity: 1024,
        http: HttpLimits::new().set_max_active_streams(4),
        ..Config::default()
    });
    let held = pair.client.request(&request(b"GET"), true).unwrap();
    pair.pump(32_768);
    pair.server.respond(held, &response(b"200"), false).unwrap();
    pair.pump(32_768);
    assert_eq!(pair.server_ports.permits.len(), 1);
    for index in 1..=512 {
        pair.client
            .advance_time(Duration::from_secs(index))
            .unwrap();
        pair.server
            .advance_time(Duration::from_secs(index))
            .unwrap();
        let id = pair.client.request(&request(b"GET"), true).unwrap();
        pair.pump(32_768);
        pair.server.respond(id, &response(b"200"), false).unwrap();
        pair.pump(32_768);
        assert_eq!(pair.server.demand.len(), 1);
        pair.client.reset(id, H2ErrorCode::Cancel).unwrap();
        pair.pump(32_768);
        assert!(pair.server.demand.is_empty());
        assert!(pair.server.ready.is_empty());
        assert!(pair.server.blocked.is_empty());
        assert_eq!(pair.server.streams.len(), 1);
        assert_eq!(pair.server_ports.permits.len(), 1);
    }
}

#[test]
fn a_retained_small_fragment_does_not_monopolize_receive_storage() {
    let mut pair = Pair::new(Config {
        connection_receive_window: 65_535,
        max_receive_capacity: 4 * PAGE_SIZE,
        ..Config::default()
    });
    let held = pair.client.request(&request(b"POST"), false).unwrap();
    let sibling = pair.client.request(&request(b"POST"), false).unwrap();
    pair.pump(32_768);
    let permit = pair.client_ports.permits.pop_front().unwrap();
    assert_eq!(permit.stream(), held);
    pair.client.send(permit, vec![1], false).unwrap();
    pair.pump(32_768);
    let body = pair.server_ports.bodies.pop_front().unwrap();
    let retained_address = body.bytes().as_ptr();
    for _ in 0..128 {
        let index = pair
            .client_ports
            .permits
            .iter()
            .position(|permit| permit.stream() == sibling)
            .unwrap();
        let permit = pair.client_ports.permits.remove(index).unwrap();
        pair.client.send(permit, vec![2; 1024], false).unwrap();
        pair.pump(32_768);
        let chunk = pair.server_ports.bodies.pop_front().unwrap();
        assert_eq!(chunk.stream(), sibling);
        assert_eq!(chunk.bytes(), &[2; 1024]);
        pair.server.release_body(chunk.release()).unwrap();
        pair.pump(32_768);
        assert_eq!(body.bytes().as_ptr(), retained_address);
        assert_eq!(body.bytes(), &[1]);
        assert!(pair.server.receive_capacity <= 3 * PAGE_SIZE);
        assert!(pair.server_ports.closed.is_empty());
    }
    pair.server.release_body(body.release()).unwrap();
}

#[test]
fn small_held_pages_hit_capacity_bound_not_only_payload_bound() {
    let mut pair = Pair::new(Config {
        connection_receive_window: 65_535,
        max_receive_capacity: 4 * PAGE_SIZE,
        max_stream_receive_capacity: 8 * PAGE_SIZE,
        ..Config::default()
    });
    pair.client.request(&request(b"POST"), false).unwrap();
    pair.pump(32_768);
    for _ in 0..4 {
        pair.client
            .send(
                pair.client_ports.permits.pop_front().unwrap(),
                vec![1],
                false,
            )
            .unwrap();
        pair.pump(32_768);
    }
    assert_eq!(pair.server_ports.bodies.len(), 4);
    assert_eq!(pair.server.receive_capacity, 4 * PAGE_SIZE);
    assert_eq!(
        pair.server_ports.closed,
        [ConnectionResult::ResourceExhausted]
    );
    assert!(pair.server_ports.retired.is_empty());
    pair.release_all();
    pair.pump(32_768);
    assert_eq!(pair.server_ports.retired.len(), 1);
    assert_eq!(pair.server.pool.len(), 4);
}
