#[path = "../examples/support/mod.rs"]
mod support;

use std::{collections::VecDeque, time::Duration};

use kimojio_fsm_http1 as h1;
use kimojio_fsm_http2::{self as h2, http};

#[derive(Default)]
struct Ports {
    h2: support::MemoryPorts,
    read: Option<h1::ReadOp<Vec<u8>>>,
    write: Option<h1::WriteOp<Vec<u8>>>,
    cancel: Vec<h1::CancelOp>,
    close: Option<h1::CloseOp>,
    requests: Vec<(h1::ExchangeId, String, String)>,
    closed: Vec<h1::ConnectionResult>,
    detection_closed: Vec<http::DetectionClosed>,
    h1_trace: Vec<&'static str>,
    yielding: bool,
    admission_changes: usize,
    #[cfg(feature = "http1-diagnostics")]
    logs: Vec<h1::LogEvent>,
}

impl Ports {
    fn output(&self) -> Option<()> {
        self.yielding.then_some(())
    }
    fn h1_event(&mut self, event: &'static str) -> Option<()> {
        self.h1_trace.push(event);
        self.output()
    }
}

impl h1::Ports<Vec<u8>> for Ports {
    type Output = ();
    #[cfg(feature = "http1-diagnostics")]
    fn log(&mut self, connection: h1::ConnectionId, _: h1::Tick, event: h1::LogEvent) {
        if let h1::LogEvent::RequestReceived { exchange, .. } = event {
            assert_eq!(connection, exchange.connection());
        }
        self.logs.push(event);
    }
    fn read(&mut self, op: h1::ReadOp<Vec<u8>>) -> Option<()> {
        assert!(self.read.replace(op).is_none());
        self.h1_event("read")
    }
    fn write(&mut self, op: h1::WriteOp<Vec<u8>>) -> Option<()> {
        assert!(self.write.replace(op).is_none());
        self.h1_event("write")
    }
    fn readiness(&mut self, _: h1::ReadinessOp) -> Option<()> {
        panic!("memory transport never reports would-block")
    }
    fn cancel(&mut self, op: h1::CancelOp) -> Option<()> {
        self.cancel.push(op);
        self.h1_event("cancel")
    }
    fn close(&mut self, op: h1::CloseOp) -> Option<()> {
        assert!(self.close.replace(op).is_none());
        self.h1_event("close")
    }
    fn body(&mut self, _: h1::BodyOp<Vec<u8>>) -> Option<()> {
        panic!("empty request has no body")
    }
    fn trailers(&mut self, _: h1::ExchangeId, _: h1::Headers<'_>) -> Option<()> {
        panic!("empty request has no trailers")
    }
    fn incoming_finished(&mut self, _: h1::ExchangeId) -> Option<()> {
        self.h1_event("incoming_finished")
    }
    fn send_ready(&mut self, _: h1::ExchangeId, _: usize) -> Option<()> {
        panic!("empty response has no producer demand")
    }
    fn source_finished(&mut self, _: h1::ExchangeId) -> Option<()> {
        self.h1_event("source_finished")
    }
    fn body_sent(&mut self, _: h1::BodySent<Vec<u8>>) -> Option<()> {
        panic!("empty response has no body receipt")
    }
    fn exchange_finished(&mut self, _: h1::ExchangeFinished) -> Option<()> {
        self.h1_event("exchange_finished")
    }
    fn deadline_changed(&mut self, _: Option<h1::Deadline>) -> Option<()> {
        self.h1_event("deadline_changed")
    }
    fn upgrade_ready(&mut self, _: h1::ExchangeId) -> Option<()> {
        panic!("no upgrade requested")
    }
    fn closed(&mut self, result: h1::ConnectionResult) -> Option<()> {
        self.closed.push(result);
        self.h1_event("closed")
    }
}

impl h1::ServerPorts<Vec<u8>> for Ports {
    fn request(&mut self, id: h1::ExchangeId, head: h1::RequestHead<'_>) -> Option<()> {
        #[cfg(feature = "http1-diagnostics")]
        assert_eq!(
            self.logs.last(),
            Some(&h1::LogEvent::RequestReceived {
                exchange: id,
                version: head.version
            })
        );
        self.requests
            .push((id, head.method.into(), head.target.into()));
        self.h1_event("request")
    }
}

impl h1::ClientPorts<Vec<u8>> for Ports {
    fn response(&mut self, _: h1::ExchangeId, _: h1::ResponseHead<'_>, _: bool) -> Option<()> {
        self.h1_event("response")
    }
}

impl h2::Ports<Vec<u8>> for Ports {
    type Output = ();
    fn read(&mut self, op: h2::ReadOp) -> Option<()> {
        h2::Ports::read(&mut self.h2, op);
        self.output()
    }
    fn write(&mut self, op: h2::WriteOp) -> Option<()> {
        h2::Ports::write(&mut self.h2, op);
        self.output()
    }
    fn headers(&mut self, head: h2::Head<'_>) -> Option<()> {
        h2::Ports::headers(&mut self.h2, head);
        self.output()
    }
    fn body(&mut self, op: h2::BodyOp) -> Option<()> {
        h2::Ports::body(&mut self.h2, op);
        self.output()
    }
    fn send_ready(&mut self, permit: h2::SendPermit) -> Option<()> {
        h2::Ports::send_ready(&mut self.h2, permit);
        self.output()
    }
    fn admission_changed(&mut self) -> Option<()> {
        self.admission_changes += 1;
        self.output()
    }
    fn send_stopped(&mut self, id: h2::StreamId, reason: h2::SendStop) -> Option<()> {
        h2::Ports::send_stopped(&mut self.h2, id, reason);
        self.output()
    }
    fn sent(&mut self, result: h2::Sent) -> Option<()> {
        h2::Ports::sent(&mut self.h2, result);
        self.output()
    }
    fn ended(&mut self, end: h2::ReceiveEnd) -> Option<()> {
        h2::Ports::ended(&mut self.h2, end);
        self.output()
    }
    fn retired(&mut self, result: h2::StreamResult) -> Option<()> {
        h2::Ports::retired(&mut self.h2, result);
        self.output()
    }
    fn cancel(&mut self, op: h2::CancelOp) -> Option<()> {
        h2::Ports::cancel(&mut self.h2, op);
        self.output()
    }
    fn wake(&mut self, op: h2::WakeOp) -> Option<()> {
        h2::Ports::wake(&mut self.h2, op);
        self.output()
    }
    fn close(&mut self, op: h2::CloseOp) -> Option<()> {
        h2::Ports::close(&mut self.h2, op);
        self.output()
    }
    fn closed(&mut self, result: h2::ConnectionResult) -> Option<()> {
        h2::Ports::closed(&mut self.h2, result);
        self.output()
    }
    fn reschedule(&mut self) -> Option<()> {
        h2::Ports::reschedule(&mut self.h2)
    }
}

impl http::ServerPorts<Vec<u8>> for Ports {
    fn detection_closed(&mut self, result: http::DetectionClosed) -> Option<()> {
        self.detection_closed.push(result);
        self.output()
    }
}

fn h1_server() -> h1::Server<Vec<u8>> {
    h1::Server::with_output_type(
        h1::ConnectionId {
            slot: 1,
            generation: 1,
        },
        h1::Config::default(),
        vec![0; 65536],
        h1::Tick(0),
    )
    .unwrap()
}

fn detecting() -> http::Server {
    http::Server::detect(
        http::DetectionConfig {
            http1_connection: h1::ConnectionId {
                slot: 1,
                generation: 1,
            },
            http1_config: h1::Config::default(),
            http1_buffer: vec![0; 65536],
            http2_config: h2::Config::default(),
            timeout: Duration::from_secs(1),
        },
        Duration::ZERO,
    )
    .unwrap()
}

#[cfg(feature = "http1-metrics")]
#[test]
fn snapshots_require_a_selected_http1_child_and_do_not_drive_it() {
    assert_eq!(detecting().http1_metrics(), None);
    let mut server = http::Server::http1(h1_server());
    let before = server.http1_mut().unwrap().metrics();
    assert_eq!(server.http1_metrics(), Some(before));
    assert_eq!(server.http1_mut().unwrap().metrics(), before);
    let server = http::Server::<Vec<u8>>::http2(
        h2::Server::new(h2::Config::default(), Duration::ZERO).unwrap(),
    );
    assert_eq!(server.http1_metrics(), None);
}

fn drive(server: &mut http::Server, ports: &mut Ports) {
    for _ in 0..100 {
        if server.next(ports).is_none() {
            return;
        }
    }
    panic!("bounded drive did not suspend");
}

fn drive_client(client: &mut http::Client, ports: &mut Ports) {
    for _ in 0..100 {
        if client.next(ports).is_none() {
            return;
        }
    }
    panic!("bounded client drive did not suspend");
}

#[test]
fn selected_client_forwards_metadata_readiness_before_original_write_settlement() {
    for yielding in [false, true] {
        let config = h2::Config {
            max_outbound_items: 4,
            ..h2::Config::default()
        };
        let mut client = http::Client::http2(h2::Client::new(config, Duration::ZERO).unwrap());
        let mut ports = Ports {
            yielding,
            ..Ports::default()
        };
        let fields = support::request(b"GET");
        for expected in [1, 3, 5] {
            assert_eq!(
                client
                    .http2_mut()
                    .unwrap()
                    .request(&fields, true)
                    .unwrap()
                    .get(),
                expected
            );
        }
        assert_eq!(
            client.http2_mut().unwrap().request(&fields, true),
            Err(h2::CommandError::Blocked)
        );
        assert_eq!(ports.admission_changes, 0);
        drive_client(&mut client, &mut ports);
        assert!(ports.h2.write.is_some());
        assert_eq!(ports.admission_changes, 1);
        assert_eq!(
            client
                .http2_mut()
                .unwrap()
                .request(&fields, true)
                .unwrap()
                .get(),
            7
        );
        for _ in 0..100 {
            drive_client(&mut client, &mut ports);
        }
        assert_eq!(ports.admission_changes, 1);
        assert!(ports.h2.write.is_some());
        assert!(ports.h1_trace.is_empty());
    }
}

fn settle_detection_cancels(server: &mut http::Server, ports: &mut Ports) {
    for op in std::mem::take(&mut ports.h2.cancels) {
        if let Some(index) = ports
            .h2
            .alarms
            .iter()
            .position(|alarm| alarm.token() == op.original())
        {
            let alarm = ports.h2.alarms.remove(index);
            server
                .complete_wake(alarm.complete(Duration::ZERO))
                .unwrap();
        } else if ports
            .h2
            .read
            .as_ref()
            .is_some_and(|read| read.token() == op.original())
        {
            let read = ports.h2.read.take().unwrap();
            server
                .complete_read(read.complete(h2::ReadOutcome::Failed(h2::IoFailure::Cancelled)))
                .unwrap();
        } else {
            panic!("unknown original operation");
        }
        server.complete_cancel(op.complete()).unwrap();
    }
}

#[test]
fn http1_prefetch_is_lossless_and_keeps_the_connection_reusable() {
    for yielding in [false, true] {
        for fragment in [1, 7, 24, 65536] {
            let mut baseline = None;
            for auto in [false, true] {
                let mut server = if auto {
                    detecting()
                } else {
                    http::Server::http1(h1_server())
                };
                let mut ports = Ports {
                    yielding,
                    ..Ports::default()
                };
                let request = b"GET /first HTTP/1.1\r\nHost: example\r\n\r\n";
                let second = b"HEAD /second HTTP/1.1\r\nHost: example\r\n\r\n";
                let mut input: VecDeque<u8> = request.iter().chain(second).copied().collect();
                let mut output = Vec::new();
                let mut responded = 0;
                for _ in 0..10000 {
                    drive(&mut server, &mut ports);
                    settle_detection_cancels(&mut server, &mut ports);
                    if !input.is_empty() {
                        if let Some(mut op) = ports.h2.read.take() {
                            let n = op.buffer_mut().len().min(fragment).min(input.len());
                            for byte in &mut op.buffer_mut()[..n] {
                                *byte = input.pop_front().unwrap();
                            }
                            server
                                .complete_read(op.complete(h2::ReadOutcome::Read(n)))
                                .unwrap();
                        } else if let Some(mut op) = ports.read.take() {
                            let n = op.bytes_mut().len().min(fragment).min(input.len());
                            for byte in &mut op.bytes_mut()[..n] {
                                *byte = input.pop_front().unwrap();
                            }
                            server
                                .http1_mut()
                                .unwrap()
                                .complete_read(op.complete(Ok(n)))
                                .unwrap();
                        }
                    }
                    if let Some(op) = ports.write.take() {
                        let bytes: Vec<u8> = op
                            .slices()
                            .into_iter()
                            .flatten()
                            .copied()
                            .take(fragment)
                            .collect();
                        output.extend_from_slice(&bytes);
                        server
                            .http1_mut()
                            .unwrap()
                            .complete_write(op.complete(Ok(bytes.len())))
                            .unwrap();
                    }
                    while responded < ports.requests.len() {
                        let id = ports.requests[responded].0;
                        server
                            .http1_mut()
                            .unwrap()
                            .respond(id, h1::Response::new(200, "OK", &[], h1::BodyLength::Empty))
                            .unwrap();
                        responded += 1;
                    }
                    if ports
                        .h1_trace
                        .iter()
                        .filter(|event| **event == "exchange_finished")
                        .count()
                        == 2
                    {
                        break;
                    }
                }
                assert_eq!(server.protocol(), Some(http::Protocol::Http1));
                assert!(input.is_empty());
                assert_eq!(
                    ports
                        .requests
                        .iter()
                        .map(|(_, method, path)| (method.as_str(), path.as_str()))
                        .collect::<Vec<_>>(),
                    [("GET", "/first"), ("HEAD", "/second")]
                );
                assert_eq!(
                    ports
                        .h1_trace
                        .iter()
                        .filter(|event| **event == "exchange_finished")
                        .count(),
                    2
                );
                assert_eq!(
                    String::from_utf8(output.clone())
                        .unwrap()
                        .matches("HTTP/1.1 200 OK\r\n")
                        .count(),
                    2
                );
                assert!(ports.h2.heads.is_empty());
                assert!(ports.h2.write.is_none());
                assert!(ports.detection_closed.is_empty());
                #[cfg(feature = "http1-metrics")]
                {
                    let metrics = server.http1_metrics().unwrap();
                    assert_eq!(metrics.counters.exchanges_started, 2);
                    assert_eq!(metrics.counters.exchanges_retired, 2);
                    assert_eq!(
                        metrics.counters.read_bytes,
                        (request.len() + second.len()) as u64
                    );
                    assert_eq!(
                        metrics.counters.written_bytes_lower_bound,
                        output.len() as u64
                    );
                    assert_eq!(metrics.counters.exchanges_failed, 0);
                }
                #[cfg(feature = "http1-diagnostics")]
                assert_eq!(
                    ports
                        .logs
                        .iter()
                        .filter(|event| matches!(event, h1::LogEvent::RequestReceived { .. }))
                        .count(),
                    2
                );
                let observations = (
                    output,
                    ports
                        .h1_trace
                        .into_iter()
                        .filter(|event| *event != "read")
                        .collect::<Vec<_>>(),
                );
                if let Some(baseline) = &baseline {
                    assert_eq!(&observations, baseline);
                } else {
                    baseline = Some(observations);
                }
            }
        }
    }
}

#[test]
fn http2_preface_selects_the_binary_child_and_preserves_exact_output() {
    for yielding in [false, true] {
        for fragment in [1, 7, 16384] {
            let mut baseline = None;
            for auto in [false, true] {
                let mut server = if auto {
                    detecting()
                } else {
                    http::Server::http2(
                        h2::Server::new(h2::Config::default(), Duration::ZERO).unwrap(),
                    )
                };
                let mut client = h2::Client::new(h2::Config::default(), Duration::ZERO).unwrap();
                let mut client_ports = support::MemoryPorts::default();
                let mut ports = Ports {
                    yielding,
                    ..Ports::default()
                };
                let mut to_client = VecDeque::new();
                let mut to_server = VecDeque::new();
                let mut wire = Vec::new();
                let mut responded = 0;
                client.request(&support::request(b"GET"), true).unwrap();
                client.request(&support::request(b"GET"), true).unwrap();
                for _ in 0..10000 {
                    support::step(
                        &mut client,
                        &mut client_ports,
                        &mut to_client,
                        &mut to_server,
                        fragment,
                    );
                    drive(&mut server, &mut ports);
                    settle_detection_cancels(&mut server, &mut ports);
                    if !to_server.is_empty()
                        && let Some(mut op) = ports.h2.read.take()
                    {
                        let n = op.buffer_mut().len().min(fragment).min(to_server.len());
                        for byte in &mut op.buffer_mut()[..n] {
                            *byte = to_server.pop_front().unwrap();
                        }
                        server
                            .complete_read(op.complete(h2::ReadOutcome::Read(n)))
                            .unwrap();
                    }
                    if let Some(op) = ports.h2.write.take() {
                        let bytes: Vec<u8> = op
                            .slices()
                            .iter()
                            .flat_map(|slice| slice.iter().copied())
                            .take(fragment)
                            .collect();
                        wire.extend_from_slice(&bytes);
                        to_client.extend(bytes.iter().copied());
                        server
                            .http2_mut()
                            .unwrap()
                            .complete_write(op.complete(h2::WriteOutcome::Written(bytes.len())))
                            .unwrap();
                    }
                    while responded < ports.h2.heads.len() {
                        let (stream, kind, _, end) = ports.h2.heads[responded];
                        assert_eq!(kind, h2::HeadKind::Request);
                        assert!(end);
                        server
                            .http2_mut()
                            .unwrap()
                            .respond(stream, &support::response(b"200"), true)
                            .unwrap();
                        responded += 1;
                    }
                    if client_ports.retired.len() == 2 && ports.h2.retired.len() == 2 {
                        break;
                    }
                }
                assert_eq!(server.protocol(), Some(http::Protocol::Http2));
                assert_eq!(responded, 2);
                assert_eq!(client_ports.retired.len(), 2);
                assert_eq!(ports.h2.retired.len(), 2);
                assert!(
                    client_ports
                        .retired
                        .iter()
                        .all(|end| end.outcome == h2::StreamOutcome::Complete)
                );
                assert!(
                    ports
                        .h2
                        .retired
                        .iter()
                        .all(|end| end.outcome == h2::StreamOutcome::Complete)
                );
                assert_eq!(
                    client_ports
                        .heads
                        .iter()
                        .map(|(_, kind, _, end)| (*kind, *end))
                        .collect::<Vec<_>>(),
                    [(h2::HeadKind::Response(200), true); 2]
                );
                assert!(ports.requests.is_empty());
                assert!(ports.h1_trace.is_empty());
                assert!(ports.detection_closed.is_empty());
                if let Some(baseline) = &baseline {
                    assert_eq!(&wire, baseline);
                } else {
                    baseline = Some(wire);
                }
            }
        }
    }
}

#[test]
fn eof_and_abort_never_start_a_protocol_child() {
    for yielding in [false, true] {
        for prefix in [b"".as_slice(), b"PRI * HTTP/2.0\r\n"] {
            let mut server = detecting();
            let mut ports = Ports {
                yielding,
                ..Ports::default()
            };
            drive(&mut server, &mut ports);
            if !prefix.is_empty() {
                let mut read = ports.h2.read.take().unwrap();
                read.buffer_mut()[..prefix.len()].copy_from_slice(prefix);
                server
                    .complete_read(read.complete(h2::ReadOutcome::Read(prefix.len())))
                    .unwrap();
                drive(&mut server, &mut ports);
            }
            let read = ports.h2.read.take().unwrap();
            server
                .complete_read(read.complete(h2::ReadOutcome::Eof))
                .unwrap();
            drive(&mut server, &mut ports);
            settle_detection_cancels(&mut server, &mut ports);
            drive(&mut server, &mut ports);
            let close = ports.h2.close.take().unwrap();
            server.complete_close(close.complete(Ok(()))).unwrap();
            drive(&mut server, &mut ports);
            assert_eq!(
                ports.detection_closed,
                [http::DetectionClosed {
                    failure: if prefix.is_empty() {
                        http::DetectionFailure::EndOfInput
                    } else {
                        http::DetectionFailure::TruncatedInput
                    },
                    close_result: Ok(()),
                }]
            );
            assert_eq!(server.protocol(), None);
            assert!(ports.h1_trace.is_empty());
            assert!(ports.h2.write.is_none());
        }
        let mut server = detecting();
        let mut ports = Ports {
            yielding,
            ..Ports::default()
        };
        server.shutdown().unwrap();
        drive(&mut server, &mut ports);
        assert!(ports.h2.read.is_none());
        assert!(ports.h2.alarms.is_empty());
        assert!(ports.h2.cancels.is_empty());
        let close = ports.h2.close.take().unwrap();
        server.complete_close(close.complete(Ok(()))).unwrap();
        drive(&mut server, &mut ports);
        assert_eq!(
            ports.detection_closed[0].failure,
            http::DetectionFailure::Aborted
        );
    }
}

#[test]
fn invalidated_alarm_cannot_reverse_protocol_selection() {
    let mut server = detecting();
    let mut ports = Ports::default();
    drive(&mut server, &mut ports);
    let alarm = ports.h2.alarms.pop().unwrap();
    let rejected = server
        .complete_wake(alarm.complete(Duration::ZERO))
        .unwrap_err();
    // Retain the rejected completion across invalidation of its original alarm.
    let invalid_time = Duration::from_secs(u64::MAX);
    assert_eq!(
        server.advance_time(invalid_time),
        Err(http::Error::TimeRange)
    );
    let mut read = ports.h2.read.take().unwrap();
    read.buffer_mut()[0] = b'G';
    server
        .complete_read(read.complete(h2::ReadOutcome::Read(1)))
        .unwrap();
    drive(&mut server, &mut ports);
    assert_eq!(ports.h2.cancels.len(), 1);
    server.advance_time(Duration::from_secs(2)).unwrap();
    server.complete_wake(rejected.value).unwrap();
    for cancel in ports.h2.cancels.drain(..) {
        server.complete_cancel(cancel.complete()).unwrap();
    }
    drive(&mut server, &mut ports);
    assert_eq!(server.protocol(), Some(http::Protocol::Http1));
    assert!(ports.detection_closed.is_empty());
}

#[test]
fn active_alarm_failure_waits_for_read_and_preserves_transport_cause() {
    for yielding in [false, true] {
        for ack_first in [false, true] {
            for failure in [h2::IoFailure::Cancelled, h2::IoFailure::Failed] {
                let mut server = detecting();
                let mut ports = Ports {
                    yielding,
                    ..Ports::default()
                };
                drive(&mut server, &mut ports);
                let alarm = ports.h2.alarms.pop().unwrap();
                server.complete_wake(alarm.failed(failure)).unwrap();
                server.advance_time(Duration::ZERO).unwrap();
                server.abort();
                drive(&mut server, &mut ports);
                assert_eq!(ports.h2.cancels.len(), 1);
                assert!(ports.h2.close.is_none());
                let cancel = ports.h2.cancels.pop().unwrap();
                let mut cancel = Some(cancel);
                if ack_first {
                    server
                        .complete_cancel(cancel.take().unwrap().complete())
                        .unwrap();
                    drive(&mut server, &mut ports);
                    assert!(ports.h2.close.is_none());
                }
                let mut read = ports.h2.read.take().unwrap();
                read.buffer_mut()[0] = b'G';
                server
                    .complete_read(read.complete(h2::ReadOutcome::Read(1)))
                    .unwrap();
                if let Some(cancel) = cancel {
                    drive(&mut server, &mut ports);
                    assert!(ports.h2.close.is_none());
                    server.complete_cancel(cancel.complete()).unwrap();
                }
                drive(&mut server, &mut ports);
                let close = ports.h2.close.take().unwrap();
                server
                    .complete_close(close.complete(Err(h2::IoFailure::Failed)))
                    .unwrap();
                drive(&mut server, &mut ports);
                server.abort();
                drive(&mut server, &mut ports);
                assert_eq!(
                    ports.detection_closed,
                    [http::DetectionClosed {
                        failure: http::DetectionFailure::Transport,
                        close_result: Err(h2::IoFailure::Failed),
                    }]
                );
                assert_eq!(server.protocol(), None);
                assert!(ports.h1_trace.is_empty());
                assert!(ports.h2.write.is_none());
                assert!(ports.h2.closed.is_empty());
                assert_eq!(ports.h2.sequence, ["wake", "read", "cancel", "close"]);
            }
        }
    }
}

#[test]
fn wrong_owner_returns_a_failed_alarm_unchanged() {
    let mut first = detecting();
    let mut second = detecting();
    let mut ports = Ports::default();
    drive(&mut first, &mut ports);
    let alarm = ports.h2.alarms.pop().unwrap();
    let token = alarm.token().clone();
    let rejected = second
        .complete_wake(alarm.failed(h2::IoFailure::Failed))
        .unwrap_err();
    assert_eq!(rejected.error, h2::CommandError::InvalidCompletion);
    assert_eq!(rejected.value.token(), &token);
    assert_eq!(
        rejected.value.outcome(),
        h2::WakeOutcome::Failed(h2::IoFailure::Failed)
    );
    first.complete_wake(rejected.value).unwrap();
    drive(&mut first, &mut ports);
    assert_eq!(ports.h2.cancels.len(), 1);
    assert_eq!(second.protocol(), None);
}

#[test]
fn obsolete_alarm_failure_preserves_selection_before_and_after_cancel_dispatch() {
    for prefix in [b"G".as_slice(), b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"] {
        for yielding in [false, true] {
            for dispatch_cancel in [false, true] {
                for ack_first in [false, true] {
                    for failure in [h2::IoFailure::Cancelled, h2::IoFailure::Failed] {
                        let mut server = detecting();
                        let mut ports = Ports {
                            yielding,
                            ..Ports::default()
                        };
                        drive(&mut server, &mut ports);
                        let alarm = ports.h2.alarms.pop().unwrap();
                        let mut read = ports.h2.read.take().unwrap();
                        read.buffer_mut()[..prefix.len()].copy_from_slice(prefix);
                        server
                            .complete_read(read.complete(h2::ReadOutcome::Read(prefix.len())))
                            .unwrap();
                        let mut cancel = None;
                        if dispatch_cancel {
                            drive(&mut server, &mut ports);
                            assert_eq!(ports.h2.cancels.len(), 1);
                            cancel = ports.h2.cancels.pop();
                            if ack_first {
                                server
                                    .complete_cancel(cancel.take().unwrap().complete())
                                    .unwrap();
                                drive(&mut server, &mut ports);
                            }
                            assert_eq!(server.protocol(), None);
                        }
                        server.complete_wake(alarm.failed(failure)).unwrap();
                        server.advance_time(Duration::ZERO).unwrap();
                        if let Some(cancel) = cancel {
                            drive(&mut server, &mut ports);
                            assert_eq!(server.protocol(), None);
                            server.complete_cancel(cancel.complete()).unwrap();
                        }
                        drive(&mut server, &mut ports);
                        assert_eq!(
                            server.protocol(),
                            Some(if prefix.len() == 1 {
                                http::Protocol::Http1
                            } else {
                                http::Protocol::Http2
                            })
                        );
                        assert!(ports.detection_closed.is_empty());
                        assert!(ports.h2.closed.is_empty());
                        assert!(ports.closed.is_empty());
                        assert!(ports.h2.close.is_none());
                        assert!(ports.close.is_none());
                    }
                }
            }
        }
    }
}

#[test]
fn hard_abort_joins_detection_operations_and_preserves_an_earlier_timeout() {
    for yielding in [false, true] {
        for timeout_first in [false, true] {
            for ack_first in [false, true] {
                let mut server = detecting();
                let mut ports = Ports {
                    yielding,
                    ..Ports::default()
                };
                drive(&mut server, &mut ports);
                if timeout_first {
                    server.advance_time(Duration::from_secs(1)).unwrap();
                }
                server.abort();
                server.abort();
                drive(&mut server, &mut ports);
                assert_eq!(ports.h2.cancels.len(), 2);
                assert!(ports.h2.close.is_none());
                let mut cancels = std::mem::take(&mut ports.h2.cancels);
                if ack_first {
                    for cancel in cancels.drain(..) {
                        server.complete_cancel(cancel.complete()).unwrap();
                    }
                    drive(&mut server, &mut ports);
                    assert!(ports.h2.close.is_none());
                }
                let alarm = ports.h2.alarms.pop().unwrap();
                server
                    .complete_wake(alarm.failed(h2::IoFailure::Failed))
                    .unwrap();
                let mut read = ports.h2.read.take().unwrap();
                read.buffer_mut()[0] = b'G';
                server
                    .complete_read(read.complete(h2::ReadOutcome::Read(1)))
                    .unwrap();
                if !ack_first {
                    drive(&mut server, &mut ports);
                    assert!(ports.h2.close.is_none());
                }
                for cancel in cancels {
                    server.complete_cancel(cancel.complete()).unwrap();
                }
                drive(&mut server, &mut ports);
                let close = ports.h2.close.take().unwrap();
                server.complete_close(close.complete(Ok(()))).unwrap();
                drive(&mut server, &mut ports);
                server.abort();
                drive(&mut server, &mut ports);
                assert_eq!(
                    ports.detection_closed,
                    [http::DetectionClosed {
                        failure: if timeout_first {
                            http::DetectionFailure::Timeout
                        } else {
                            http::DetectionFailure::Aborted
                        },
                        close_result: Ok(()),
                    }]
                );
                assert_eq!(server.protocol(), None);
                assert!(ports.h1_trace.is_empty());
                assert!(ports.h2.write.is_none());
                assert!(ports.h2.closed.is_empty());
                assert_eq!(
                    ports.h2.sequence,
                    ["wake", "read", "cancel", "cancel", "close"]
                );
            }
        }
    }
}

#[test]
fn selected_clients_and_servers_hard_abort_without_starting_transport_work() {
    for yielding in [false, true] {
        let h1_client = h1::Client::with_output_type(
            h1::ConnectionId {
                slot: 4,
                generation: 2,
            },
            h1::Config::default(),
            vec![0; 65536],
            h1::Tick(0),
        )
        .unwrap();
        for mut client in [
            http::Client::http1(h1_client),
            http::Client::http2(h2::Client::new(h2::Config::default(), Duration::ZERO).unwrap()),
        ] {
            let mut ports = Ports {
                yielding,
                ..Ports::default()
            };
            client.abort();
            client.abort();
            drive_client(&mut client, &mut ports);
            match client.protocol() {
                http::Protocol::Http1 => {
                    let close = ports.close.take().unwrap();
                    client
                        .http1_mut()
                        .unwrap()
                        .complete_close(close.complete(Ok(())))
                        .unwrap();
                }
                http::Protocol::Http2 => {
                    let close = ports.h2.close.take().unwrap();
                    client
                        .http2_mut()
                        .unwrap()
                        .complete_close(close.complete(Ok(())))
                        .unwrap();
                }
            }
            drive_client(&mut client, &mut ports);
            client.abort();
            assert!(client.next(&mut ports).is_none());
            if client.protocol() == http::Protocol::Http1 {
                assert_eq!(ports.closed, [Err(h1::Failure::Cancelled)]);
                assert!(ports.h2.sequence.is_empty());
            } else {
                assert_eq!(ports.h2.closed, [h2::ConnectionResult::Aborted]);
                assert_eq!(ports.h2.sequence, ["close", "closed"]);
                assert!(ports.h1_trace.is_empty());
            }
            assert!(ports.read.is_none());
            assert!(ports.write.is_none());
        }
        for mut server in [
            http::Server::http1(h1_server()),
            http::Server::http2(h2::Server::new(h2::Config::default(), Duration::ZERO).unwrap()),
        ] {
            let mut ports = Ports {
                yielding,
                ..Ports::default()
            };
            server.abort();
            server.abort();
            drive(&mut server, &mut ports);
            match server.protocol().unwrap() {
                http::Protocol::Http1 => {
                    let close = ports.close.take().unwrap();
                    server
                        .http1_mut()
                        .unwrap()
                        .complete_close(close.complete(Ok(())))
                        .unwrap();
                }
                http::Protocol::Http2 => {
                    let close = ports.h2.close.take().unwrap();
                    server.complete_close(close.complete(Ok(()))).unwrap();
                }
            }
            drive(&mut server, &mut ports);
            server.abort();
            drive(&mut server, &mut ports);
            if server.protocol() == Some(http::Protocol::Http1) {
                assert_eq!(ports.closed, [Err(h1::Failure::Cancelled)]);
                assert!(ports.h2.sequence.is_empty());
            } else {
                assert_eq!(ports.h2.closed, [h2::ConnectionResult::Aborted]);
                assert_eq!(ports.h2.sequence, ["close", "closed"]);
                assert!(ports.h1_trace.is_empty());
            }
            assert!(ports.read.is_none());
            assert!(ports.write.is_none());
            assert!(ports.detection_closed.is_empty());
        }
    }
}

#[test]
fn abort_after_detection_selection_never_activates_the_child() {
    for prefix in [b"G".as_slice(), b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"] {
        for yielding in [false, true] {
            for dispatched in [false, true] {
                let mut server = detecting();
                let mut ports = Ports {
                    yielding,
                    ..Ports::default()
                };
                drive(&mut server, &mut ports);
                let mut read = ports.h2.read.take().unwrap();
                read.buffer_mut()[..prefix.len()].copy_from_slice(prefix);
                server
                    .complete_read(read.complete(h2::ReadOutcome::Read(prefix.len())))
                    .unwrap();
                if dispatched {
                    drive(&mut server, &mut ports);
                }
                server.abort();
                drive(&mut server, &mut ports);
                assert_eq!(ports.h2.cancels.len(), 1);
                assert!(ports.h2.close.is_none());
                settle_detection_cancels(&mut server, &mut ports);
                drive(&mut server, &mut ports);
                let close = ports.h2.close.take().unwrap();
                server.complete_close(close.complete(Ok(()))).unwrap();
                drive(&mut server, &mut ports);
                assert_eq!(server.protocol(), None);
                assert_eq!(
                    ports.detection_closed,
                    [http::DetectionClosed {
                        failure: http::DetectionFailure::Aborted,
                        close_result: Ok(()),
                    }]
                );
                assert!(ports.h1_trace.is_empty());
                assert_eq!(ports.h2.sequence, ["wake", "read", "cancel", "close"]);
            }
        }
    }
}

#[test]
fn timeout_waits_for_originals_and_reports_close_failure_once() {
    for yielding in [false, true] {
        for ack_first in [false, true] {
            let mut server = detecting();
            let mut ports = Ports {
                yielding,
                ..Ports::default()
            };
            drive(&mut server, &mut ports);
            server.advance_time(Duration::from_secs(1)).unwrap();
            drive(&mut server, &mut ports);
            assert_eq!(ports.h2.cancels.len(), 2);
            assert!(ports.h2.close.is_none());
            let cancels = std::mem::take(&mut ports.h2.cancels);
            if ack_first {
                for cancel in cancels {
                    server.complete_cancel(cancel.complete()).unwrap();
                }
                drive(&mut server, &mut ports);
                assert!(ports.h2.close.is_none());
            } else {
                ports.h2.cancels = cancels;
            }
            let mut read = ports.h2.read.take().unwrap();
            read.buffer_mut()[0] = b'G';
            server
                .complete_read(read.complete(h2::ReadOutcome::Read(1)))
                .unwrap();
            for alarm in ports.h2.alarms.drain(..) {
                server
                    .complete_wake(alarm.complete(Duration::ZERO))
                    .unwrap();
            }
            for cancel in ports.h2.cancels.drain(..) {
                server.complete_cancel(cancel.complete()).unwrap();
            }
            drive(&mut server, &mut ports);
            assert!(ports.requests.is_empty());
            assert!(ports.h2.heads.is_empty());
            let close = ports.h2.close.take().unwrap();
            server
                .complete_close(close.complete(Err(h2::IoFailure::Failed)))
                .unwrap();
            drive(&mut server, &mut ports);
            drive(&mut server, &mut ports);
            assert_eq!(
                ports.detection_closed,
                [http::DetectionClosed {
                    failure: http::DetectionFailure::Timeout,
                    close_result: Err(h2::IoFailure::Failed),
                }]
            );
            assert!(ports.h2.closed.is_empty());
            assert!(ports.closed.is_empty());
        }
    }
}

#[test]
fn wrong_owner_and_invalid_count_return_the_original_operation() {
    let mut first = detecting();
    let mut second = detecting();
    let mut ports = Ports::default();
    drive(&mut first, &mut ports);
    let read = ports.h2.read.take().unwrap();
    let rejected = second
        .complete_read(read.complete(h2::ReadOutcome::Read(1)))
        .unwrap_err();
    let (read, _) = rejected.value.into_parts();
    let rejected = first
        .complete_read(read.complete(h2::ReadOutcome::Read(0)))
        .unwrap_err();
    let (mut read, _) = rejected.value.into_parts();
    read.buffer_mut()[0] = b'G';
    first
        .complete_read(read.complete(h2::ReadOutcome::Read(1)))
        .unwrap();
    drive(&mut first, &mut ports);
    settle_detection_cancels(&mut first, &mut ports);
    drive(&mut first, &mut ports);
    assert_eq!(first.protocol(), Some(http::Protocol::Http1));
    assert_eq!(second.protocol(), None);
}

#[test]
fn detector_model_covers_batched_originals_and_both_cancel_join_orders() {
    let prefaces: [&[u8]; 2] = [b"G", b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"];
    let mut schedules = 0;
    for prefix in prefaces {
        for yielding in [false, true] {
            for read_first in [false, true] {
                for batch in [false, true] {
                    for ack_first in [false, true] {
                        schedules += 1;
                        let mut server = detecting();
                        let mut ports = Ports {
                            yielding,
                            ..Ports::default()
                        };
                        drive(&mut server, &mut ports);
                        let mut read = ports.h2.read.take().unwrap();
                        read.buffer_mut()[..prefix.len()].copy_from_slice(prefix);
                        let mut read = Some(read);
                        let mut alarm = Some(ports.h2.alarms.pop().unwrap());
                        for index in 0..2 {
                            if (index == 0) == read_first {
                                server
                                    .complete_read(
                                        read.take()
                                            .unwrap()
                                            .complete(h2::ReadOutcome::Read(prefix.len())),
                                    )
                                    .unwrap();
                            } else {
                                server
                                    .complete_wake(
                                        alarm.take().unwrap().complete(Duration::from_secs(1)),
                                    )
                                    .unwrap();
                            }
                            if !batch && index == 0 {
                                drive(&mut server, &mut ports);
                                assert_eq!(ports.h2.cancels.len(), 1);
                                if ack_first {
                                    let cancel = ports.h2.cancels.pop().unwrap();
                                    server.complete_cancel(cancel.complete()).unwrap();
                                    drive(&mut server, &mut ports);
                                }
                                assert!(ports.h2.close.is_none());
                                assert_eq!(server.protocol(), None);
                            }
                        }
                        if !ports.h2.cancels.is_empty() {
                            drive(&mut server, &mut ports);
                            assert!(ports.h2.close.is_none());
                            assert_eq!(server.protocol(), None);
                            let cancel = ports.h2.cancels.pop().unwrap();
                            server.complete_cancel(cancel.complete()).unwrap();
                        }
                        drive(&mut server, &mut ports);
                        if read_first {
                            assert_eq!(
                                server.protocol(),
                                Some(if prefix.len() == 1 {
                                    http::Protocol::Http1
                                } else {
                                    http::Protocol::Http2
                                })
                            );
                            assert!(ports.detection_closed.is_empty());
                            assert!(ports.h2.close.is_none());
                        } else {
                            assert_eq!(server.protocol(), None);
                            let close = ports.h2.close.take().unwrap();
                            server.complete_close(close.complete(Ok(()))).unwrap();
                            drive(&mut server, &mut ports);
                            assert_eq!(
                                ports.detection_closed,
                                [http::DetectionClosed {
                                    failure: http::DetectionFailure::Timeout,
                                    close_result: Ok(()),
                                }]
                            );
                            assert!(ports.requests.is_empty());
                            assert!(ports.h2.heads.is_empty());
                            assert!(ports.h2.write.is_none());
                        }
                        assert_eq!(
                            ports
                                .h2
                                .sequence
                                .iter()
                                .filter(|event| **event == "cancel")
                                .count(),
                            usize::from(!batch),
                        );
                    }
                }
            }
        }
    }
    assert_eq!(schedules, 32);
}

#[test]
fn selected_client_preserves_http1_commands_and_callbacks() {
    for yielding in [false, true] {
        let child = h1::Client::with_output_type(
            h1::ConnectionId {
                slot: 4,
                generation: 2,
            },
            h1::Config::default(),
            vec![0; 65536],
            h1::Tick(0),
        )
        .unwrap();
        let mut client = http::Client::http1(child);
        assert_eq!(client.protocol(), http::Protocol::Http1);
        assert!(client.http2_mut().is_none());
        #[cfg(feature = "http1-metrics")]
        {
            let before = client.http1_mut().unwrap().metrics();
            assert_eq!(client.http1_metrics(), Some(before));
            assert_eq!(client.http1_mut().unwrap().metrics(), before);
        }
        client
            .http1_mut()
            .unwrap()
            .request(h1::Request {
                head: h1::RequestHead {
                    method: "GET",
                    target: "/",
                    version: h1::Version::Http11,
                    headers: &[h1::Header {
                        name: "host",
                        value: b"example",
                    }],
                },
                body: h1::BodyLength::Empty,
                expect_continue: false,
            })
            .unwrap();
        let mut ports = Ports {
            yielding,
            ..Ports::default()
        };
        let mut response = Some(b"HTTP/1.1 204 No Content\r\n\r\n".as_slice());
        let mut wire = Vec::new();
        for _ in 0..100 {
            client.next(&mut ports);
            if let Some(op) = ports.write.take() {
                let bytes: Vec<u8> = op.slices().into_iter().flatten().copied().collect();
                wire.extend_from_slice(&bytes);
                client
                    .http1_mut()
                    .unwrap()
                    .complete_write(op.complete(Ok(bytes.len())))
                    .unwrap();
            }

            if let Some(mut op) = ports.read.take() {
                if let Some(bytes) = response.take() {
                    op.bytes_mut()[..bytes.len()].copy_from_slice(bytes);
                    client
                        .http1_mut()
                        .unwrap()
                        .complete_read(op.complete(Ok(bytes.len())))
                        .unwrap();
                } else {
                    ports.read = Some(op);
                }
            }
            if ports.h1_trace.contains(&"exchange_finished") {
                break;
            }
        }
        assert_eq!(
            wire,
            b"GET / HTTP/1.1\r\nhost: example\r\ncontent-length: 0\r\n\r\n"
        );
        assert_eq!(
            ports
                .h1_trace
                .iter()
                .filter(|event| **event == "response")
                .count(),
            1
        );
        assert_eq!(
            ports
                .h1_trace
                .iter()
                .filter(|event| **event == "exchange_finished")
                .count(),
            1
        );
        assert!(ports.h2.sequence.is_empty());
    }
}

#[test]
fn selected_http2_client_has_the_direct_initial_wire_and_notifications() {
    for yielding in [false, true] {
        let mut direct = h2::Client::new(h2::Config::default(), Duration::ZERO).unwrap();
        let mut selected =
            http::Client::http2(h2::Client::new(h2::Config::default(), Duration::ZERO).unwrap());
        let fields = support::request(b"GET");
        let expected_id = direct.request(&fields, true).unwrap();
        let id = selected
            .http2_mut()
            .unwrap()
            .request(&fields, true)
            .unwrap();
        assert_eq!(id, expected_id);
        assert_eq!(selected.protocol(), http::Protocol::Http2);
        assert!(selected.http1_mut().is_none());
        #[cfg(feature = "http1-metrics")]
        assert_eq!(selected.http1_metrics(), None);
        let mut baseline = support::MemoryPorts::default();
        let mut ports = Ports {
            yielding,
            ..Ports::default()
        };
        direct.next(&mut baseline);
        for _ in 0..100 {
            if selected.next(&mut ports).is_none() {
                break;
            }
        }
        let wire = |op: &h2::WriteOp| -> Vec<u8> {
            op.slices()
                .iter()
                .flat_map(|part| part.iter().copied())
                .collect()
        };
        assert_eq!(
            wire(ports.h2.write.as_ref().unwrap()),
            wire(baseline.write.as_ref().unwrap())
        );
        assert_eq!(ports.h2.sequence, baseline.sequence);
        assert!(ports.h1_trace.is_empty());
    }
}
