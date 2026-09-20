#[path = "../examples/support/mod.rs"]
mod support;

use kimojio_fsm_http2::*;
use std::{collections::VecDeque, time::Duration};
use support::*;

fn fields() -> [H2RawHeaderRef<'static>; 5] {
    [
        H2RawHeaderRef::new(b":method", b"GET"),
        H2RawHeaderRef::new(b":scheme", b"https"),
        H2RawHeaderRef::new(b":authority", b"example.test"),
        H2RawHeaderRef::new(b":path", b"/"),
        H2RawHeaderRef::new(b"x-shared", b"transaction"),
    ]
}

fn drive(connection: &mut Connection, ports: &mut MemoryPorts) {
    for _ in 0..256 {
        let before = ports.sequence.len();
        connection.next(ports);
        if ports.sequence.len() == before {
            return;
        }
    }
    panic!("drive did not become idle");
}

fn pump(connection: &mut Connection, ports: &mut MemoryPorts, input: &[u8]) -> Vec<u8> {
    let mut input = input.iter().copied().collect();
    let mut output = VecDeque::new();
    for _ in 0..1024 {
        if !step(connection, ports, &mut input, &mut output, 32_768) {
            assert!(input.is_empty());
            return output.into_iter().collect();
        }
    }
    panic!("transport did not become idle");
}

fn settings(concurrency: u32) -> Vec<u8> {
    let mut bytes = vec![0, 0, 6, 4, 0, 0, 0, 0, 0, 0, 3];
    bytes.extend(concurrency.to_be_bytes());
    bytes
}

#[test]
fn queue_blockage_is_transactional_coalesced_and_released_at_write_issuance() {
    for yield_admission in [false, true] {
        for turn_budget in [1, 128] {
            let config = Config {
                max_outbound_items: 4,
                turn_budget,
                ..Config::default()
            };
            let mut client = Client::new(config.clone(), Duration::ZERO).unwrap();
            let mut reference = Client::new(config, Duration::ZERO).unwrap();
            let mut ports = MemoryPorts {
                yield_admission,
                ..MemoryPorts::default()
            };
            let mut reference_ports = MemoryPorts::default();
            for expected in [1, 3, 5] {
                assert_eq!(client.request_ref(&fields(), true).unwrap().get(), expected);
                reference.request_ref(&fields(), true).unwrap();
            }
            let mut retry_fields = fields().to_vec();
            retry_fields.push(H2RawHeaderRef::new(b"x-blocked", b"new-dynamic-entry"));
            for _ in 0..1024 {
                assert_eq!(
                    client.request_ref(&retry_fields, true),
                    Err(CommandError::Blocked)
                );
            }
            assert_eq!(ports.admissions, 0);
            drive(&mut client, &mut ports);
            drive(&mut reference, &mut reference_ports);
            assert!(ports.write.is_some());
            assert_eq!(ports.admissions, 1);
            for _ in 0..1024 {
                client.next(&mut ports);
            }
            assert_eq!(ports.admissions, 1);
            assert_eq!(client.request_ref(&retry_fields, true).unwrap().get(), 7);
            assert_eq!(reference.request_ref(&retry_fields, true).unwrap().get(), 7);
            assert_eq!(
                pump(&mut client, &mut ports, &[]),
                pump(&mut reference, &mut reference_ports, &[]),
                "Blocked must not consume an ID, mutate HPACK, or enqueue wire bytes"
            );
            assert_eq!(ports.admissions, 1, "successful commands do not rearm");
            assert_eq!(reference_ports.admissions, 0);
        }
    }
}

#[test]
fn permanent_metadata_bounds_do_not_arm_admission() {
    let config = Config {
        max_outbound_capacity: 65_536,
        http: HttpLimits::new().set_max_header_bytes(131_072),
        ..Config::default()
    };
    assert!(matches!(
        Client::<Vec<u8>>::new(
            Config {
                max_outbound_items: 3,
                ..config
            },
            Duration::ZERO
        ),
        Err(CommandError::Capacity)
    ));
    let value = vec![b'a'; 70_000];
    let mut large = fields().to_vec();
    large.push(H2RawHeaderRef::new(b"x-large", &value));
    let mut client = Client::new(config.clone(), Duration::ZERO).unwrap();
    let mut ports = MemoryPorts::default();
    for _ in 0..2 {
        assert_eq!(
            client.request_ref(&large, true),
            Err(CommandError::Capacity)
        );
        pump(&mut client, &mut ports, &[]);
    }
    assert_eq!(ports.admissions, 0);
    assert_eq!(client.request_ref(&fields(), true).unwrap().get(), 1);

    let mut pair = Pair::new(config);
    let id = pair.client.request_ref(&fields(), true).unwrap();
    pair.pump(32_768);
    let response = [
        H2RawHeaderRef::new(b":status", b"200"),
        H2RawHeaderRef::new(b"x-large", &value),
    ];
    assert_eq!(
        pair.server.respond_ref(id, &response, false),
        Err(CommandError::Capacity)
    );
    pair.server.respond_ref(id, &response[..1], false).unwrap();
    pair.pump(32_768);
    assert_eq!(
        pair.server.trailers_ref(id, &response[1..]),
        Err(CommandError::Capacity)
    );
    pair.server
        .trailers_ref(id, &[H2RawHeaderRef::new(b"x-trailer", b"ok")])
        .unwrap();
    pair.pump(32_768);
    assert_eq!(pair.server_ports.admissions, 0);
    assert_eq!(
        pair.client_ports.retired[0].outcome,
        StreamOutcome::Complete
    );
}

#[test]
fn control_bytes_remain_reserved_through_partial_write() {
    for yield_admission in [false, true] {
        let config = Config {
            max_outbound_capacity: 65_536,
            ..Config::default()
        };
        let mut client = Client::new(config.clone(), Duration::ZERO).unwrap();
        let mut ports = MemoryPorts {
            yield_admission,
            ..MemoryPorts::default()
        };
        let mut wire = pump(&mut client, &mut ports, &[]);
        let value = vec![b'a'; 38_000];
        let mut request = fields().to_vec();
        request.push(H2RawHeaderRef::new(b"x-pad", &value));
        assert_eq!(client.request_ref(&request, true).unwrap().get(), 1);
        drive(&mut client, &mut ports);
        let first = ports.write.take().unwrap();
        wire.push(first.slices()[0][0]);
        assert_eq!(client.request_ref(&request, true).unwrap().get(), 3);
        assert_eq!(
            client.request_ref(&request, true),
            Err(CommandError::Blocked)
        );
        client
            .complete_write(first.complete(WriteOutcome::Written(1)))
            .unwrap();
        drive(&mut client, &mut ports);
        assert_eq!(ports.admissions, 0);
        assert_eq!(
            client.request_ref(&request, true),
            Err(CommandError::Blocked)
        );
        let rest = ports.write.take().unwrap();
        wire.extend(rest.slices().iter().flat_map(|bytes| bytes.iter().copied()));
        let length = rest.remaining();
        client
            .complete_write(rest.complete(WriteOutcome::Written(length)))
            .unwrap();
        drive(&mut client, &mut ports);
        assert_eq!(ports.admissions, 1);
        assert_eq!(client.request_ref(&request, true).unwrap().get(), 5);
        wire.extend(pump(&mut client, &mut ports, &[]));
        assert_eq!(ports.admissions, 1);
        let mut server = Server::new(config, Duration::ZERO).unwrap();
        let mut server_ports = MemoryPorts::default();
        pump(&mut server, &mut server_ports, &wire);
        assert_eq!(
            server_ports
                .heads
                .iter()
                .map(|head| head.0.get())
                .collect::<Vec<_>>(),
            [1, 3, 5]
        );
        for head in &server_ports.heads {
            assert_eq!(head.2.last().unwrap().value, value);
        }
    }
}

#[test]
fn settings_and_reset_notify_changes_not_guaranteed_admission() {
    for yield_admission in [false, true] {
        let mut client = Client::new(Config::default(), Duration::ZERO).unwrap();
        let mut ports = MemoryPorts {
            yield_admission,
            ..MemoryPorts::default()
        };
        pump(&mut client, &mut ports, &settings(0));
        assert_eq!(
            client.request_ref(&fields(), true),
            Err(CommandError::Blocked)
        );
        drive(&mut client, &mut ports);
        assert_eq!(ports.admissions, 0);
        pump(&mut client, &mut ports, &settings(1));
        assert_eq!(ports.admissions, 1);
        assert!(
            ports.permits.is_empty(),
            "body permits are not request readiness"
        );
        assert_eq!(client.request_ref(&fields(), true).unwrap().get(), 1);
        pump(&mut client, &mut ports, &[]);
        assert_eq!(
            client.request_ref(&fields(), true),
            Err(CommandError::Blocked)
        );
        pump(&mut client, &mut ports, &settings(0));
        assert_eq!(ports.admissions, 2);
        assert_eq!(
            client.request_ref(&fields(), true),
            Err(CommandError::Blocked)
        );
        for _ in 0..1024 {
            client.next(&mut ports);
        }
        assert_eq!(ports.admissions, 2);
        pump(&mut client, &mut ports, &settings(1));
        assert_eq!(ports.admissions, 3);
        assert_eq!(
            client.request_ref(&fields(), true),
            Err(CommandError::Blocked)
        );
        pump(
            &mut client,
            &mut ports,
            &[0, 0, 4, 3, 0, 0, 0, 0, 1, 0, 0, 0, 8],
        );
        assert_eq!(ports.admissions, 4, "reset and retirement coalesce");
        assert_eq!(client.request_ref(&fields(), true).unwrap().get(), 3);
    }
}

#[test]
fn response_and_trailer_blockage_preserve_hpack_and_stream_state() {
    for yield_admission in [false, true] {
        let mut pair = Pair::new(Config {
            max_outbound_items: 4,
            ..Config::default()
        });
        pair.server_ports.yield_admission = yield_admission;
        let first = pair.client.request_ref(&fields(), true).unwrap();
        let second = pair.client.request_ref(&fields(), true).unwrap();
        pair.pump(32_768);
        pair.server
            .respond(first, &response(b"200"), false)
            .unwrap();
        pair.pump(32_768);
        for _ in 0..4 {
            pair.server
                .respond(second, &response(b"103"), false)
                .unwrap();
        }
        let final_fields = [
            H2RawHeaderRef::new(b":status", b"200"),
            H2RawHeaderRef::new(b"x-transaction", b"committed-once"),
        ];
        assert_eq!(
            pair.server.respond_ref(second, &final_fields, true),
            Err(CommandError::Blocked)
        );
        assert_eq!(
            pair.server.trailers_ref(first, &final_fields[1..]),
            Err(CommandError::Blocked)
        );
        pair.pump(32_768);
        assert_eq!(pair.server_ports.admissions, 1);
        pair.server
            .respond_ref(second, &final_fields, true)
            .unwrap();
        pair.server.trailers_ref(first, &final_fields[1..]).unwrap();
        pair.pump(32_768);
        assert_eq!(pair.server_ports.admissions, 1);
        assert_eq!(
            pair.client_ports
                .heads
                .iter()
                .map(|head| head.1)
                .collect::<Vec<_>>(),
            [
                HeadKind::Response(200),
                HeadKind::Informational(103),
                HeadKind::Informational(103),
                HeadKind::Informational(103),
                HeadKind::Informational(103),
                HeadKind::Response(200),
                HeadKind::Trailers,
            ]
        );
        let trailer = pair.client_ports.heads.last().unwrap();
        assert_eq!(trailer.2, [final_fields[1].to_owned()]);
        assert_eq!(pair.client_ports.retired.len(), 2);
        assert!(
            pair.client_ports
                .retired
                .iter()
                .all(|result| result.outcome == StreamOutcome::Complete)
        );
        assert!(pair.client_ports.closed.is_empty());
    }
}

#[test]
fn terminal_commands_wake_without_waiting_for_body_retirement() {
    for terminal in 0..4 {
        for yield_admission in [false, true] {
            let mut pair = Pair::new(Config {
                http: HttpLimits::new().set_max_active_streams(1),
                ..Config::default()
            });
            pair.client_ports.yield_admission = yield_admission;
            let id = pair.client.request_ref(&fields(), true).unwrap();
            pair.pump(32_768);
            pair.server.respond(id, &response(b"200"), false).unwrap();
            pair.pump(32_768);
            pair.server
                .send(
                    pair.server_ports.permits.pop_front().unwrap(),
                    vec![7],
                    true,
                )
                .unwrap();
            pair.pump(32_768);
            let body = pair.client_ports.bodies.pop_front().unwrap();
            assert!(pair.client_ports.retired.is_empty());
            assert_eq!(
                pair.client.request_ref(&fields(), true),
                Err(CommandError::Blocked)
            );
            match terminal {
                0 => pair.client.reset(id, H2ErrorCode::Cancel).unwrap(),
                1 => pair
                    .to_client
                    .extend([0, 0, 8, 7, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0]),
                2 => pair.client.abort(),
                3 => pair.client.shutdown().unwrap(),
                _ => unreachable!(),
            }
            pair.pump(32_768);
            assert_eq!(pair.client_ports.admissions, 1);
            assert!(pair.client_ports.retired.is_empty());
            assert_eq!(
                pair.client.request_ref(&fields(), true),
                Err(if terminal == 0 {
                    CommandError::Blocked
                } else {
                    CommandError::InvalidState
                })
            );
            pair.client.release_body(body.release()).unwrap();
            pair.pump(32_768);
            assert_eq!(pair.client_ports.retired.len(), 1);
            assert_eq!(
                pair.client_ports.admissions,
                if terminal == 0 { 2 } else { 1 }
            );
            if terminal == 0 {
                assert_eq!(pair.client.request_ref(&fields(), true).unwrap().get(), 3);
            } else {
                assert_eq!(
                    pair.client.request_ref(&fields(), true),
                    Err(CommandError::InvalidState)
                );
            }
        }
    }
}

#[test]
fn protocol_slot_release_precedes_application_and_transport_joins() {
    for send_end in [false, true] {
        for yield_admission in [false, true] {
            let mut pair = Pair::new(Config::default());
            pair.server = Server::new(
                Config {
                    http: HttpLimits::new().set_max_active_streams(1),
                    ..Config::default()
                },
                Duration::ZERO,
            )
            .unwrap();
            pair.client_ports.yield_admission = yield_admission;
            let id = pair.client.request_ref(&fields(), !send_end).unwrap();
            pair.pump(32_768);
            assert_eq!(
                pair.client.request_ref(&fields(), true),
                Err(CommandError::Blocked)
            );
            pair.server
                .respond(id, &response(b"200"), send_end)
                .unwrap();
            pair.pump(32_768);
            assert_eq!(pair.client_ports.admissions, 0);
            if send_end {
                pair.client
                    .send(
                        pair.client_ports.permits.pop_front().unwrap(),
                        vec![9],
                        true,
                    )
                    .unwrap();
                drive(&mut pair.client, &mut pair.client_ports);
                assert!(
                    pair.client_ports.write.is_some(),
                    "the original DATA write is still owned"
                );
            } else {
                pair.server
                    .send(
                        pair.server_ports.permits.pop_front().unwrap(),
                        vec![9],
                        true,
                    )
                    .unwrap();
                pair.pump(32_768);
                assert_eq!(pair.client_ports.bodies.len(), 1);
            }
            assert_eq!(pair.client_ports.admissions, 1);
            assert!(pair.client_ports.retired.is_empty());
            assert_eq!(pair.client.request_ref(&fields(), true).unwrap().get(), 3);
            if send_end {
                let data = pair.client_ports.write.take().unwrap();
                let wire: Vec<_> = data
                    .slices()
                    .iter()
                    .flat_map(|bytes| bytes.iter().copied())
                    .collect();
                pair.client
                    .complete_write(data.complete(WriteOutcome::Written(wire.len())))
                    .unwrap();
                pair.to_client
                    .extend(pump(&mut pair.server, &mut pair.server_ports, &wire));
                pair.server
                    .release_body(pair.server_ports.bodies.pop_front().unwrap().release())
                    .unwrap();
            } else {
                pair.client
                    .release_body(pair.client_ports.bodies.pop_front().unwrap().release())
                    .unwrap();
            }
            pair.pump(32_768);
            assert_eq!(
                pair.client_ports.retired,
                [StreamResult {
                    stream: id,
                    outcome: StreamOutcome::Complete,
                }]
            );
            assert_eq!(pair.client_ports.admissions, 1);
            assert_eq!(pair.server_ports.heads.len(), 2);
        }
    }
}

#[test]
fn reset_invalidates_blocked_response_with_original_write_and_body_still_owned() {
    for yield_admission in [false, true] {
        let mut pair = Pair::new(Config {
            max_outbound_items: 4,
            ..Config::default()
        });
        pair.server_ports.yield_admission = yield_admission;
        let id = pair.client.request(&request(b"POST"), false).unwrap();
        pair.pump(32_768);
        pair.client
            .send(
                pair.client_ports.permits.pop_front().unwrap(),
                vec![1],
                false,
            )
            .unwrap();
        pair.pump(32_768);
        let body = pair.server_ports.bodies.pop_front().unwrap();
        for _ in 0..4 {
            pair.server.respond(id, &response(b"103"), false).unwrap();
        }
        drive(&mut pair.server, &mut pair.server_ports);
        let original = pair.server_ports.write.take().unwrap();
        pair.server.respond(id, &response(b"103"), false).unwrap();
        assert_eq!(
            pair.server.respond(id, &response(b"200"), true),
            Err(CommandError::Blocked)
        );
        pump(
            &mut pair.server,
            &mut pair.server_ports,
            &[0, 0, 4, 3, 0, 0, 0, 0, 1, 0, 0, 0, 8],
        );
        assert_eq!(pair.server_ports.admissions, 1);
        assert!(pair.server_ports.retired.is_empty());
        assert_eq!(
            pair.server.respond(id, &response(b"200"), true),
            Err(CommandError::InvalidState)
        );
        let length = original.remaining();
        pair.server
            .complete_write(original.complete(WriteOutcome::Written(length)))
            .unwrap();
        pump(&mut pair.server, &mut pair.server_ports, &[]);
        assert!(pair.server_ports.retired.is_empty());
        pair.server.release_body(body.release()).unwrap();
        drive(&mut pair.server, &mut pair.server_ports);
        assert_eq!(
            pair.server_ports.retired[0].outcome,
            StreamOutcome::Reset(8)
        );
        assert_eq!(pair.server_ports.admissions, 1);
    }
}
