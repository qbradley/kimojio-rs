#[path = "../examples/support/mod.rs"]
mod support;

use kimojio_fsm_http2::*;
use std::{collections::VecDeque, time::Duration};
use support::*;

fn frame(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let mut bytes = vec![
        (payload.len() >> 16) as u8,
        (payload.len() >> 8) as u8,
        payload.len() as u8,
        kind,
        flags,
    ];
    bytes.extend_from_slice(&stream.to_be_bytes());
    bytes.extend_from_slice(payload);
    bytes
}
fn pump(connection: &mut Connection, ports: &mut MemoryPorts, bytes: &[u8]) -> Vec<u8> {
    let mut incoming = bytes.iter().copied().collect();
    let mut outgoing = VecDeque::new();
    for _ in 0..4096 {
        if !step(connection, ports, &mut incoming, &mut outgoing, 32_768) {
            assert!(incoming.is_empty());
            return outgoing.into();
        }
    }
    panic!("endpoint did not become idle");
}
fn initialize(server: &mut Server, ports: &mut MemoryPorts) {
    let mut preface = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".to_vec();
    preface.extend(frame(4, 0, 0, &[]));
    pump(server, ports, &preface);
    pump(server, ports, &frame(4, 1, 0, &[]));
}
fn goaway(last: u32, code: u32) -> Vec<u8> {
    frame(7, 0, 0, &[last.to_be_bytes(), code.to_be_bytes()].concat())
}

#[test]
fn rejected_shutdown_does_not_commit_a_partial_lifecycle_transition() {
    let mut pair = Pair::new(Config {
        max_outbound_items: 4,
        ..Config::default()
    });
    pair.pump(32_768);
    pair.client.request(&request(b"GET"), true).unwrap();
    pair.client.next(&mut pair.client_ports);
    let original = pair.client_ports.write.take().unwrap();
    for _ in 0..4 {
        pair.client.request(&request(b"GET"), true).unwrap();
    }
    assert_eq!(pair.client.shutdown(), Err(CommandError::Capacity));
    let count = original.remaining();
    pair.client
        .complete_write(original.complete(WriteOutcome::Written(count)))
        .unwrap();
    pump(&mut pair.client, &mut pair.client_ports, &[]);
    assert!(pair.client.request(&request(b"GET"), true).is_ok());
    pump(&mut pair.client, &mut pair.client_ports, &[]);
    pair.client.shutdown().unwrap();
}

#[test]
fn stream_deadline_under_control_pressure_has_an_explicit_aggregate_failure() {
    let mut pair = Pair::new(Config {
        max_outbound_items: 4,
        ..Config::default()
    });
    let first = pair.client.request(&request(b"GET"), true).unwrap();
    pair.pump(32_768);
    let mut siblings = Vec::new();
    for _ in 0..4 {
        siblings.push(pair.client.request(&request(b"GET"), true).unwrap());
        pair.pump(32_768);
    }
    pair.server
        .respond(first, &response(b"200"), false)
        .unwrap();
    pair.server.next(&mut pair.server_ports);
    let original = pair.server_ports.write.take().unwrap();
    for &id in &siblings {
        pair.server.respond(id, &response(b"200"), true).unwrap();
    }
    pair.server
        .set_deadline(first, Some(Duration::ZERO))
        .unwrap();
    pair.server.advance_time(Duration::ZERO).unwrap();
    let count = original.remaining();
    pair.server
        .complete_write(original.complete(WriteOutcome::Written(count)))
        .unwrap();
    let mut expected = Vec::new();
    for &id in &siblings {
        expected.extend(frame(1, 5, id.get(), &[0x88]));
    }
    expected.extend(goaway(siblings.last().unwrap().get(), 11));
    assert_eq!(
        pump(&mut pair.server, &mut pair.server_ports, &[]),
        expected
    );
    assert_eq!(
        pair.server_ports.closed,
        [ConnectionResult::ResourceExhausted]
    );
    let mut retired = vec![StreamResult {
        stream: first,
        outcome: StreamOutcome::Deadline,
    }];
    retired.extend(siblings.into_iter().map(|stream| StreamResult {
        stream,
        outcome: StreamOutcome::Complete,
    }));
    assert_eq!(pair.server_ports.retired, retired);
}

#[test]
fn settings_budget_uses_supplied_time_and_has_an_exact_flood_boundary() {
    let mut client = Client::new(Config::default(), Duration::ZERO).unwrap();
    let mut ports = MemoryPorts::default();
    pump(&mut client, &mut ports, &frame(4, 0, 0, &[]));
    pump(&mut client, &mut ports, &frame(4, 1, 0, &[]));
    client.advance_time(Duration::from_secs(1)).unwrap();
    let sixty_four = frame(4, 0, 0, &[]).repeat(64);
    assert_eq!(
        pump(&mut client, &mut ports, &sixty_four),
        frame(4, 1, 0, &[]).repeat(64)
    );
    client.advance_time(Duration::from_secs(2)).unwrap();
    assert_eq!(
        pump(&mut client, &mut ports, &sixty_four),
        frame(4, 1, 0, &[]).repeat(64)
    );
    assert_eq!(
        pump(&mut client, &mut ports, &frame(4, 0, 0, &[])),
        goaway(0, 1)
    );
    assert!(
        matches!(ports.closed.as_slice(), [ConnectionResult::Protocol(error)]
        if error.code == H2ErrorCode::ProtocolError && error.scope == H2ErrorScope::Connection)
    );
}

#[test]
fn blocked_write_control_pressure_has_one_bounded_emergency_goaway() {
    let mut pair = Pair::new(Config {
        max_outbound_items: 4,
        ..Config::default()
    });
    pair.pump(32_768);
    let id = pair.client.request(&request(b"GET"), true).unwrap();
    pair.client.next(&mut pair.client_ports);
    let original = pair.client_ports.write.take().unwrap();
    let packets = frame(6, 0, 0, b"pressure").repeat(5);
    let mut read = pair.client_ports.read.take().unwrap();
    read.buffer_mut()[..packets.len()].copy_from_slice(&packets);
    pair.client
        .complete_read(read.complete(ReadOutcome::Read(packets.len())))
        .unwrap();
    pair.client.next(&mut pair.client_ports);
    assert!(pair.client_ports.write.is_none());
    assert!(pair.client_ports.retired.is_empty());
    let count = original.remaining();
    pair.client
        .complete_write(original.complete(WriteOutcome::Written(count)))
        .unwrap();
    let output = pump(&mut pair.client, &mut pair.client_ports, &[]);
    let mut expected = frame(6, 1, 0, b"pressure").repeat(4);
    expected.extend(goaway(0, 11));
    assert_eq!(output, expected);
    assert_eq!(
        pair.client_ports.closed,
        [ConnectionResult::ResourceExhausted]
    );
    assert_eq!(
        pair.client_ports.retired,
        [StreamResult {
            stream: id,
            outcome: StreamOutcome::ConnectionFailed
        }]
    );
}

#[test]
fn decoded_byte_limit_refuses_one_stream_without_losing_dynamic_names() {
    let mut server = Server::new(
        Config {
            http: HttpLimits::new().set_max_header_bytes(256),
            ..Config::default()
        },
        Duration::ZERO,
    )
    .unwrap();
    let mut ports = MemoryPorts::default();
    initialize(&mut server, &mut ports);
    let mut encoder = H2HeaderBlockEncoder::new();
    let mut fields = request(b"GET");
    fields.push(H2HeaderField::new(b"x-shared", vec![b'a'; 128]));
    let block = encoder.try_encode_fields(&fields).unwrap();
    assert!(
        block.len() < 256,
        "this case must exceed only the decoded limit"
    );
    assert_eq!(
        pump(&mut server, &mut ports, &frame(1, 5, 1, &block)),
        frame(3, 0, 1, &11u32.to_be_bytes())
    );
    fields.last_mut().unwrap().value = b"ok".to_vec();
    let next = encoder.try_encode_fields(&fields).unwrap();
    assert!(
        next.contains(&0x7e),
        "the next literal must reference dynamic name 62"
    );
    assert!(pump(&mut server, &mut ports, &frame(1, 5, 3, &next)).is_empty());
    assert_eq!(ports.heads.len(), 1);
    assert_eq!(ports.heads[0].2.last().unwrap().value, b"ok");
    assert!(ports.closed.is_empty());
}

#[test]
fn encoded_continuation_limit_is_a_terminal_connection_limit() {
    let mut server = Server::new(
        Config {
            http: HttpLimits::new().set_max_header_bytes(256),
            ..Config::default()
        },
        Duration::ZERO,
    )
    .unwrap();
    let mut ports = MemoryPorts::default();
    initialize(&mut server, &mut ports);
    assert!(pump(&mut server, &mut ports, &frame(1, 0, 1, &[0x82; 200])).is_empty());
    assert_eq!(
        pump(&mut server, &mut ports, &frame(9, 4, 1, &[0x82; 100])),
        goaway(0, 11)
    );
    assert!(ports.heads.is_empty());
    assert!(
        matches!(ports.closed.as_slice(), [ConnectionResult::Protocol(error)]
        if error.code == H2ErrorCode::EnhanceYourCalm && error.scope == H2ErrorScope::Connection)
    );
}

#[test]
fn settings_timeout_starts_after_the_last_exact_preface_write() {
    let mut client = Client::new(Config::default(), Duration::ZERO).unwrap();
    let mut ports = MemoryPorts::default();
    client.next(&mut ports);
    let first = ports.write.take().unwrap();
    assert!(ports.alarms.is_empty());
    client.advance_time(Duration::from_secs(30)).unwrap();
    client
        .complete_write(first.complete(WriteOutcome::Written(3)))
        .unwrap();
    client.next(&mut ports);
    assert!(ports.alarms.is_empty());
    let remainder = ports.write.take().unwrap();
    let count = remainder.remaining();
    client
        .complete_write(remainder.complete(WriteOutcome::Written(count)))
        .unwrap();
    client.next(&mut ports);
    assert_eq!(ports.alarms.len(), 1);
    let alarm = ports.alarms.pop().unwrap();
    assert_eq!(alarm.deadline(), Duration::from_secs(40));
    client.advance_time(Duration::from_secs(39)).unwrap();
    assert!(pump(&mut client, &mut ports, &[]).is_empty());
    client.advance_time(Duration::from_secs(40)).unwrap();
    assert_eq!(pump(&mut client, &mut ports, &[]), goaway(0, 4));
    assert!(
        ports.closed.is_empty(),
        "original alarm still owns a settlement obligation"
    );
    client
        .complete_wake(alarm.complete(Duration::from_secs(40)))
        .unwrap();
    pump(&mut client, &mut ports, &[]);
    assert!(
        matches!(ports.closed.as_slice(), [ConnectionResult::Protocol(error)]
        if error.code == H2ErrorCode::SettingsTimeout)
    );
}

#[test]
fn graceful_ping_wait_starts_after_its_original_write_settles() {
    let mut pair = Pair::new(Config::default());
    let id = pair.client.request(&request(b"GET"), true).unwrap();
    pair.pump(32_768);
    pair.server.shutdown().unwrap();
    pair.server.next(&mut pair.server_ports);
    let first = pair.server_ports.write.take().unwrap();
    pair.server.advance_time(Duration::from_secs(2)).unwrap();
    assert!(pump(&mut pair.server, &mut pair.server_ports, &[]).is_empty());
    pair.server
        .complete_write(first.complete(WriteOutcome::Written(5)))
        .unwrap();
    pair.server.next(&mut pair.server_ports);
    let remainder = pair.server_ports.write.take().unwrap();
    pair.server.advance_time(Duration::from_secs(10)).unwrap();
    let count = remainder.remaining();
    pair.server
        .complete_write(remainder.complete(WriteOutcome::Written(count)))
        .unwrap();
    assert!(pump(&mut pair.server, &mut pair.server_ports, &[]).is_empty());
    assert_eq!(pair.server_ports.alarms.len(), 1);
    assert_eq!(
        pair.server_ports.alarms[0].deadline(),
        Duration::from_secs(11)
    );
    pair.server.advance_time(Duration::from_secs(11)).unwrap();
    assert_eq!(
        pump(&mut pair.server, &mut pair.server_ports, &[]),
        goaway(id.get(), 0)
    );
    assert!(pair.server_ports.closed.is_empty());
}

#[test]
fn protocol_completion_invalidates_deadline_while_the_application_retains_a_body() {
    let mut pair = Pair::new(Config::default());
    let id = pair.client.request(&request(b"GET"), true).unwrap();
    pair.pump(32_768);
    pair.client
        .set_deadline(id, Some(Duration::from_secs(5)))
        .unwrap();
    pair.server.respond(id, &response(b"200"), false).unwrap();
    pair.pump(32_768);
    pair.server
        .send(
            pair.server_ports.permits.pop_front().unwrap(),
            vec![1],
            true,
        )
        .unwrap();
    pair.pump(32_768);
    assert!(pair.client_ports.retired.is_empty());
    assert!(
        pair.client
            .set_deadline(id, Some(Duration::from_secs(10)))
            .is_err()
    );
    pair.client.advance_time(Duration::from_secs(5)).unwrap();
    assert!(pump(&mut pair.client, &mut pair.client_ports, &[]).is_empty());
    pair.release_all();
    pair.pump(32_768);
    assert_eq!(
        pair.client_ports.retired,
        [StreamResult {
            stream: id,
            outcome: StreamOutcome::Complete
        }]
    );
}

#[test]
fn classic_connect_tunnel_bytes_are_not_an_http_message_body_limit() {
    let mut pair = Pair::new(Config {
        http: HttpLimits::new().set_max_body_bytes(1),
        ..Config::default()
    });
    let id = pair
        .client
        .request_ref(
            &[
                H2RawHeaderRef::new(b":method", b"CONNECT"),
                H2RawHeaderRef::new(b":authority", b"example.test:443"),
            ],
            false,
        )
        .unwrap();
    pair.pump(32_768);
    pair.server
        .respond_ref(id, &[H2RawHeaderRef::new(b":status", b"200")], false)
        .unwrap();
    pair.pump(32_768);
    pair.client
        .send(
            pair.client_ports.permits.pop_front().unwrap(),
            vec![1; 100],
            true,
        )
        .unwrap();
    pair.server
        .send(
            pair.server_ports.permits.pop_front().unwrap(),
            vec![2; 100],
            true,
        )
        .unwrap();
    pair.pump(32_768);
    assert_eq!(pair.server_ports.bodies[0].bytes(), &[1; 100]);
    assert_eq!(pair.client_ports.bodies[0].bytes(), &[2; 100]);
    pair.release_all();
    pair.pump(32_768);
    assert_eq!(pair.client_ports.retired.len(), 1);
    assert_eq!(pair.server_ports.retired.len(), 1);
}

#[test]
fn malformed_trailer_resets_only_its_stream_and_preserves_held_credit() {
    let mut pair = Pair::new(Config::default());
    let id = pair.client.request(&request(b"POST"), false).unwrap();
    pair.pump(32_768);
    pair.client
        .send(
            pair.client_ports.permits.pop_front().unwrap(),
            b"abc".to_vec(),
            false,
        )
        .unwrap();
    pair.pump(32_768);
    let body = pair.server_ports.bodies.pop_front().unwrap();
    // Literal without indexing, static name 28 (content-length), value "3".
    let invalid = frame(1, 5, id.get(), &[0x0f, 0x0d, 1, b'3']);
    assert_eq!(
        pump(&mut pair.server, &mut pair.server_ports, &invalid),
        frame(3, 0, id.get(), &1u32.to_be_bytes())
    );
    assert!(pair.server_ports.retired.is_empty());
    pair.server.release_body(body.release()).unwrap();
    assert_eq!(
        pump(&mut pair.server, &mut pair.server_ports, &[]),
        frame(8, 0, 0, &3u32.to_be_bytes())
    );
    assert_eq!(
        pair.server_ports.retired,
        [StreamResult {
            stream: id,
            outcome: StreamOutcome::Reset(1)
        }]
    );
    assert!(pair.server_ports.closed.is_empty());
}
