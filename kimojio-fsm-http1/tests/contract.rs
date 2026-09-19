use kimojio_fsm_http1::*;

type Bytes = Vec<u8>;

#[derive(Debug)]
enum Event {
    Read(ReadOp<Bytes>),
    Write(WriteOp<Bytes>),
    Readiness(ReadinessOp),
    Cancel(CancelOp),
    Close(CloseOp),
    Body(BodyOp<Bytes>),
    Head(ExchangeId, String),
    Response(ExchangeId, u16, bool),
    Demand(ExchangeId, usize),
    Sent(BodySent<Bytes>),
    Finished(ExchangeFinished),
    Incoming(ExchangeId),
    Trailers,
    Deadline,
    Upgrade,
    Closed(ConnectionResult),
}

struct Capture;

impl Ports<Bytes> for Capture {
    type Output = Event;
    fn read(&mut self, op: ReadOp<Bytes>) -> Option<Event> {
        Some(Event::Read(op))
    }
    fn write(&mut self, op: WriteOp<Bytes>) -> Option<Event> {
        Some(Event::Write(op))
    }
    fn readiness(&mut self, op: ReadinessOp) -> Option<Event> {
        Some(Event::Readiness(op))
    }
    fn cancel(&mut self, op: CancelOp) -> Option<Event> {
        Some(Event::Cancel(op))
    }
    fn close(&mut self, op: CloseOp) -> Option<Event> {
        Some(Event::Close(op))
    }
    fn body(&mut self, op: BodyOp<Bytes>) -> Option<Event> {
        Some(Event::Body(op))
    }
    fn trailers(&mut self, _: ExchangeId, _: Headers<'_>) -> Option<Event> {
        Some(Event::Trailers)
    }
    fn incoming_finished(&mut self, id: ExchangeId) -> Option<Event> {
        Some(Event::Incoming(id))
    }
    fn send_ready(&mut self, id: ExchangeId, capacity: usize) -> Option<Event> {
        Some(Event::Demand(id, capacity))
    }
    fn body_sent(&mut self, value: BodySent<Bytes>) -> Option<Event> {
        Some(Event::Sent(value))
    }
    fn exchange_finished(&mut self, value: ExchangeFinished) -> Option<Event> {
        Some(Event::Finished(value))
    }
    fn deadline_changed(&mut self, _: Option<Deadline>) -> Option<Event> {
        Some(Event::Deadline)
    }
    fn upgrade_ready(&mut self, _: ExchangeId) -> Option<Event> {
        Some(Event::Upgrade)
    }
    fn closed(&mut self, result: ConnectionResult) -> Option<Event> {
        Some(Event::Closed(result))
    }
}

impl ServerPorts<Bytes> for Capture {
    fn request(&mut self, id: ExchangeId, head: RequestHead<'_>) -> Option<Event> {
        Some(Event::Head(id, head.target.to_owned()))
    }
}

impl ClientPorts<Bytes> for Capture {
    fn response(&mut self, id: ExchangeId, head: ResponseHead<'_>, info: bool) -> Option<Event> {
        Some(Event::Response(id, head.status, info))
    }
}

fn server(slot: u64) -> Server<Bytes> {
    Server::new(
        ConnectionId {
            slot,
            generation: 1,
        },
        Config::default(),
        vec![0; 1024],
        Tick(0),
    )
    .unwrap()
}

fn next(server: &mut Server<Bytes>) -> Option<Event> {
    loop {
        let event = server.next(&mut Capture);
        if matches!(event, Some(Event::Deadline)) {
            continue;
        }
        return event;
    }
}

fn feed(server: &mut Server<Bytes>, bytes: &[u8]) {
    let Some(Event::Read(mut op)) = next(server) else {
        panic!("expected read")
    };
    op.bytes_mut()[..bytes.len()].copy_from_slice(bytes);
    server.complete_read(op.complete(Ok(bytes.len()))).unwrap();
}

fn request(server: &mut Server<Bytes>, bytes: &[u8]) -> ExchangeId {
    feed(server, bytes);
    let Some(Event::Head(id, target)) = next(server) else {
        panic!("expected request")
    };
    assert_eq!(target, "/");
    id
}

fn response(body: BodyLength) -> Response<'static> {
    Response {
        head: ResponseHead {
            version: Version::Http11,
            status: 200,
            reason: "OK",
            headers: &[],
        },
        body,
    }
}

fn write_all(server: &mut Server<Bytes>, step: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        match next(server) {
            Some(Event::Write(op)) => {
                let offered: Vec<u8> = op.slices().concat();
                let n = offered.len().min(step);
                bytes.extend_from_slice(&offered[..n]);
                server.complete_write(op.complete(Ok(n))).unwrap();
            }
            Some(Event::Incoming(id)) => assert_eq!(id.connection(), server.connection_id()),
            Some(Event::Demand(id, capacity)) => {
                assert_eq!(id.connection(), server.connection_id());
                assert!(capacity > 0);
                return bytes;
            }
            Some(Event::Sent(result)) => {
                assert_eq!(result.accepted, result.buffer.len());
                assert_eq!(result.result, Ok(()));
                return bytes;
            }
            other => {
                assert!(
                    matches!(
                        other,
                        Some(Event::Finished(_)) | Some(Event::Demand(..)) | Some(Event::Sent(_))
                    ),
                    "{other:?}"
                );
                return bytes;
            }
        }
    }
}

#[test]
fn fragmented_request_never_reports_early_success() {
    let mut server = server(1);
    feed(&mut server, b"GET / HTTP/1.1\r\nHost: localhost\r\n");
    assert!(matches!(next(&mut server), Some(Event::Read(_))));
}

#[test]
fn fixed_response_survives_every_partial_write() {
    let mut server = server(1);
    let id = request(&mut server, b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");
    server.respond(id, response(BodyLength::Known(5))).unwrap();
    let head = write_all(&mut server, 1);
    assert_eq!(head, b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\n");
    server
        .send_body(SendBody {
            exchange: id,
            buffer: b"hello".to_vec(),
            range: 0..5,
            end: true,
        })
        .unwrap();
    let body = write_all(&mut server, 1);
    assert_eq!(body, b"hello");
    let Some(Event::Finished(result)) = next(&mut server) else {
        panic!("missing finish")
    };
    assert!(result.reusable);
    assert_eq!(result.result, Ok(()));
    assert!(matches!(next(&mut server), Some(Event::Read(_))));
}

#[test]
fn wrong_owner_completion_returns_live_resource() {
    let mut first = server(1);
    let mut second = server(2);
    let Some(Event::Read(op)) = next(&mut first) else {
        panic!()
    };
    let id = op.id();
    let rejected = second.complete_read(op.complete(Ok(0))).unwrap_err();
    assert_eq!(rejected.reason, RejectReason::WrongConnection);
    let (op, result) = rejected.value.into_parts();
    assert_eq!(op.id(), id);
    first.complete_read(op.complete(result)).unwrap();
    assert!(matches!(next(&mut first), Some(Event::Close(_))));
}

#[test]
fn impossible_read_count_does_not_settle_the_read() {
    let mut server = server(1);
    let Some(Event::Read(op)) = next(&mut server) else {
        panic!()
    };
    let rejected = server.complete_read(op.complete(Ok(1025))).unwrap_err();
    assert_eq!(rejected.reason, RejectReason::InvalidCount);
    assert!(next(&mut server).is_none());
    let (op, _) = rejected.value.into_parts();
    server.complete_read(op.complete(Ok(0))).unwrap();
    assert!(matches!(next(&mut server), Some(Event::Close(_))));
}

#[test]
fn source_failure_settles_pending_demand_and_closes() {
    let mut server = server(1);
    let id = request(&mut server, b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");
    server.respond(id, response(BodyLength::Streaming)).unwrap();
    write_all(&mut server, usize::MAX);
    assert!(next(&mut server).is_none(), "demand must not repeat");
    server.fail_source(id, Failure::Application).unwrap();
    let Some(Event::Finished(result)) = next(&mut server) else {
        panic!()
    };
    assert_eq!(result.result, Err(Failure::Application));
    let Some(Event::Close(op)) = next(&mut server) else {
        panic!()
    };
    server.complete_close(op.complete(Ok(()))).unwrap();
    assert!(matches!(
        next(&mut server),
        Some(Event::Closed(Err(Failure::Application)))
    ));
    let rejected = server
        .send_body(SendBody {
            exchange: id,
            buffer: vec![42],
            range: 0..1,
            end: true,
        })
        .unwrap_err();
    assert_eq!(rejected.value.buffer, [42]);
}

#[test]
fn zero_consumption_withholds_credit_without_blocking_writes() {
    let mut server = server(1);
    let id = request(
        &mut server,
        b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\n\r\nhello",
    );
    server.grant_body_credit(id, 10).unwrap();
    let Some(Event::Body(op)) = next(&mut server) else {
        panic!()
    };
    assert_eq!(op.bytes(), b"hello");
    server.release_body(op.release(0)).unwrap();
    assert!(next(&mut server).is_none());
    server.respond(id, response(BodyLength::Empty)).unwrap();
    assert!(matches!(next(&mut server), Some(Event::Write(_))));
}

#[test]
fn client_reads_while_request_write_is_outstanding() {
    let mut client = Client::new(
        ConnectionId {
            slot: 1,
            generation: 1,
        },
        Config::default(),
        vec![0; 1024],
        Tick(0),
    )
    .unwrap();
    let exchange = client
        .request(Request {
            head: RequestHead {
                method: "GET",
                target: "/",
                version: Version::Http11,
                headers: &[Header {
                    name: "host",
                    value: b"localhost",
                }],
            },
            body: BodyLength::Empty,
            expect_continue: false,
        })
        .unwrap();
    let mut write = None;
    let mut read = None;
    while let Some(event) = client.next(&mut Capture) {
        match event {
            Event::Read(op) => read = Some(op),
            Event::Write(op) => write = Some(op),
            Event::Deadline => {}
            other => panic!("{other:?}"),
        }
    }
    let write = write.unwrap();
    let len = write.slices().iter().map(|s| s.len()).sum();
    client.complete_write(write.complete(Ok(len))).unwrap();
    let mut read = read.unwrap();
    let bytes = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
    read.bytes_mut()[..bytes.len()].copy_from_slice(bytes);
    client
        .complete_read(read.complete(Ok(bytes.len())))
        .unwrap();
    assert!(
        matches!(client.next(&mut Capture), Some(Event::Response(id, 200, false)) if id == exchange)
    );
}

#[test]
fn zero_write_is_not_success() {
    let mut server = server(1);
    let id = request(&mut server, b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");
    server.respond(id, response(BodyLength::Empty)).unwrap();
    let Some(Event::Write(op)) = next(&mut server) else {
        panic!()
    };
    server.complete_write(op.complete(Ok(0))).unwrap();
    let Some(Event::Finished(result)) = next(&mut server) else {
        panic!()
    };
    assert_eq!(result.result, Err(Failure::WriteZero));
}

#[test]
fn would_block_waits_once_and_rejects_duplicate_readiness() {
    let mut server = server(1);
    let Some(Event::Read(op)) = next(&mut server) else {
        panic!()
    };
    server
        .complete_read(op.complete(Err(IoError {
            kind: IoErrorKind::WouldBlock,
            code: None,
        })))
        .unwrap();
    let Some(Event::Readiness(op)) = next(&mut server) else {
        panic!()
    };
    assert_eq!(op.direction, Direction::Read);
    assert!(next(&mut server).is_none());
    let completion = op.complete(Ok(()));
    server.complete_readiness(completion).unwrap();
    assert_eq!(
        server.complete_readiness(completion).unwrap_err().reason,
        RejectReason::Stale
    );
    assert!(matches!(next(&mut server), Some(Event::Read(_))));
}

#[test]
fn abort_does_not_close_before_original_read_settles() {
    let mut server = server(1);
    let Some(Event::Read(op)) = next(&mut server) else {
        panic!()
    };
    let id = op.id();
    server.shutdown(ShutdownMode::Abort);
    let Some(Event::Cancel(cancel)) = next(&mut server) else {
        panic!()
    };
    assert_eq!(cancel.target, id);
    assert!(next(&mut server).is_none());
    server
        .complete_read(op.complete(Err(IoError {
            kind: IoErrorKind::Cancelled,
            code: None,
        })))
        .unwrap();
    assert!(matches!(next(&mut server), Some(Event::Close(_))));
}

#[test]
fn bodyless_responses_finish_without_delivery_credit() {
    let cases: &[(&str, &[u8])] = &[
        ("HEAD", b"HTTP/1.1 200 OK\r\nContent-Length: 42\r\n\r\n"),
        ("GET", b"HTTP/1.1 204 No Content\r\n\r\n"),
        (
            "GET",
            b"HTTP/1.1 304 Not Modified\r\nContent-Length: 42\r\n\r\n",
        ),
        ("GET", b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"),
    ];
    for (method, response) in cases {
        let mut client = Client::new(
            ConnectionId {
                slot: 1,
                generation: 1,
            },
            Config::default(),
            vec![0; 1024],
            Tick(0),
        )
        .unwrap();
        let exchange = client
            .request(Request {
                head: RequestHead {
                    method,
                    target: "/",
                    version: Version::Http11,
                    headers: &[Header {
                        name: "host",
                        value: b"localhost",
                    }],
                },
                body: BodyLength::Empty,
                expect_continue: false,
            })
            .unwrap();
        let mut read = None;
        while let Some(event) = client.next(&mut Capture) {
            match event {
                Event::Read(op) => read = Some(op),
                Event::Write(op) => {
                    let len = op.slices().iter().map(|s| s.len()).sum();
                    client.complete_write(op.complete(Ok(len))).unwrap();
                }
                Event::Deadline => {}
                other => panic!("{other:?}"),
            }
        }
        let mut read = read.unwrap();
        read.bytes_mut()[..response.len()].copy_from_slice(response);
        client
            .complete_read(read.complete(Ok(response.len())))
            .unwrap();
        let mut incoming = false;
        let mut finished = false;
        while let Some(event) = client.next(&mut Capture) {
            match event {
                Event::Response(id, _, false) => assert_eq!(id, exchange),
                Event::Incoming(id) => {
                    assert_eq!(id, exchange);
                    incoming = true;
                }
                Event::Finished(result) => {
                    assert_eq!(result.exchange, exchange);
                    assert_eq!(result.result, Ok(()));
                    assert!(result.reusable);
                    finished = true;
                }
                Event::Deadline => {}
                other => panic!("{method}: unexpected {other:?}"),
            }
        }
        assert!(
            incoming && finished,
            "{method}: bodyless response waited for credit"
        );
    }
}

#[test]
fn unknown_write_progress_is_terminal_and_keeps_acceptance_uncertainty() {
    for cancelled in [false, true] {
        for (kind, acceptance) in [
            (IoErrorKind::Other, Acceptance::Exact),
            (IoErrorKind::UnknownProgress, Acceptance::LowerBound),
        ] {
            let mut server = server(1);
            let id = request(&mut server, b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");
            server.respond(id, response(BodyLength::Known(5))).unwrap();
            write_all(&mut server, usize::MAX);
            server
                .send_body(SendBody {
                    exchange: id,
                    buffer: b"hello".to_vec(),
                    range: 0..5,
                    end: true,
                })
                .unwrap();
            let Some(Event::Write(op)) = next(&mut server) else {
                panic!()
            };
            server.complete_write(op.complete(Ok(2))).unwrap();
            let Some(Event::Write(op)) = next(&mut server) else {
                panic!()
            };
            assert_eq!(op.slices().concat(), b"llo");
            if cancelled {
                server.cancel_exchange(id).unwrap();
                let Some(Event::Cancel(cancel)) = next(&mut server) else {
                    panic!()
                };
                assert_eq!(cancel.target, op.id());
                assert!(matches!(next(&mut server), Some(Event::Finished(_))));
                assert!(next(&mut server).is_none());
            }
            let error = IoError {
                kind,
                code: Some(5),
            };
            server.complete_write(op.complete(Err(error))).unwrap();
            let Some(Event::Sent(sent)) = next(&mut server) else {
                panic!()
            };
            assert_eq!(sent.buffer, b"hello");
            assert_eq!(sent.accepted, 2);
            assert_eq!(sent.acceptance, acceptance);
            assert_eq!(
                sent.result,
                Err(if cancelled {
                    Failure::Cancelled
                } else {
                    Failure::Transport(error)
                })
            );
            if !cancelled {
                assert!(matches!(next(&mut server), Some(Event::Finished(_))));
            }
            assert!(
                matches!(next(&mut server), Some(Event::Close(_))),
                "failed write must not replay"
            );
        }
    }
}
