mod support;
use kimojio_fsm_http1::*;
use support::*;

#[derive(Default)]
struct Ports {
    deadlines: Vec<Option<Deadline>>,
    logs: Vec<LogEvent>,
}
impl kimojio_fsm_http1::Ports<B> for Ports {
    type Output = Event;
    fn log(&mut self, _: ConnectionId, _: Tick, event: LogEvent) {
        self.logs.push(event);
    }
    fn read(&mut self, op: ReadOp<B>) -> Option<Event> {
        Capture.read(op)
    }
    fn write(&mut self, op: WriteOp<B>) -> Option<Event> {
        Capture.write(op)
    }
    fn readiness(&mut self, op: ReadinessOp) -> Option<Event> {
        Capture.readiness(op)
    }
    fn cancel(&mut self, op: CancelOp) -> Option<Event> {
        Capture.cancel(op)
    }
    fn close(&mut self, op: CloseOp) -> Option<Event> {
        Capture.close(op)
    }
    fn body(&mut self, op: BodyOp<B>) -> Option<Event> {
        Capture.body(op)
    }
    fn trailers(&mut self, id: ExchangeId, h: Headers<'_>) -> Option<Event> {
        Capture.trailers(id, h)
    }
    fn incoming_finished(&mut self, id: ExchangeId) -> Option<Event> {
        Capture.incoming_finished(id)
    }
    fn send_ready(&mut self, id: ExchangeId, n: usize) -> Option<Event> {
        Capture.send_ready(id, n)
    }
    fn body_sent(&mut self, receipt: BodySent<B>) -> Option<Event> {
        Capture.body_sent(receipt)
    }
    fn exchange_finished(&mut self, result: ExchangeFinished) -> Option<Event> {
        Capture.exchange_finished(result)
    }
    fn deadline_changed(&mut self, deadline: Option<Deadline>) -> Option<Event> {
        self.deadlines.push(deadline);
        None // All tests deliberately exercise non-yielding deadline callbacks.
    }
    fn upgrade_ready(&mut self, id: ExchangeId) -> Option<Event> {
        Capture.upgrade_ready(id)
    }
    fn closed(&mut self, result: ConnectionResult) -> Option<Event> {
        Capture.closed(result)
    }
}
impl ServerPorts<B> for Ports {
    fn request(&mut self, id: ExchangeId, h: RequestHead<'_>) -> Option<Event> {
        Capture.request(id, h)
    }
}
impl ClientPorts<B> for Ports {
    fn response(&mut self, id: ExchangeId, h: ResponseHead<'_>, info: bool) -> Option<Event> {
        Capture.response(id, h, info)
    }
}

#[test]
fn initial_due_deadline_closes_without_read_and_legacy_driving_stays_manual() {
    let cfg = Config {
        head_timeout_ns: Some(0),
        ..config()
    };
    let mut clocked = server(cfg.clone());
    let mut ports = Ports::default();
    let Some(Event::Close(close)) = clocked.next_at(Tick(0), &mut ports).unwrap() else {
        panic!("expired initial deadline issued work")
    };
    assert_eq!(ports.deadlines, [None]);
    clocked.complete_close(close.complete(Ok(()))).unwrap();
    assert!(matches!(
        clocked.next_at(Tick(0), &mut ports).unwrap(),
        Some(Event::Closed(Err(Failure::Timeout)))
    ));
    assert!(clocked.next_at(Tick(10), &mut ports).unwrap().is_none());

    let mut manual = server(cfg);
    manual.observe_time(Tick(10)).unwrap();
    assert!(matches!(
        manual.next(&mut Capture),
        Some(Event::Deadline(Some(_)))
    ));
    assert!(
        matches!(manual.next(&mut Capture), Some(Event::Read(_))),
        "legacy caller still chooses expiry ordering"
    );
}

#[test]
fn time_regression_is_transactional_for_both_clocked_entry_points() {
    let mut server = server(config());
    server.advance_time(Tick(10)).unwrap();
    let before = format!("{server:?}");
    let mut ports = Ports::default();
    assert!(matches!(
        server.next_at(Tick(9), &mut ports),
        Err(CommandError::TimeRegression)
    ));
    assert_eq!(
        server.advance_time(Tick(9)),
        Err(CommandError::TimeRegression)
    );
    assert_eq!(format!("{server:?}"), before);
    assert!(ports.logs.is_empty() && ports.deadlines.is_empty());
}

#[test]
fn zero_head_deadline_created_inside_drive_preempts_reused_and_pipelined_heads() {
    for pipelined in [false, true] {
        let mut server = server(Config {
            head_timeout_ns: Some(0),
            ..config()
        });
        // Legacy/manual driving deliberately admits the first request. This lets
        // the next request start its zero head deadline inside next_at itself.
        let first = b"GET / HTTP/1.1\r\nHost: a\r\n\r\n";
        let second = b"GET /second HTTP/1.1\r\nHost: a\r\n\r\n";
        let wire = [
            first.as_slice(),
            if pipelined { second.as_slice() } else { b"" },
        ]
        .concat();
        let id = feed_server(&mut server, &wire);
        server.respond(id, response(BodyLength::Empty)).unwrap();
        loop {
            match next_server(&mut server).unwrap() {
                Event::Write(op) => server.complete_write(finish_write(op)).unwrap(),
                Event::Incoming(_) => {}
                Event::Finished(f) => {
                    assert!(f.reusable);
                    break;
                }
                other => panic!("{other:?}"),
            }
        }
        let mut ports = Ports::default();
        if !pipelined {
            let Some(Event::Read(op)) = server.next_at(Tick(50), &mut ports).unwrap() else {
                panic!()
            };
            server.complete_read(fill(op, second)).unwrap();
        }
        let Some(Event::Write(error)) = server.next_at(Tick(50), &mut ports).unwrap() else {
            panic!("new due head was dispatched")
        };
        assert!(error.slices().concat().starts_with(b"HTTP/1.1 408 "));
        assert!(
            !ports
                .logs
                .iter()
                .any(|e| matches!(e, LogEvent::RequestReceived { .. }))
        );
        server.complete_write(finish_write(error)).unwrap();
        assert!(matches!(
            server.next_at(Tick(50), &mut ports).unwrap(),
            Some(Event::Close(_))
        ));
    }
}

fn expect_client(
    head: Option<u64>,
    continuation: u64,
) -> (Client<B>, ExchangeId, ReadOp<B>, Ports) {
    let mut client = client(Config {
        head_timeout_ns: head,
        continue_timeout_ns: Some(continuation),
        ..config()
    });
    let id = client
        .request(get(
            "POST",
            BodyLength::Known(3),
            true,
            &[Header {
                name: "host",
                value: b"a",
            }],
        ))
        .unwrap();
    let mut ports = Ports::default();
    let Some(Event::Write(op)) = client.next_at(Tick(0), &mut ports).unwrap() else {
        panic!()
    };
    client.complete_write(finish_write(op)).unwrap();
    let Some(Event::Read(read)) = client.next_at(Tick(0), &mut ports).unwrap() else {
        panic!()
    };
    (client, id, read, ports)
}

#[test]
fn continue_fallback_releases_gate_but_cannot_hide_an_also_due_head_deadline() {
    let (mut client, id, _read, mut ports) = expect_client(Some(100), 10);
    client.advance_time(Tick(10)).unwrap();
    assert!(
        matches!(client.next_at(Tick(10), &mut ports).unwrap(), Some(Event::Demand(exchange, 3)) if exchange == id)
    );
    assert_eq!(ports.deadlines.last().unwrap().unwrap().at, Tick(100));
    let (mut client, _, read, mut ports) = expect_client(Some(100), 10);
    assert!(
        matches!(client.next_at(Tick(150), &mut ports).unwrap(), Some(Event::Cancel(c)) if c.target == read.id())
    );
    assert!(
        !ports
            .logs
            .iter()
            .any(|e| matches!(e, LogEvent::SendReady { .. }))
    );
    client
        .complete_read(read.complete(Err(IoError {
            kind: IoErrorKind::Cancelled,
            code: None,
        })))
        .unwrap();
    assert!(
        matches!(client.next_at(Tick(150), &mut ports).unwrap(), Some(Event::Finished(f)) if f.result == Err(Failure::Timeout))
    );
}

#[test]
fn zero_continue_timeout_is_a_fallback_not_a_connection_failure() {
    let mut client = client(Config {
        continue_timeout_ns: Some(0),
        ..config()
    });
    client
        .request(get(
            "POST",
            BodyLength::Known(3),
            true,
            &[Header {
                name: "host",
                value: b"a",
            }],
        ))
        .unwrap();
    let mut ports = Ports::default();
    let Some(Event::Write(op)) = client.next_at(Tick(0), &mut ports).unwrap() else {
        panic!()
    };
    client.complete_write(finish_write(op)).unwrap();
    assert!(matches!(
        client.next_at(Tick(0), &mut ports).unwrap(),
        Some(Event::Demand(_, 3))
    ));
}

#[test]
fn due_upload_preserves_owned_operations_and_accounts_late_positive_write_progress() {
    let mut client = client(Config {
        body_timeout_ns: Some(10),
        ..config()
    });
    let id = client
        .request(get(
            "POST",
            BodyLength::Known(3),
            false,
            &[Header {
                name: "host",
                value: b"a",
            }],
        ))
        .unwrap();
    let mut ports = Ports::default();
    let Some(Event::Write(head)) = client.next_at(Tick(0), &mut ports).unwrap() else {
        panic!()
    };
    client.complete_write(finish_write(head)).unwrap();
    assert!(matches!(
        client.next_at(Tick(0), &mut ports).unwrap(),
        Some(Event::Demand(_, _))
    ));
    let body = b"abc".to_vec();
    let pointer = body.as_ptr();
    client
        .send_body(SendBody {
            exchange: id,
            buffer: body,
            range: 0..3,
            end: true,
        })
        .unwrap();
    let Some(Event::Write(write)) = client.next_at(Tick(0), &mut ports).unwrap() else {
        panic!()
    };
    let Some(Event::Read(read)) = client.next_at(Tick(0), &mut ports).unwrap() else {
        panic!()
    };
    let mut cancelled = Vec::new();
    while let Some(event) = client.next_at(Tick(10), &mut ports).unwrap() {
        match event {
            Event::Cancel(c) => cancelled.push(c.target),
            Event::Finished(f) => assert_eq!(f.result, Err(Failure::Timeout)),
            other => panic!("closed before I/O settled: {other:?}"),
        }
    }
    assert!(cancelled.contains(&read.id()) && cancelled.contains(&write.id()));
    client.complete_write(write.complete(Ok(1))).unwrap();
    client
        .complete_read(read.complete(Err(IoError {
            kind: IoErrorKind::Cancelled,
            code: None,
        })))
        .unwrap();
    let Some(Event::Sent(receipt)) = client.next_at(Tick(10), &mut ports).unwrap() else {
        panic!()
    };
    assert_eq!(receipt.buffer.as_ptr(), pointer);
    assert_eq!(receipt.accepted, 1);
    assert_eq!(receipt.acceptance, Acceptance::Exact);
    assert_eq!(receipt.result, Err(Failure::Timeout));
    assert!(matches!(
        client.next_at(Tick(10), &mut ports).unwrap(),
        Some(Event::Close(_))
    ));
}

#[test]
fn zero_body_and_error_flush_timeouts_terminate_without_delivery_or_error_write() {
    for malformed in [false, true] {
        let mut server = server(Config {
            body_timeout_ns: Some(0),
            ..config()
        });
        let mut ports = Ports::default();
        let Some(Event::Read(read)) = server.next_at(Tick(0), &mut ports).unwrap() else {
            panic!()
        };
        let wire = if malformed {
            b"GET / HTTP/1.1\n\n".as_slice()
        } else {
            b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 3\r\n\r\nabc"
        };
        server.complete_read(fill(read, wire)).unwrap();
        if !malformed {
            let Some(Event::Request(id, _)) = server.next_at(Tick(0), &mut ports).unwrap() else {
                panic!()
            };
            server.grant_body_credit(id, 3).unwrap();
        }
        let expected = if malformed {
            Failure::Protocol
        } else {
            Failure::Timeout
        };
        let close = loop {
            match server.next_at(Tick(0), &mut ports).unwrap().unwrap() {
                Event::Finished(f) => assert_eq!(f.result, Err(expected)),
                Event::Close(close) => break close,
                event => panic!("work issued after zero timeout: {event:?}"),
            }
        };
        server.complete_close(close.complete(Ok(()))).unwrap();
        assert!(
            matches!(server.next_at(Tick(0), &mut ports).unwrap(), Some(Event::Closed(Err(f))) if f == expected)
        );
    }
}

#[test]
fn caller_can_choose_progress_first_or_expiry_first_for_a_late_body_read() {
    for expire_first in [false, true] {
        let mut server = server(Config {
            body_timeout_ns: Some(10),
            ..config()
        });
        let mut ports = Ports::default();
        let Some(Event::Read(head)) = server.next_at(Tick(0), &mut ports).unwrap() else {
            panic!()
        };
        server
            .complete_read(fill(
                head,
                b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 3\r\n\r\n",
            ))
            .unwrap();
        let Some(Event::Request(id, _)) = server.next_at(Tick(0), &mut ports).unwrap() else {
            panic!()
        };
        server.grant_body_credit(id, 3).unwrap();
        let Some(Event::Read(body)) = server.next_at(Tick(0), &mut ports).unwrap() else {
            panic!()
        };
        if expire_first {
            server.advance_time(Tick(15)).unwrap();
        } else {
            server.observe_time(Tick(15)).unwrap();
        }
        server.complete_read(fill(body, b"abc")).unwrap();
        let event = server.next_at(Tick(15), &mut ports).unwrap().unwrap();
        if expire_first {
            let Event::Write(error) = event else {
                panic!("late input revived a timed-out exchange")
            };
            assert!(error.slices().concat().starts_with(b"HTTP/1.1 408 "));
        } else {
            let Event::Body(body) = event else {
                panic!("progress did not refresh the body deadline")
            };
            assert_eq!(body.bytes(), b"abc");
            assert_eq!(ports.deadlines.last().unwrap().unwrap().at, Tick(25));
            server.release_body(body.release(3)).unwrap();
        }
    }
}

#[test]
fn idle_expiry_can_precede_request_admission() {
    let cfg = Config {
        idle_timeout_ns: Some(5),
        head_timeout_ns: Some(10),
        ..config()
    };
    let mut expired = client(cfg.clone());
    expired.advance_time(Tick(5)).unwrap();
    assert_eq!(
        expired.request(get(
            "GET",
            BodyLength::Empty,
            false,
            &[Header {
                name: "host",
                value: b"a"
            }]
        )),
        Err(CommandError::InvalidState)
    );
    let mut refreshed = client(cfg);
    refreshed.observe_time(Tick(5)).unwrap();
    refreshed
        .request(get(
            "GET",
            BodyLength::Empty,
            false,
            &[Header {
                name: "host",
                value: b"a",
            }],
        ))
        .unwrap();
    assert!(matches!(
        refreshed.next_at(Tick(5), &mut Ports::default()).unwrap(),
        Some(Event::Write(_))
    ));
}

#[test]
fn handoff_revokes_clocked_http_authority() {
    let mut client = client(Config {
        head_timeout_ns: Some(10),
        ..config()
    });
    client
        .request(get(
            "CONNECT",
            BodyLength::Empty,
            false,
            &[Header {
                name: "host",
                value: b"a",
            }],
        ))
        .unwrap();
    let mut ports = Ports::default();
    let Some(Event::Write(head)) = client.next_at(Tick(0), &mut ports).unwrap() else {
        panic!()
    };
    client.complete_write(finish_write(head)).unwrap();
    let Some(Event::Read(read)) = client.next_at(Tick(0), &mut ports).unwrap() else {
        panic!()
    };
    client
        .complete_read(fill(read, b"HTTP/1.1 200 Connected\r\n\r\nnext-protocol"))
        .unwrap();
    loop {
        match client.next_at(Tick(1), &mut ports).unwrap().unwrap() {
            Event::Response(_, 200, false) | Event::Incoming(_) => {}
            Event::Upgrade(_) => break,
            other => panic!("{other:?}"),
        }
    }
    let handoff = client.take_upgrade().unwrap();
    assert_eq!(handoff.buffered.bytes(), b"next-protocol");
    assert!(client.next_at(Tick(100), &mut ports).unwrap().is_none());
}
