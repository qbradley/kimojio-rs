#[path = "../examples/support/mod.rs"]
mod support;

use std::time::Duration;

use kimojio_fsm_http1 as h1;
use kimojio_fsm_http2::{self as h2, http};

const FIRST: &[u8] = b"GET /first HTTP/1.1\r\nHost: test\r\n\r\n";
const PARTIAL: &[u8] = b"GET /second HTTP/1.1\r\nHost:";
const OK: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n";
const TIMEOUT: &[u8] =
    b"HTTP/1.1 408 Request Timeout\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";

#[derive(Default)]
struct Ports {
    detection: support::MemoryPorts,
    read: Option<h1::ReadOp<Vec<u8>>>,
    write: Option<h1::WriteOp<Vec<u8>>>,
    cancels: Vec<h1::CancelOp>,
    close: Option<h1::CloseOp>,
    requests: Vec<h1::ExchangeId>,
    retired: Vec<h1::ExchangeFinished>,
    deadlines: Vec<Option<h1::Deadline>>,
    closed: Vec<h1::ConnectionResult>,
    trace: Vec<&'static str>,
    yielding: bool,
}

impl Ports {
    fn event(&mut self, name: &'static str) -> Option<()> {
        self.trace.push(name);
        self.yielding.then_some(())
    }
}

impl h1::Ports<Vec<u8>> for Ports {
    type Output = ();
    fn read(&mut self, op: h1::ReadOp<Vec<u8>>) -> Option<()> {
        assert!(self.read.replace(op).is_none());
        self.event("read")
    }
    fn write(&mut self, op: h1::WriteOp<Vec<u8>>) -> Option<()> {
        assert!(self.write.replace(op).is_none());
        self.event("write")
    }
    fn readiness(&mut self, _: h1::ReadinessOp) -> Option<()> {
        panic!("no would-block completions")
    }
    fn cancel(&mut self, op: h1::CancelOp) -> Option<()> {
        self.cancels.push(op);
        self.event("cancel")
    }
    fn close(&mut self, op: h1::CloseOp) -> Option<()> {
        assert!(self.close.replace(op).is_none());
        self.event("close")
    }
    fn body(&mut self, _: h1::BodyOp<Vec<u8>>) -> Option<()> {
        panic!("empty request")
    }
    fn trailers(&mut self, _: h1::ExchangeId, _: h1::Headers<'_>) -> Option<()> {
        panic!("empty request")
    }
    fn incoming_finished(&mut self, id: h1::ExchangeId) -> Option<()> {
        assert_eq!(self.requests, [id]);
        self.event("incoming_finished")
    }
    fn send_ready(&mut self, _: h1::ExchangeId, _: usize) -> Option<()> {
        panic!("empty response")
    }
    fn source_finished(&mut self, id: h1::ExchangeId) -> Option<()> {
        assert_eq!(self.requests, [id]);
        self.event("source_finished")
    }
    fn body_sent(&mut self, _: h1::BodySent<Vec<u8>>) -> Option<()> {
        panic!("empty response")
    }
    fn exchange_finished(&mut self, result: h1::ExchangeFinished) -> Option<()> {
        self.retired.push(result);
        self.event("exchange_finished")
    }
    fn deadline_changed(&mut self, deadline: Option<h1::Deadline>) -> Option<()> {
        self.deadlines.push(deadline);
        self.event("deadline_changed")
    }
    fn upgrade_ready(&mut self, _: h1::ExchangeId) -> Option<()> {
        panic!("no upgrade")
    }
    fn closed(&mut self, result: h1::ConnectionResult) -> Option<()> {
        self.closed.push(result);
        self.event("closed")
    }
}

impl h1::ServerPorts<Vec<u8>> for Ports {
    fn request(&mut self, id: h1::ExchangeId, head: h1::RequestHead<'_>) -> Option<()> {
        assert_eq!((head.method, head.target), ("GET", "/first"));
        self.requests.push(id);
        self.event("request")
    }
}

impl h2::Ports<Vec<u8>> for Ports {
    type Output = ();
    fn read(&mut self, op: h2::ReadOp) -> Option<()> {
        h2::Ports::read(&mut self.detection, op)
    }
    fn wake(&mut self, op: h2::WakeOp) -> Option<()> {
        h2::Ports::wake(&mut self.detection, op)
    }
    fn cancel(&mut self, op: h2::CancelOp) -> Option<()> {
        h2::Ports::cancel(&mut self.detection, op)
    }
    fn write(&mut self, _: h2::WriteOp) -> Option<()> {
        panic!("HTTP/1 selected")
    }
    fn headers(&mut self, _: h2::Head<'_>) -> Option<()> {
        panic!("HTTP/1 selected")
    }
    fn body(&mut self, _: h2::BodyOp) -> Option<()> {
        panic!("HTTP/1 selected")
    }
    fn send_ready(&mut self, _: h2::SendPermit) -> Option<()> {
        panic!("HTTP/1 selected")
    }
    fn admission_changed(&mut self) -> Option<()> {
        panic!("HTTP/1 selected")
    }
    fn send_stopped(&mut self, _: h2::StreamId, _: h2::SendStop) -> Option<()> {
        panic!("HTTP/1 selected")
    }
    fn sent(&mut self, _: h2::Sent) -> Option<()> {
        panic!("HTTP/1 selected")
    }
    fn ended(&mut self, _: h2::ReceiveEnd) -> Option<()> {
        panic!("HTTP/1 selected")
    }
    fn retired(&mut self, _: h2::StreamResult) -> Option<()> {
        panic!("HTTP/1 selected")
    }
    fn close(&mut self, _: h2::CloseOp) -> Option<()> {
        panic!("HTTP/1 child owns close")
    }
    fn closed(&mut self, _: h2::ConnectionResult) -> Option<()> {
        panic!("HTTP/1 child owns close")
    }
    fn reschedule(&mut self) -> Option<()> {
        Some(())
    }
}

impl http::ServerPorts<Vec<u8>> for Ports {
    fn detection_closed(&mut self, _: http::DetectionClosed) -> Option<()> {
        panic!("detection must select HTTP/1")
    }
}

fn server(detect: bool) -> http::Server {
    let id = h1::ConnectionId {
        slot: 1,
        generation: 1,
    };
    let config = h1::Config {
        idle_timeout_ns: None,
        head_timeout_ns: Some(1_000_000_000),
        ..h1::Config::default()
    };
    if detect {
        http::Server::detect(
            http::DetectionConfig {
                http1_connection: id,
                http1_config: config,
                http1_buffer: vec![0; 65536],
                http2_config: h2::Config::default(),
                timeout: Duration::from_secs(5),
            },
            Duration::ZERO,
        )
        .unwrap()
    } else {
        http::Server::http1(h1::Server::new(id, config, vec![0; 65536], h1::Tick(0)).unwrap())
    }
}

fn drive(server: &mut http::Server, ports: &mut Ports) {
    for _ in 0..100 {
        let yielded = server.next(ports).is_some();
        let cancels = std::mem::take(&mut ports.detection.cancels);
        if !yielded && cancels.is_empty() {
            return;
        }
        for cancel in cancels {
            let index = ports
                .detection
                .alarms
                .iter()
                .position(|op| op.token() == cancel.original())
                .expect("only the detection alarm is outstanding");
            let alarm = ports.detection.alarms.remove(index);
            server
                .complete_wake(alarm.failed(h2::IoFailure::Cancelled))
                .unwrap();
            server.complete_cancel(cancel.complete()).unwrap();
        }
    }
    panic!("composition did not suspend");
}

fn input(server: &mut http::Server, ports: &mut Ports, mut bytes: &[u8]) {
    loop {
        let count = if let Some(mut op) = ports.detection.read.take() {
            let count = op.buffer_mut().len().min(bytes.len());
            op.buffer_mut()[..count].copy_from_slice(&bytes[..count]);
            server
                .complete_read(op.complete(h2::ReadOutcome::Read(count)))
                .unwrap();
            count
        } else {
            let mut op = ports.read.take().unwrap();
            let count = op.bytes_mut().len().min(bytes.len());
            op.bytes_mut()[..count].copy_from_slice(&bytes[..count]);
            server
                .http1_mut()
                .unwrap()
                .complete_read(op.complete(Ok(count)))
                .unwrap();
            count
        };
        drive(server, ports);
        bytes = &bytes[count..];
        if bytes.is_empty() {
            return;
        }
    }
}

fn write(server: &mut http::Server, ports: &mut Ports, expected: &[u8]) {
    let op = ports.write.take().unwrap();
    let bytes: Vec<_> = op.slices().into_iter().flatten().copied().collect();
    assert_eq!(bytes, expected);
    server
        .http1_mut()
        .unwrap()
        .complete_write(op.complete(Ok(bytes.len())))
        .unwrap();
    drive(server, ports);
}

fn first_exchange(server: &mut http::Server, ports: &mut Ports, pipeline: bool) {
    drive(server, ports);
    let bytes = if pipeline {
        [FIRST, PARTIAL].concat()
    } else {
        FIRST.to_vec()
    };
    input(server, ports, &bytes);
    assert_eq!(server.protocol(), Some(http::Protocol::Http1));
    assert_eq!(ports.requests.len(), 1);
    server
        .http1_mut()
        .unwrap()
        .respond(
            ports.requests[0],
            h1::Response::new(200, "OK", &[], h1::BodyLength::Empty),
        )
        .unwrap();
    drive(server, ports);
    write(server, ports, OK);
    assert_eq!(
        ports.retired,
        [h1::ExchangeFinished {
            exchange: ports.requests[0],
            result: Ok(()),
            reusable: true,
        }]
    );
    assert!(ports.detection.alarms.is_empty());
    assert!(ports.read.is_some());
    assert!(ports.closed.is_empty());
    assert!(ports.close.is_none());
    assert_eq!(
        ports
            .trace
            .iter()
            .copied()
            .filter(|event| !matches!(*event, "read" | "write" | "deadline_changed"))
            .collect::<Vec<_>>(),
        [
            "request",
            "incoming_finished",
            "source_finished",
            "exchange_finished",
        ]
    );
}

fn close(server: &mut http::Server, ports: &mut Ports, result: h1::ConnectionResult) {
    let close = ports.close.take().expect("transport close was not issued");
    assert!(
        ports.closed.is_empty(),
        "closed before transport settlement"
    );
    assert!(ports.read.is_none());
    assert!(ports.write.is_none());
    assert!(ports.cancels.is_empty());
    server
        .http1_mut()
        .unwrap()
        .complete_close(close.complete(Ok(())))
        .unwrap();
    drive(server, ports);
    assert_eq!(ports.closed, [result]);
    let trace = ports.trace.clone();
    server.advance_time(Duration::from_secs(100)).unwrap();
    drive(server, ports);
    assert_eq!(ports.trace, trace, "terminal connection emitted more work");
}

#[test]
fn selected_and_detected_reused_partial_heads_arm_and_expire() {
    for detect in [false, true] {
        for yielding in [false, true] {
            for pipeline in [false, true] {
                let mut server = server(detect);
                let mut ports = Ports {
                    yielding,
                    ..Ports::default()
                };
                first_exchange(&mut server, &mut ports, pipeline);
                let start = if pipeline {
                    Duration::ZERO
                } else {
                    assert_eq!(ports.deadlines.last(), Some(&None));
                    server.advance_time(Duration::from_secs(20)).unwrap();
                    drive(&mut server, &mut ports);
                    assert!(ports.close.is_none());
                    input(&mut server, &mut ports, PARTIAL);
                    Duration::from_secs(20)
                };
                let deadline = ports.deadlines.last().copied().flatten().unwrap();
                assert_eq!(
                    deadline.at,
                    h1::Tick((start + Duration::from_secs(1)).as_nanos() as u64)
                );
                let trace = ports.trace.clone();
                server
                    .advance_time(start + Duration::from_secs(1) - Duration::from_nanos(1))
                    .unwrap();
                drive(&mut server, &mut ports);
                assert_eq!(ports.trace, trace);
                server.advance_time(start + Duration::from_secs(1)).unwrap();
                server
                    .http1_mut()
                    .unwrap()
                    .expire(deadline, deadline.at)
                    .unwrap();
                drive(&mut server, &mut ports);
                assert_eq!(ports.cancels.len(), 1);
                let read = ports.read.take().unwrap();
                assert_eq!(ports.cancels.pop().unwrap().target, read.id());
                assert!(ports.close.is_none(), "close raced an outstanding read");
                server
                    .http1_mut()
                    .unwrap()
                    .complete_read(read.complete(Err(h1::IoError {
                        kind: h1::IoErrorKind::Cancelled,
                        code: None,
                    })))
                    .unwrap();
                drive(&mut server, &mut ports);
                write(&mut server, &mut ports, TIMEOUT);
                close(&mut server, &mut ports, Err(h1::Failure::Timeout));
                assert_eq!(
                    &ports.trace[trace.len()..],
                    [
                        "deadline_changed",
                        "cancel",
                        "write",
                        "deadline_changed",
                        "close",
                        "closed",
                    ]
                );
                assert_eq!(ports.requests.len(), 1);
                assert_eq!(ports.retired.len(), 1);
                assert_eq!(ports.deadlines.last(), Some(&None));
            }
        }
    }
}

#[test]
fn selected_and_detected_reused_idle_eof_does_not_arm_a_head_deadline() {
    for detect in [false, true] {
        for yielding in [false, true] {
            let mut server = server(detect);
            let mut ports = Ports {
                yielding,
                ..Ports::default()
            };
            first_exchange(&mut server, &mut ports, false);
            assert_eq!(ports.deadlines.last(), Some(&None));
            let deadlines = ports.deadlines.clone();
            let trace = ports.trace.clone();
            server.advance_time(Duration::from_secs(20)).unwrap();
            drive(&mut server, &mut ports);
            assert_eq!(ports.trace, trace);
            input(&mut server, &mut ports, b"");
            close(&mut server, &mut ports, Ok(()));
            assert_eq!(ports.deadlines, deadlines);
            assert_eq!(&ports.trace[trace.len()..], ["close", "closed"]);
            assert_eq!(ports.requests.len(), 1);
            assert_eq!(ports.retired.len(), 1);
        }
    }
}
