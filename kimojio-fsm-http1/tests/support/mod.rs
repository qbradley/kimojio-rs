#![allow(dead_code)]
use kimojio_fsm_http1::*;

pub type B = Vec<u8>;

#[derive(Debug)]
pub enum Event {
    Read(ReadOp<B>),
    Write(WriteOp<B>),
    Ready(ReadinessOp),
    Cancel(CancelOp),
    Close(CloseOp),
    Body(BodyOp<B>),
    Request(ExchangeId, Version),
    Response(ExchangeId, u16, bool),
    Trailers(Vec<(String, Vec<u8>)>),
    Incoming(ExchangeId),
    Demand(ExchangeId, usize),
    Sent(BodySent<B>),
    Finished(ExchangeFinished),
    Deadline(Option<Deadline>),
    Upgrade(ExchangeId),
    Closed(ConnectionResult),
}

pub struct Capture;

impl Ports<B> for Capture {
    type Output = Event;
    fn read(&mut self, op: ReadOp<B>) -> Option<Event> {
        Some(Event::Read(op))
    }
    fn write(&mut self, op: WriteOp<B>) -> Option<Event> {
        Some(Event::Write(op))
    }
    fn readiness(&mut self, op: ReadinessOp) -> Option<Event> {
        Some(Event::Ready(op))
    }
    fn cancel(&mut self, op: CancelOp) -> Option<Event> {
        Some(Event::Cancel(op))
    }
    fn close(&mut self, op: CloseOp) -> Option<Event> {
        Some(Event::Close(op))
    }
    fn body(&mut self, op: BodyOp<B>) -> Option<Event> {
        Some(Event::Body(op))
    }
    fn trailers(&mut self, _: ExchangeId, headers: Headers<'_>) -> Option<Event> {
        Some(Event::Trailers(
            headers
                .iter()
                .map(|h| (h.name.to_ascii_lowercase(), h.value.to_vec()))
                .collect(),
        ))
    }
    fn incoming_finished(&mut self, id: ExchangeId) -> Option<Event> {
        Some(Event::Incoming(id))
    }
    fn send_ready(&mut self, id: ExchangeId, capacity: usize) -> Option<Event> {
        Some(Event::Demand(id, capacity))
    }
    fn body_sent(&mut self, result: BodySent<B>) -> Option<Event> {
        Some(Event::Sent(result))
    }
    fn exchange_finished(&mut self, result: ExchangeFinished) -> Option<Event> {
        Some(Event::Finished(result))
    }
    fn deadline_changed(&mut self, value: Option<Deadline>) -> Option<Event> {
        Some(Event::Deadline(value))
    }
    fn upgrade_ready(&mut self, id: ExchangeId) -> Option<Event> {
        Some(Event::Upgrade(id))
    }
    fn closed(&mut self, result: ConnectionResult) -> Option<Event> {
        Some(Event::Closed(result))
    }
}

impl ServerPorts<B> for Capture {
    fn request(&mut self, id: ExchangeId, head: RequestHead<'_>) -> Option<Event> {
        Some(Event::Request(id, head.version))
    }
}

impl ClientPorts<B> for Capture {
    fn response(&mut self, id: ExchangeId, head: ResponseHead<'_>, info: bool) -> Option<Event> {
        Some(Event::Response(id, head.status, info))
    }
}

pub fn config() -> Config {
    Config {
        head_timeout_ns: None,
        body_timeout_ns: None,
        idle_timeout_ns: None,
        continue_timeout_ns: None,
        ..Config::default()
    }
}

pub fn client(config: Config) -> Client<B> {
    Client::new(
        ConnectionId {
            slot: 1,
            generation: 1,
        },
        config,
        vec![0; 1024],
        Tick(0),
    )
    .unwrap()
}

pub fn server(config: Config) -> Server<B> {
    Server::new(
        ConnectionId {
            slot: 2,
            generation: 1,
        },
        config,
        vec![0; 1024],
        Tick(0),
    )
    .unwrap()
}

pub fn get<'a>(
    method: &'a str,
    body: BodyLength,
    expect_continue: bool,
    headers: Headers<'a>,
) -> Request<'a> {
    Request {
        head: RequestHead {
            method,
            target: if method == "CONNECT" {
                "localhost:443"
            } else {
                "/"
            },
            version: Version::Http11,
            headers,
        },
        body,
        expect_continue,
    }
}

pub fn response(body: BodyLength) -> Response<'static> {
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

pub fn next_server(server: &mut Server<B>) -> Option<Event> {
    loop {
        let event = server.next(&mut Capture);
        if matches!(event, Some(Event::Deadline(_))) {
            continue;
        }
        return event;
    }
}

pub fn next_client(client: &mut Client<B>) -> Option<Event> {
    loop {
        let event = client.next(&mut Capture);
        if matches!(event, Some(Event::Deadline(_))) {
            continue;
        }
        return event;
    }
}

pub fn fill(mut op: ReadOp<B>, bytes: &[u8]) -> ReadCompletion<B> {
    op.bytes_mut()[..bytes.len()].copy_from_slice(bytes);
    op.complete(Ok(bytes.len()))
}

pub fn finish_write(op: WriteOp<B>) -> WriteCompletion<B> {
    let n = op.slices().iter().map(|s| s.len()).sum();
    op.complete(Ok(n))
}

pub fn feed_server(server: &mut Server<B>, bytes: &[u8]) -> ExchangeId {
    let Some(Event::Read(op)) = next_server(server) else {
        panic!("missing read")
    };
    server.complete_read(fill(op, bytes)).unwrap();
    let Some(Event::Request(id, _)) = next_server(server) else {
        panic!("missing request")
    };
    id
}
