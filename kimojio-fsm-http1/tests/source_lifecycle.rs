mod support;
use kimojio_fsm_http1::*;
use support::{B, Capture, Event, config, fill, finish_write, response};

#[derive(Debug)]
enum Output {
    Core(Event),
    SourceFinished(ExchangeId),
}

struct SourceCapture;

macro_rules! forward {
    ($name:ident, $ty:ty) => {
        fn $name(&mut self, value: $ty) -> Option<Output> {
            Capture.$name(value).map(Output::Core)
        }
    };
}

impl Ports<B> for SourceCapture {
    type Output = Output;
    forward!(read, ReadOp<B>);
    forward!(write, WriteOp<B>);
    forward!(readiness, ReadinessOp);
    forward!(cancel, CancelOp);
    forward!(close, CloseOp);
    forward!(body, BodyOp<B>);
    forward!(incoming_finished, ExchangeId);
    forward!(body_sent, BodySent<B>);
    forward!(exchange_finished, ExchangeFinished);
    forward!(deadline_changed, Option<Deadline>);
    forward!(upgrade_ready, ExchangeId);
    forward!(closed, ConnectionResult);
    fn source_finished(&mut self, exchange: ExchangeId) -> Option<Output> {
        Some(Output::SourceFinished(exchange))
    }
    fn trailers(&mut self, exchange: ExchangeId, headers: Headers<'_>) -> Option<Output> {
        Capture.trailers(exchange, headers).map(Output::Core)
    }
    fn send_ready(&mut self, exchange: ExchangeId, capacity: usize) -> Option<Output> {
        Capture.send_ready(exchange, capacity).map(Output::Core)
    }
}

impl ServerPorts<B> for SourceCapture {
    fn request(&mut self, exchange: ExchangeId, head: RequestHead<'_>) -> Option<Output> {
        Capture.request(exchange, head).map(Output::Core)
    }
}

fn next(server: &mut Server<B>) -> Option<Output> {
    loop {
        let event = server.next(&mut SourceCapture);
        if matches!(event, Some(Output::Core(Event::Deadline(_)))) {
            continue;
        }
        return event;
    }
}

#[test]
fn suppressed_source_releases_retained_request_body_without_status_policy_in_adapter() {
    for (method, status, length) in [
        ("HEAD", 200, BodyLength::Known(3)),
        ("POST", 204, BodyLength::Empty),
        ("POST", 304, BodyLength::Known(3)),
        ("POST", 200, BodyLength::Empty),
    ] {
        let mut server = support::server(config());
        let Some(Output::Core(Event::Read(op))) = next(&mut server) else {
            panic!()
        };
        let request = format!("{method} / HTTP/1.1\r\nHost: a\r\nContent-Length: 3\r\n\r\nabc");
        server.complete_read(fill(op, request.as_bytes())).unwrap();
        let Some(Output::Core(Event::Request(id, _))) = next(&mut server) else {
            panic!()
        };
        server.grant_body_credit(id, 3).unwrap();
        let Some(Output::Core(Event::Body(retained_by_source))) = next(&mut server) else {
            panic!()
        };
        server
            .respond(
                id,
                Response {
                    head: ResponseHead {
                        status,
                        ..response(length).head
                    },
                    body: length,
                },
            )
            .unwrap();
        let Some(Output::SourceFinished(finished)) = next(&mut server) else {
            panic!("source remained stranded")
        };
        assert_eq!(finished, id);
        server.release_body(retained_by_source.release(0)).unwrap();
        let Some(Output::Core(Event::Write(op))) = next(&mut server) else {
            panic!()
        };
        server.complete_write(finish_write(op)).unwrap();
        let Some(Output::Core(Event::Finished(result))) = next(&mut server) else {
            panic!()
        };
        assert_eq!(result.result, Ok(()));
        assert!(!result.reusable);
        assert!(matches!(
            next(&mut server),
            Some(Output::Core(Event::Close(_)))
        ));
    }
}

#[test]
fn source_completion_precedes_pending_body_receipt_and_occurs_once() {
    let mut server = support::server(config());
    let Some(Output::Core(Event::Read(op))) = next(&mut server) else {
        panic!()
    };
    server
        .complete_read(fill(op, b"GET / HTTP/1.1\r\nHost: a\r\n\r\n"))
        .unwrap();
    let Some(Output::Core(Event::Request(id, _))) = next(&mut server) else {
        panic!()
    };
    server.respond(id, response(BodyLength::Known(3))).unwrap();
    let Some(Output::Core(Event::Write(op))) = next(&mut server) else {
        panic!()
    };
    server.complete_write(finish_write(op)).unwrap();
    assert!(matches!(
        next(&mut server),
        Some(Output::Core(Event::Incoming(_)))
    ));
    assert!(matches!(
        next(&mut server),
        Some(Output::Core(Event::Demand(_, 3)))
    ));
    server
        .send_body(SendBody {
            exchange: id,
            buffer: b"abc".to_vec(),
            range: 0..3,
            end: true,
        })
        .unwrap();
    assert!(matches!(next(&mut server), Some(Output::SourceFinished(exchange)) if exchange == id));
    let Some(Output::Core(Event::Write(op))) = next(&mut server) else {
        panic!()
    };
    assert!(next(&mut server).is_none());
    server.complete_write(finish_write(op)).unwrap();
    let Some(Output::Core(Event::Sent(sent))) = next(&mut server) else {
        panic!()
    };
    assert_eq!(sent.buffer, b"abc");
    assert_eq!(sent.accepted, 3);
    assert!(matches!(
        next(&mut server),
        Some(Output::Core(Event::Finished(_)))
    ));
}

#[test]
fn producer_failure_finishes_source_before_retirement() {
    let mut server = support::server(config());
    let Some(Output::Core(Event::Read(op))) = next(&mut server) else {
        panic!()
    };
    server
        .complete_read(fill(op, b"GET / HTTP/1.1\r\nHost: a\r\n\r\n"))
        .unwrap();
    let Some(Output::Core(Event::Request(id, _))) = next(&mut server) else {
        panic!()
    };
    server.fail_source(id, Failure::Application).unwrap();
    assert!(matches!(next(&mut server), Some(Output::SourceFinished(exchange)) if exchange == id));
    assert!(matches!(
        next(&mut server),
        Some(Output::Core(Event::Finished(_)))
    ));
    assert!(matches!(
        next(&mut server),
        Some(Output::Core(Event::Close(_)))
    ));
}
