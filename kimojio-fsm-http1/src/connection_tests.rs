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
    assert_eq!(
        server.core.lifecycle,
        Lifecycle::Closed(Notification::Delivered)
    );
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
        assert!(client.core.timers.armed.is_none());
        assert!(client.core.exchange.is_none());
        assert!(client.core.output.is_none());
        assert_eq!(client.core.outgoing_metadata_bytes, 0);
        assert_eq!(client.core.receive.buffer().unwrap().len(), 64);
    }
}

#[test]
fn deadline_selection_pairs_kind_and_generation_across_all_timer_combinations() {
    for phase in [
        None,
        Some((TimerPhase::Head, Tick(10))),
        Some((TimerPhase::Body, Tick(10))),
        Some((TimerPhase::Idle, Tick(10))),
        Some((TimerPhase::Head, Tick(20))),
        Some((TimerPhase::Body, Tick(20))),
        Some((TimerPhase::Idle, Tick(20))),
    ] {
        for continuation in [None, Some(Tick(10)), Some(Tick(20))] {
            for upload in [None, Some(Tick(10)), Some(Tick(20))] {
                let mut core = Core::<Vec<u8>, Vec<u8>>::new(
                    ConnectionId {
                        slot: 1,
                        generation: 1,
                    },
                    configuration(),
                    vec![0; 64],
                    Tick(0),
                    false,
                )
                .unwrap();
                core.timers.upload_at = upload;
                let expected = [
                    phase,
                    continuation.map(|at| (TimerPhase::Continue, at)),
                    upload.map(|at| (TimerPhase::Upload, at)),
                ]
                .into_iter()
                .enumerate()
                .filter_map(|(priority, value)| value.map(|value| (priority, value)))
                .min_by_key(|(priority, (_, at))| (at.0, *priority))
                .map(|(_, value)| value);
                core.update_deadline(phase, continuation).unwrap();
                assert_eq!(
                    core.timers
                        .armed
                        .map(|armed| (armed.kind, armed.deadline.at)),
                    expected
                );
                assert_eq!(core.timers.notification.take(), expected.is_some());
                let sequence = core.sequence;
                let armed = core.timers.armed;
                core.update_deadline(phase, continuation).unwrap();
                assert_eq!(core.sequence, sequence);
                assert_eq!(core.timers.armed, armed);
                assert!(!core.timers.notification.take());
            }
        }
    }
}

#[test]
fn informational_completion_cannot_settle_queued_final_metadata() {
    fn issue(core: &mut Core<Vec<u8>, Vec<u8>>) -> WriteOp<Vec<u8>> {
        let mut op = core.output.take().unwrap();
        op.id = core.operation(OperationKind::Write).unwrap();
        core.write.issue(op.id);
        op
    }

    for automatic_error in [false, true] {
        for informational_in_flight in [false, true] {
            let mut core = Core::new(
                ConnectionId {
                    slot: 1,
                    generation: 1,
                },
                configuration(),
                vec![0; 64],
                Tick(0),
                true,
            )
            .unwrap();
            let mut headers = [httparse::EMPTY_HEADER; 128];
            core.metadata(
                b"GET / HTTP/1.1\r\nHost: a\r\n\r\n",
                &mut headers,
                &mut CloseOnly,
                |_, _, _| None,
            )
            .unwrap();
            let exchange = core.exchange.as_ref().unwrap().id;
            core.inform(exchange, ResponseHead::new(103, "Early Hints", &[]))
                .unwrap();
            let informational = informational_in_flight.then(|| issue(&mut core));
            if automatic_error {
                core.fail(Failure::Protocol);
                assert_eq!(core.lifecycle, Lifecycle::ErrorResponse);
            } else {
                core.respond(
                    exchange,
                    Response::new(200, "OK", &[], BodyLength::Empty),
                    ResponseMode::Conservative,
                )
                .unwrap();
            }
            assert!(matches!(
                core.output.as_ref().unwrap().storage,
                WriteStorage::Head {
                    kind: MetadataKind::FinalHead,
                    ..
                }
            ));
            assert!(core.tx.source_finished());
            assert!(!core.tx.settled());
            if let Some(informational) = informational {
                let count = informational.remaining();
                core.complete_write(informational.complete(Ok(count)))
                    .unwrap();
                assert!(
                    !core.tx.settled(),
                    "finishing informational output cannot finish the queued final head"
                );
            }
            core.assert_invariants();
            let final_head = issue(&mut core);
            let count = final_head.remaining();
            core.complete_write(final_head.complete(Ok(count))).unwrap();
            assert!(core.tx.settled());
            core.assert_invariants();
        }
    }
}
