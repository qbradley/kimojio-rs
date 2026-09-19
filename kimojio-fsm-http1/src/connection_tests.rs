use super::*;

struct CloseOnly;

impl Ports<Vec<u8>> for CloseOnly {
    type Output = CloseOp;
    fn close(&mut self, op: CloseOp) -> Option<CloseOp> {
        Some(op)
    }
    fn read(&mut self, _: ReadOp<Vec<u8>>) -> Option<CloseOp> {
        panic!("generation wrapped")
    }
    fn write(&mut self, _: WriteOp<Vec<u8>>) -> Option<CloseOp> {
        panic!("unexpected write")
    }
    fn readiness(&mut self, _: ReadinessOp) -> Option<CloseOp> {
        panic!("unexpected readiness")
    }
    fn body(&mut self, _: BodyOp<Vec<u8>>) -> Option<CloseOp> {
        panic!("unexpected body")
    }
    fn cancel(&mut self, _: CancelOp) -> Option<CloseOp> {
        panic!("unexpected cancellation")
    }
    fn trailers(&mut self, _: ExchangeId, _: Headers<'_>) -> Option<CloseOp> {
        None
    }
    fn incoming_finished(&mut self, _: ExchangeId) -> Option<CloseOp> {
        None
    }
    fn send_ready(&mut self, _: ExchangeId, _: usize) -> Option<CloseOp> {
        None
    }
    fn body_sent(&mut self, _: BodySent<Vec<u8>>) -> Option<CloseOp> {
        None
    }
    fn exchange_finished(&mut self, _: ExchangeFinished) -> Option<CloseOp> {
        None
    }
    fn deadline_changed(&mut self, _: Option<Deadline>) -> Option<CloseOp> {
        None
    }
    fn upgrade_ready(&mut self, _: ExchangeId) -> Option<CloseOp> {
        None
    }
    fn closed(&mut self, _: ConnectionResult) -> Option<CloseOp> {
        None
    }
}

impl ServerPorts<Vec<u8>> for CloseOnly {
    fn request(&mut self, _: ExchangeId, _: RequestHead<'_>) -> Option<CloseOp> {
        None
    }
}

fn configuration() -> Config {
    Config {
        head_timeout_ns: None,
        body_timeout_ns: None,
        idle_timeout_ns: None,
        continue_timeout_ns: None,
        ..Config::default()
    }
}

fn request() -> Request<'static> {
    Request {
        head: RequestHead {
            method: "GET",
            target: "/",
            version: Version::Http11,
            headers: &[Header {
                name: "host",
                value: b"a",
            }],
        },
        body: BodyLength::Empty,
        expect_continue: false,
    }
}

#[test]
fn generation_exhaustion_uses_reserved_close_without_false_quiescence() {
    let mut server = Server::new(
        ConnectionId {
            slot: 1,
            generation: 7,
        },
        configuration(),
        vec![0; 64],
        Tick(0),
    )
    .unwrap();
    server.core.sequence = u64::MAX - 1;
    let close = server
        .next(&mut CloseOnly)
        .expect("terminal cleanup remains runnable");
    assert_eq!(close.id().sequence(), u64::MAX);
    assert_eq!(close.id().kind(), OperationKind::Close);
    assert_eq!(server.core.failure, Some(Failure::SequenceExhausted));
    server.complete_close(close.complete(Ok(()))).unwrap();
    assert!(server.next(&mut CloseOnly).is_none());
    assert!(server.core.closed_notified);
}

#[test]
fn rejected_request_never_changes_sequences_timers_or_owned_slots() {
    for timer_overflow in [false, true] {
        let mut config = configuration();
        if timer_overflow {
            config.head_timeout_ns = Some(10);
        }
        let mut client = Client::new(
            ConnectionId {
                slot: 1,
                generation: 7,
            },
            config,
            vec![0; 64],
            Tick(0),
        )
        .unwrap();
        if timer_overflow {
            client.observe_time(Tick(u64::MAX - 1)).unwrap();
        } else {
            client.core.sequence = u64::MAX - 2;
        }
        let sequence = client.core.sequence;
        let now = client.core.now;
        let expected = if timer_overflow {
            CommandError::InvalidConfig
        } else {
            CommandError::SequenceExhausted
        };
        assert_eq!(client.request(request()), Err(expected));
        assert_eq!(client.core.sequence, sequence);
        assert_eq!(client.core.now, now);
        assert!(client.core.deadline.is_none());
        assert!(client.core.exchange.is_none());
        assert!(client.core.output.is_none());
        assert_eq!(client.core.outgoing_metadata_bytes, 0);
        assert_eq!(client.core.input.as_ref().unwrap().len(), 64);
    }
}
