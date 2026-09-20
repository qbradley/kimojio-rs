use super::*;

fn event(machine: &mut Machine, deadline: &mut Option<core::Deadline>) -> Event {
    loop {
        let event = match machine {
            Machine::Client(client) => client.next(&mut Ports),
            Machine::Server(server) => server.next(&mut Ports),
        };
        match event {
            Some(Event::Deadline(value)) => *deadline = value,
            Some(Event::SourceFinished(_) | Event::IncomingFinished(_)) => {}
            Some(event) => return event,
            None => panic!("expected an eligible transition"),
        }
    }
}

fn observe(machine: &mut Machine, tick: u64) {
    match machine {
        Machine::Client(client) => client.observe_time(core::Tick(tick)),
        Machine::Server(server) => server.observe_time(core::Tick(tick)),
    }
    .unwrap();
}

fn complete(machine: &mut Machine, op: core::WriteOp<OutgoingData>) {
    let len = op.slices().iter().map(|slice| slice.len()).sum();
    let result = op.complete(Ok(len));
    match machine {
        Machine::Client(client) => client.complete_write(result),
        Machine::Server(server) => server.complete_write(result),
    }
    .unwrap();
}

fn deadline_boundaries(server_role: bool) {
    for full in [false, true] {
        for coalesce in [false, true] {
            let id = core::ConnectionId {
                slot: 97,
                generation: 1,
            };
            let mut config = Config::new(id);
            assert!(!config.coalesce_full_bodies);
            config.coalesce_full_bodies = coalesce;
            config.protocol.head_timeout_ns = None;
            config.protocol.idle_timeout_ns = None;
            config.protocol.continue_timeout_ns = None;
            config.protocol.body_timeout_ns = Some(10);
            let mut machine = if server_role {
                Machine::Server(
                    core::Server::with_output_type(
                        id,
                        config.protocol,
                        vec![0; 1024],
                        core::Tick(0),
                    )
                    .unwrap(),
                )
            } else {
                Machine::Client(
                    core::Client::with_output_type(
                        id,
                        config.protocol,
                        vec![0; 1024],
                        core::Tick(0),
                    )
                    .unwrap(),
                )
            };
            let mut deadline = None;
            let exchange = if server_role {
                let Event::Read(mut op) = event(&mut machine, &mut deadline) else {
                    panic!()
                };
                let bytes = b"GET / HTTP/1.1\r\nhost: test\r\n\r\n";
                op.bytes_mut()[..bytes.len()].copy_from_slice(bytes);
                let Machine::Server(server) = &mut machine else {
                    unreachable!()
                };
                server.complete_read(op.complete(Ok(bytes.len()))).unwrap();
                let Event::Request(exchange, request) = event(&mut machine, &mut deadline) else {
                    panic!()
                };
                request.unwrap();
                let Machine::Server(server) = &mut machine else {
                    unreachable!()
                };
                server
                    .respond(
                        exchange,
                        core::Response::new(200, "OK", &[], core::BodyLength::Known(3)),
                    )
                    .unwrap();
                exchange
            } else {
                let Machine::Client(client) = &mut machine else {
                    unreachable!()
                };
                client
                    .request(core::Request {
                        head: core::RequestHead {
                            method: "POST",
                            target: "/",
                            version: core::Version::Http11,
                            headers: &[core::Header {
                                name: "host",
                                value: b"test",
                            }],
                        },
                        body: core::BodyLength::Known(3),
                        expect_continue: false,
                    })
                    .unwrap()
            };
            let mut body = if full {
                OutgoingBody::full(b"abc")
            } else {
                OutgoingBody::from_stream(
                    Some(3),
                    futures::stream::iter([Ok(OutgoingFrame::Data(b"abc".to_vec()))]),
                )
            };
            let eager = admit_eager(
                &mut machine,
                exchange,
                &mut body,
                1024,
                config.coalesce_full_bodies,
            );
            assert_eq!(eager, full && coalesce);
            let Event::Write(mut op) = event(&mut machine, &mut deadline) else {
                panic!()
            };
            assert_eq!(op.slices()[1].is_empty(), !eager);
            observe(&mut machine, 9);
            if !eager {
                complete(&mut machine, op);
                let Event::SendReady(id, 3) = event(&mut machine, &mut deadline) else {
                    panic!()
                };
                assert_eq!(id, exchange);
                let Some(Ok(OutgoingFrame::Data(bytes))) =
                    futures::executor::block_on(body.source.next())
                else {
                    panic!()
                };
                let command = core::SendBody {
                    exchange,
                    buffer: OutgoingData::Owned(bytes),
                    range: 0..3,
                    end: false,
                };
                match &mut machine {
                    Machine::Client(client) => client.send_body(command),
                    Machine::Server(server) => server.send_body(command),
                }
                .unwrap();
                op = match event(&mut machine, &mut deadline) {
                    Event::Write(op) => op,
                    _ => panic!(),
                };
                assert_eq!(deadline.unwrap().at, core::Tick(19));
            } else {
                // A generic write-all operation hides its metadata progress at tick 9.
                assert_eq!(deadline.unwrap().at, core::Tick(10));
            }
            observe(&mut machine, 10);
            if eager {
                let deadline = deadline.unwrap();
                match &mut machine {
                    Machine::Client(client) => client.expire(deadline, core::Tick(10)),
                    Machine::Server(server) => server.expire(deadline, core::Tick(10)),
                }
                .unwrap();
                let Event::Cancel(cancel) = event(&mut machine, &mut None) else {
                    panic!()
                };
                assert_eq!(cancel.target, op.id());
            }
            observe(&mut machine, 11);
            complete(&mut machine, op);
            let Event::BodySent(receipt) = event(&mut machine, &mut deadline) else {
                panic!()
            };
            assert_eq!(receipt.accepted, 3);
            assert_eq!(receipt.acceptance, core::Acceptance::Exact);
            assert_eq!(
                receipt.result,
                if eager {
                    Err(core::Failure::Timeout)
                } else {
                    Ok(())
                }
            );
        }
    }
}

#[test]
fn client_full_and_stream_deadline_boundaries_require_explicit_coalescing() {
    deadline_boundaries(false);
}

#[test]
fn server_full_and_stream_deadline_refresh_requires_explicit_coalescing() {
    deadline_boundaries(true);
}
