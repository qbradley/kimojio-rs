mod support;
use kimojio_fsm_http1::*;
use support::*;

#[cfg(feature = "metrics")]
#[test]
fn counters_follow_acceptance_consumption_and_uncertain_write_progress() {
    let mut machine = server(config());
    let Some(Event::Read(mut read)) = next_server(&mut machine) else {
        panic!()
    };
    let before = machine.metrics();
    let invalid = read.bytes_mut().len() + 1;
    let rejected = machine
        .complete_read(read.complete(Ok(invalid)))
        .unwrap_err();
    assert_eq!(rejected.reason, RejectReason::InvalidCount);
    assert_eq!(machine.metrics(), before);
    let (read, _) = rejected.value.into_parts();
    let request = b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 3\r\n\r\nabc";
    machine.complete_read(fill(read, request)).unwrap();
    let Some(Event::Request(exchange, _)) = next_server(&mut machine) else {
        panic!()
    };
    assert_eq!(machine.metrics().counters.read_bytes, request.len() as u64);
    assert_eq!(machine.metrics().buffered_input_bytes, 3);
    assert_eq!(machine.metrics().counters.exchanges_started, 1);
    for (offered, consumed) in [(3, 1), (2, 0), (2, 2)] {
        machine.grant_body_credit(exchange, offered).unwrap();
        let Some(Event::Body(body)) = next_server(&mut machine) else {
            panic!()
        };
        assert_eq!(body.bytes().len(), offered);
        assert!(machine.metrics().body_lease_outstanding);
        machine.release_body(body.release(consumed)).unwrap();
        assert!(!machine.metrics().body_lease_outstanding);
    }
    assert!(matches!(
        next_server(&mut machine),
        Some(Event::Incoming(_))
    ));
    let metrics = machine.metrics();
    assert_eq!(metrics.counters.body_deliveries, 3);
    assert_eq!(metrics.counters.body_bytes_delivered, 7);
    assert_eq!(metrics.counters.body_bytes_consumed, 3);
    assert_eq!(metrics, machine.metrics());
    machine
        .respond(
            exchange,
            Response::new(200, "OK", &[], BodyLength::Known(2)),
        )
        .unwrap();
    let Some(Event::Write(write)) = next_server(&mut machine) else {
        panic!()
    };
    let head_bytes = write
        .slices()
        .iter()
        .map(|slice| slice.len())
        .sum::<usize>();
    machine.complete_write(write.complete(Ok(5))).unwrap();
    let Some(Event::Write(write)) = next_server(&mut machine) else {
        panic!()
    };
    machine.complete_write(finish_write(write)).unwrap();
    assert!(matches!(
        next_server(&mut machine),
        Some(Event::Demand(_, 2))
    ));
    machine
        .send_body(SendBody {
            exchange,
            buffer: b"xy".to_vec(),
            range: 0..2,
            end: true,
        })
        .unwrap();
    let Some(Event::Write(write)) = next_server(&mut machine) else {
        panic!()
    };
    let error = IoError {
        kind: IoErrorKind::UnknownProgress,
        code: Some(5),
    };
    machine.complete_write(write.complete(Err(error))).unwrap();
    assert!(matches!(next_server(&mut machine), Some(Event::Sent(_))));
    let Some(Event::Finished(finished)) = next_server(&mut machine) else {
        panic!()
    };
    assert_eq!(finished.result, Err(Failure::Transport(error)));
    let Some(Event::Close(close)) = next_server(&mut machine) else {
        panic!()
    };
    machine.complete_close(close.complete(Ok(()))).unwrap();
    assert!(matches!(
        next_server(&mut machine),
        Some(Event::Closed(Err(_)))
    ));
    let final_metrics = machine.metrics();
    assert_eq!(final_metrics.phase, SnapshotPhase::Closed);
    assert_eq!(final_metrics.counters.read_completions, 1);
    assert_eq!(final_metrics.counters.write_completions, 3);
    assert_eq!(
        final_metrics.counters.written_bytes_lower_bound,
        head_bytes as u64
    );
    assert_eq!(final_metrics.counters.uncertain_write_completions, 1);
    assert_eq!(final_metrics.counters.producer_bytes_accepted, 2);
    assert_eq!(final_metrics.counters.exchanges_retired, 1);
    assert_eq!(final_metrics.counters.exchanges_failed, 1);
    assert!(!final_metrics.counters.saturated);
}

#[cfg(feature = "metrics")]
#[test]
fn expiry_and_cancellation_count_once_and_originals_still_settle() {
    let mut machine = server(Config {
        head_timeout_ns: Some(100),
        ..config()
    });
    let Some(Event::Deadline(Some(deadline))) = machine.next(&mut Capture) else {
        panic!()
    };
    let Some(Event::Read(read)) = next_server(&mut machine) else {
        panic!()
    };
    machine.expire(deadline, Tick(100)).unwrap();
    let before = machine.metrics();
    assert_eq!(before.counters.deadline_expirations, 1);
    assert_eq!(
        machine.expire(deadline, Tick(200)),
        Err(CommandError::StaleDeadline)
    );
    assert_eq!(machine.metrics(), before);
    assert!(matches!(next_server(&mut machine), Some(Event::Cancel(_))));
    assert!(machine.next(&mut Capture).is_none());
    assert_eq!(machine.metrics().counters.cancellation_requests, 1);
    assert!(machine.metrics().read_outstanding);
    machine
        .complete_read(read.complete(Err(IoError {
            kind: IoErrorKind::Cancelled,
            code: None,
        })))
        .unwrap();
    assert_eq!(machine.metrics().counters.read_completions, 1);
    assert!(!machine.metrics().read_outstanding);
    let Some(Event::Close(close)) = next_server(&mut machine) else {
        panic!()
    };
    machine.complete_close(close.complete(Ok(()))).unwrap();
    assert!(matches!(
        next_server(&mut machine),
        Some(Event::Closed(Err(Failure::Timeout)))
    ));
}

#[derive(Default)]
struct Recorder {
    logs: Vec<LogEvent>,
}

impl Recorder {
    fn expect(&self, event: LogEvent) {
        assert_eq!(self.logs.last(), Some(&event));
    }
}

impl Ports<B> for Recorder {
    type Output = Event;
    fn log(&mut self, connection: ConnectionId, _: Tick, event: LogEvent) {
        assert_eq!(
            connection,
            ConnectionId {
                slot: 2,
                generation: 1
            }
        );
        self.logs.push(event);
    }
    fn read(&mut self, op: ReadOp<B>) -> Option<Event> {
        self.expect(LogEvent::OperationIssued(op.id()));
        Some(Event::Read(op))
    }
    fn write(&mut self, op: WriteOp<B>) -> Option<Event> {
        self.expect(LogEvent::OperationIssued(op.id()));
        Some(Event::Write(op))
    }
    fn readiness(&mut self, op: ReadinessOp) -> Option<Event> {
        self.expect(LogEvent::OperationIssued(op.id()));
        Some(Event::Ready(op))
    }
    fn cancel(&mut self, op: CancelOp) -> Option<Event> {
        self.expect(LogEvent::CancellationRequested(op.target));
        Some(Event::Cancel(op))
    }
    fn close(&mut self, op: CloseOp) -> Option<Event> {
        self.expect(LogEvent::OperationIssued(op.id()));
        Some(Event::Close(op))
    }
    fn body(&mut self, op: BodyOp<B>) -> Option<Event> {
        self.expect(LogEvent::BodyOffered {
            exchange: op.exchange(),
            operation: op.id(),
            bytes: op.bytes().len(),
        });
        Some(Event::Body(op))
    }
    fn trailers(&mut self, exchange: ExchangeId, fields: Headers<'_>) -> Option<Event> {
        self.expect(LogEvent::TrailersReceived {
            exchange,
            fields: fields.len(),
        });
        Capture.trailers(exchange, fields)
    }
    fn incoming_finished(&mut self, exchange: ExchangeId) -> Option<Event> {
        self.expect(LogEvent::IncomingFinished(exchange));
        Some(Event::Incoming(exchange))
    }
    fn source_finished(&mut self, exchange: ExchangeId) -> Option<Event> {
        self.expect(LogEvent::SourceFinished(exchange));
        None
    }
    fn send_ready(&mut self, exchange: ExchangeId, capacity: usize) -> Option<Event> {
        self.expect(LogEvent::SendReady { exchange, capacity });
        Some(Event::Demand(exchange, capacity))
    }
    fn body_sent(&mut self, result: BodySent<B>) -> Option<Event> {
        self.expect(LogEvent::BodyReturned {
            exchange: result.exchange,
            body: result.id,
            accepted: result.accepted,
            acceptance: result.acceptance,
            result: result.result,
        });
        Some(Event::Sent(result))
    }
    fn exchange_finished(&mut self, result: ExchangeFinished) -> Option<Event> {
        self.expect(LogEvent::ExchangeFinished(result));
        Some(Event::Finished(result))
    }
    fn deadline_changed(&mut self, deadline: Option<Deadline>) -> Option<Event> {
        self.expect(LogEvent::DeadlineChanged(deadline));
        Some(Event::Deadline(deadline))
    }
    fn upgrade_ready(&mut self, exchange: ExchangeId) -> Option<Event> {
        self.expect(LogEvent::UpgradeReady(exchange));
        Some(Event::Upgrade(exchange))
    }
    fn closed(&mut self, result: ConnectionResult) -> Option<Event> {
        self.expect(LogEvent::Closed(result));
        Some(Event::Closed(result))
    }
}

impl ServerPorts<B> for Recorder {
    fn request(&mut self, exchange: ExchangeId, head: RequestHead<'_>) -> Option<Event> {
        self.expect(LogEvent::RequestReceived {
            exchange,
            version: head.version,
        });
        Some(Event::Request(exchange, head.version))
    }
}

impl ClientPorts<B> for Recorder {
    fn response(
        &mut self,
        exchange: ExchangeId,
        head: ResponseHead<'_>,
        informational: bool,
    ) -> Option<Event> {
        self.expect(LogEvent::ResponseReceived {
            exchange,
            status: head.status,
            informational,
        });
        Some(Event::Response(exchange, head.status, informational))
    }
}

#[test]
fn client_logs_informational_final_and_readiness_boundaries() {
    let mut machine = Client::new(
        ConnectionId {
            slot: 2,
            generation: 1,
        },
        config(),
        vec![0; 1024],
        Tick(0),
    )
    .unwrap();
    let mut ports = Recorder::default();
    let exchange = machine
        .request(get(
            "GET",
            BodyLength::Empty,
            false,
            &[Header {
                name: "Host",
                value: b"a",
            }],
        ))
        .unwrap();
    let Some(Event::Write(write)) = machine.next(&mut ports) else {
        panic!()
    };
    machine.complete_write(finish_write(write)).unwrap();
    let Some(Event::Read(read)) = machine.next(&mut ports) else {
        panic!()
    };
    machine
        .complete_read(read.complete(Err(IoError {
            kind: IoErrorKind::WouldBlock,
            code: None,
        })))
        .unwrap();
    let Some(Event::Ready(ready)) = machine.next(&mut ports) else {
        panic!()
    };
    machine.complete_readiness(ready.complete(Ok(()))).unwrap();
    let Some(Event::Read(read)) = machine.next(&mut ports) else {
        panic!()
    };
    machine
        .complete_read(fill(
            read,
            b"HTTP/1.1 103 Early Hints\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
        ))
        .unwrap();
    assert!(
        matches!(machine.next(&mut ports), Some(Event::Response(id, 103, true)) if id == exchange)
    );
    assert!(
        matches!(machine.next(&mut ports), Some(Event::Response(id, 200, false)) if id == exchange)
    );
    assert!(matches!(machine.next(&mut ports), Some(Event::Incoming(id)) if id == exchange));
    assert!(matches!(machine.next(&mut ports), Some(Event::Finished(_))));
    machine.shutdown(ShutdownMode::Graceful);
    let Some(Event::Close(close)) = machine.next(&mut ports) else {
        panic!()
    };
    let failure = IoError {
        kind: IoErrorKind::Other,
        code: Some(5),
    };
    machine
        .complete_close(close.complete(Err(failure)))
        .unwrap();
    assert!(
        matches!(machine.next(&mut ports), Some(Event::Closed(Err(Failure::Transport(error)))) if error == failure)
    );
    assert_eq!(
        ports
            .logs
            .iter()
            .filter(|event| matches!(event, LogEvent::PrimaryFailure(_)))
            .count(),
        1
    );
    assert_eq!(
        ports.logs[ports.logs.len() - 2],
        LogEvent::PrimaryFailure(Failure::Transport(failure))
    );
}

#[test]
fn diagnostics_are_drive_boundaries_not_an_api_attempt_queue() {
    let mut machine = server(Config {
        head_timeout_ns: Some(100),
        ..config()
    });
    let mut ports = Recorder::default();
    machine.observe_time(Tick(7)).unwrap();
    machine.shutdown(ShutdownMode::Abort);
    machine.shutdown(ShutdownMode::Abort);
    assert!(ports.logs.is_empty());
    assert!(matches!(
        machine.next(&mut ports),
        Some(Event::Deadline(None))
    ));
    let Some(Event::Close(close)) = machine.next(&mut ports) else {
        panic!()
    };
    let id = close.id();
    machine.complete_close(close.complete(Ok(()))).unwrap();
    assert!(matches!(
        machine.next(&mut ports),
        Some(Event::Closed(Err(Failure::Cancelled)))
    ));
    assert!(machine.next(&mut ports).is_none());
    assert_eq!(
        ports.logs,
        [
            LogEvent::PrimaryFailure(Failure::Cancelled),
            LogEvent::DeadlineChanged(None),
            LogEvent::OperationIssued(id),
            LogEvent::Closed(Err(Failure::Cancelled)),
        ]
    );
}

#[test]
fn logs_precede_matching_callbacks_without_adding_yields() {
    let mut machine = server(config());
    let mut ports = Recorder::default();
    let Some(Event::Read(read)) = machine.next(&mut ports) else {
        panic!()
    };
    machine.complete_read(fill(read, b"POST / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nab\r\n0\r\nx-end: yes\r\n\r\n")).unwrap();
    let Some(Event::Request(exchange, _)) = machine.next(&mut ports) else {
        panic!()
    };
    machine.grant_body_credit(exchange, 2).unwrap();
    let Some(Event::Body(body)) = machine.next(&mut ports) else {
        panic!()
    };
    machine.release_body(body.release(2)).unwrap();
    assert!(matches!(machine.next(&mut ports), Some(Event::Trailers(_))));
    assert!(matches!(machine.next(&mut ports), Some(Event::Incoming(_))));
    machine
        .respond(
            exchange,
            Response::new(200, "OK", &[], BodyLength::Known(2)),
        )
        .unwrap();
    let Some(Event::Write(write)) = machine.next(&mut ports) else {
        panic!()
    };
    machine.complete_write(finish_write(write)).unwrap();
    assert!(matches!(
        machine.next(&mut ports),
        Some(Event::Demand(_, 2))
    ));
    machine
        .send_body(SendBody {
            exchange,
            buffer: b"xy".to_vec(),
            range: 0..2,
            end: true,
        })
        .unwrap();
    let Some(Event::Write(write)) = machine.next(&mut ports) else {
        panic!()
    };
    machine.complete_write(finish_write(write)).unwrap();
    assert!(matches!(machine.next(&mut ports), Some(Event::Sent(_))));
    assert!(matches!(machine.next(&mut ports), Some(Event::Finished(_))));
    assert!(
        !ports
            .logs
            .iter()
            .any(|event| matches!(event, LogEvent::PrimaryFailure(_)))
    );
}
