use kimojio_fsm_http1::*;
use std::hint::black_box;
use std::io::Write;

pub const IO_BYTES: usize = 16 * 1024;
const REQUEST_HEADERS: &[Header<'static>] = &[
    Header {
        name: "host",
        value: b"benchmark",
    },
    Header {
        name: "content-type",
        value: b"application/octet-stream",
    },
];
const RESPONSE_HEADERS: &[Header<'static>] = &[Header {
    name: "content-type",
    value: b"application/octet-stream",
}];

pub struct Scenario {
    request_body: Vec<u8>,
    response_body: Vec<u8>,
    request_wire: Vec<u8>,
    response_wire: Vec<u8>,
    chunk_bytes: usize,
    streaming: bool,
}

impl Scenario {
    pub fn fixed(bytes: usize) -> Self {
        Self::new(bytes, IO_BYTES, false)
    }

    pub fn chunked(bytes: usize, chunk_bytes: usize) -> Self {
        Self::new(bytes, chunk_bytes, true)
    }

    fn new(bytes: usize, chunk_bytes: usize, streaming: bool) -> Self {
        assert!(bytes > 0, "a benchmark exchange must contain payload");
        assert!(
            (1..=IO_BYTES).contains(&chunk_bytes),
            "invalid benchmark chunk size"
        );
        let request_body: Vec<_> = (0..bytes)
            .map(|i| ((i % 251 * 31 + 17) % 251) as u8)
            .collect();
        let response_body: Vec<_> = (0..bytes)
            .map(|i| ((i % 251 * 19 + 7) % 251) as u8)
            .collect();
        let request_wire = wire(
            b"POST /bench HTTP/1.1\r\nhost: benchmark\r\ncontent-type: application/octet-stream\r\n",
            &request_body, chunk_bytes, streaming,
        );
        let response_wire = wire(
            b"HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\n",
            &response_body,
            chunk_bytes,
            streaming,
        );
        Self {
            request_body,
            response_body,
            request_wire,
            response_wire,
            chunk_bytes,
            streaming,
        }
    }

    pub fn body_bytes(&self) -> usize {
        self.request_body.len()
    }

    fn length(&self) -> BodyLength {
        if self.streaming {
            BodyLength::Streaming
        } else {
            BodyLength::Known(self.body_bytes() as u64)
        }
    }

    fn request(&self) -> Request<'static> {
        Request {
            head: RequestHead {
                method: "POST",
                target: "/bench",
                version: Version::Http11,
                headers: REQUEST_HEADERS,
            },
            body: self.length(),
            expect_continue: false,
        }
    }

    fn response(&self) -> Response<'static> {
        Response::new(200, "OK", RESPONSE_HEADERS, self.length())
    }
}

fn wire(head: &[u8], body: &[u8], chunk_bytes: usize, streaming: bool) -> Vec<u8> {
    let mut wire = head.to_vec();
    if streaming {
        wire.extend_from_slice(b"transfer-encoding: chunked\r\n\r\n");
        for chunk in body.chunks(chunk_bytes) {
            write!(wire, "{:x}\r\n", chunk.len()).unwrap();
            wire.extend_from_slice(chunk);
            wire.extend_from_slice(b"\r\n");
        }
        wire.extend_from_slice(b"0\r\n\r\n");
    } else {
        write!(wire, "content-length: {}\r\n\r\n", body.len()).unwrap();
        wire.extend_from_slice(body);
    }
    wire
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Stats {
    pub reads: usize,
    pub writes: usize,
    pub received: usize,
    pub sent: usize,
    pub body_deliveries: usize,
    pub receipts: usize,
    pub heads: usize,
    pub incoming: usize,
    pub sources: usize,
    pub trailers: usize,
}

pub struct Transport<'a, const YIELD: bool, const CHECK: bool> {
    input: &'a [u8],
    incoming_body: &'a [u8],
    output: &'a [u8],
    outgoing_body: &'a [u8],
    read_at: usize,
    write_at: usize,
    send_at: usize,
    read_limit: usize,
    write_limit: usize,
    read: Option<ReadOp<Vec<u8>>>,
    write: Option<WriteCompletion<&'a [u8]>>,
    body: Option<(BodyCompletion<Vec<u8>>, ExchangeId, usize)>,
    head: Option<ExchangeId>,
    demand: Option<(ExchangeId, usize)>,
    incoming: Option<ExchangeId>,
    finished: Option<ExchangeFinished>,
    exchange: Option<ExchangeId>,
    stats: Stats,
}

impl<'a, const YIELD: bool, const CHECK: bool> Transport<'a, YIELD, CHECK> {
    fn new(scenario: &'a Scenario, server: bool) -> Self {
        let (input, incoming_body, output, outgoing_body) = if server {
            (
                &scenario.request_wire,
                &scenario.request_body,
                &scenario.response_wire,
                &scenario.response_body,
            )
        } else {
            (
                &scenario.response_wire,
                &scenario.response_body,
                &scenario.request_wire,
                &scenario.request_body,
            )
        };
        Self {
            input,
            incoming_body,
            output,
            outgoing_body,
            read_at: 0,
            write_at: 0,
            send_at: 0,
            read_limit: IO_BYTES,
            write_limit: usize::MAX,
            read: None,
            write: None,
            body: None,
            head: None,
            demand: None,
            incoming: None,
            finished: None,
            exchange: None,
            stats: Stats::default(),
        }
    }

    fn callback() -> Option<()> {
        YIELD.then_some(())
    }

    fn observe_head(&mut self, exchange: ExchangeId) -> Option<()> {
        if let Some(current) = self.exchange {
            assert_eq!(current, exchange);
        }
        self.exchange = Some(exchange);
        assert!(self.head.replace(exchange).is_none());
        self.stats.heads += 1;
        Self::callback()
    }

    fn begin(&mut self) {
        assert!(self.write.is_none() && self.body.is_none() && self.head.is_none());
        assert!(self.demand.is_none() && self.incoming.is_none() && self.finished.is_none());
        // A reusable server can already own the read for the following request.
        self.read_at = 0;
        self.write_at = 0;
        self.send_at = 0;
        self.exchange = None;
        self.stats = Stats::default();
    }
}

impl<'a, const YIELD: bool, const CHECK: bool> Ports<Vec<u8>, &'a [u8]>
    for Transport<'a, YIELD, CHECK>
{
    type Output = ();
    fn read(&mut self, op: ReadOp<Vec<u8>>) -> Option<()> {
        assert!(self.read.replace(op).is_none(), "overlapping reads");
        Self::callback()
    }
    fn write(&mut self, op: WriteOp<&'a [u8]>) -> Option<()> {
        let slices = black_box(op.slices());
        let count = slices
            .iter()
            .map(|part| part.len())
            .sum::<usize>()
            .min(self.write_limit);
        assert!(count > 0 && self.write_at + count <= self.output.len());
        if CHECK {
            let mut remaining = count;
            let mut at = self.write_at;
            for part in slices {
                let n = part.len().min(remaining);
                assert_eq!(
                    &part[..n],
                    &self.output[at..at + n],
                    "incorrect outgoing wire"
                );
                at += n;
                remaining -= n;
            }
        }
        self.write_at += count;
        self.stats.writes += 1;
        assert!(self.write.replace(op.complete(Ok(count))).is_none());
        Self::callback()
    }
    fn readiness(&mut self, _: ReadinessOp) -> Option<()> {
        panic!("the simulated transport never blocks")
    }
    fn cancel(&mut self, _: CancelOp) -> Option<()> {
        panic!("unexpected cancellation in a successful round trip")
    }
    fn close(&mut self, _: CloseOp) -> Option<()> {
        panic!("benchmark connection was not reusable")
    }
    fn body(&mut self, op: BodyOp<Vec<u8>>) -> Option<()> {
        let bytes = black_box(op.bytes());
        let count = bytes.len();
        if CHECK {
            assert_eq!(
                bytes,
                &self.incoming_body[self.stats.received..self.stats.received + count]
            );
        }
        let exchange = op.exchange();
        assert_eq!(self.exchange, Some(exchange));
        self.stats.received += count;
        self.stats.body_deliveries += 1;
        assert!(
            self.body
                .replace((op.release(count), exchange, count))
                .is_none()
        );
        Self::callback()
    }
    fn trailers(&mut self, _: ExchangeId, trailers: Headers<'_>) -> Option<()> {
        assert!(trailers.is_empty());
        self.stats.trailers += 1;
        Self::callback()
    }
    fn incoming_finished(&mut self, exchange: ExchangeId) -> Option<()> {
        assert_eq!(self.exchange, Some(exchange));
        assert!(self.incoming.replace(exchange).is_none());
        self.stats.incoming += 1;
        Self::callback()
    }
    fn send_ready(&mut self, exchange: ExchangeId, capacity: usize) -> Option<()> {
        assert_eq!(self.exchange, Some(exchange));
        assert!(self.demand.replace((exchange, capacity)).is_none());
        Self::callback()
    }
    fn source_finished(&mut self, exchange: ExchangeId) -> Option<()> {
        assert_eq!(self.exchange, Some(exchange));
        self.stats.sources += 1;
        Self::callback()
    }
    fn body_sent(&mut self, receipt: BodySent<&'a [u8]>) -> Option<()> {
        assert_eq!(self.exchange, Some(receipt.exchange));
        assert_eq!(receipt.result, Ok(()));
        assert_eq!(receipt.acceptance, Acceptance::Exact);
        assert_eq!(receipt.accepted, receipt.buffer.len());
        if CHECK {
            assert_eq!(
                receipt.buffer.as_ptr(),
                self.outgoing_body[self.stats.sent..].as_ptr()
            );
        }
        self.stats.sent += receipt.accepted;
        self.stats.receipts += 1;
        Self::callback()
    }
    fn exchange_finished(&mut self, finished: ExchangeFinished) -> Option<()> {
        assert_eq!(self.exchange, Some(finished.exchange));
        assert!(self.finished.replace(finished).is_none());
        Self::callback()
    }
    fn deadline_changed(&mut self, deadline: Option<Deadline>) -> Option<()> {
        assert!(deadline.is_none(), "benchmark timers are disabled");
        Self::callback()
    }
    fn upgrade_ready(&mut self, _: ExchangeId) -> Option<()> {
        panic!("unexpected protocol upgrade")
    }
    fn closed(&mut self, result: ConnectionResult) -> Option<()> {
        panic!("unexpected connection termination: {result:?}")
    }
}

impl<'a, const YIELD: bool, const CHECK: bool> ClientPorts<Vec<u8>, &'a [u8]>
    for Transport<'a, YIELD, CHECK>
{
    fn response(
        &mut self,
        exchange: ExchangeId,
        head: ResponseHead<'_>,
        informational: bool,
    ) -> Option<()> {
        let head = black_box(head);
        assert_eq!(head.status, 200);
        assert!(!informational);
        if CHECK {
            assert_eq!(head.version, Version::Http11);
            assert_eq!(head.reason, "OK");
            assert_eq!(head.headers.len(), 2);
        }
        self.observe_head(exchange)
    }
}

impl<'a, const YIELD: bool, const CHECK: bool> ServerPorts<Vec<u8>, &'a [u8]>
    for Transport<'a, YIELD, CHECK>
{
    fn request(&mut self, exchange: ExchangeId, head: RequestHead<'_>) -> Option<()> {
        let head = black_box(head);
        if CHECK {
            assert_eq!(head.method, "POST");
            assert_eq!(head.target, "/bench");
            assert_eq!(head.version, Version::Http11);
            assert_eq!(head.headers.len(), 3);
        }
        self.observe_head(exchange)
    }
}

// Static forwarding shares the simulated executor without an enum containing
// large operations or a dynamic-dispatch cost in each completion.
pub trait Endpoint<'a> {
    const SERVER: bool;
    fn next<const YIELD: bool, const CHECK: bool>(
        &mut self,
        ports: &mut Transport<'a, YIELD, CHECK>,
    ) -> Option<()>;
    fn begin(&mut self, scenario: &Scenario) -> Option<ExchangeId>;
    fn incoming(&mut self, exchange: ExchangeId, scenario: &Scenario);
    fn read(&mut self, completion: ReadCompletion<Vec<u8>>);
    fn write(&mut self, completion: WriteCompletion<&'a [u8]>);
    fn release(&mut self, completion: BodyCompletion<Vec<u8>>);
    fn credit(&mut self, exchange: ExchangeId, bytes: usize);
    fn send(&mut self, body: SendBody<&'a [u8]>);
}

impl<'a> Endpoint<'a> for Client<Vec<u8>, &'a [u8]> {
    const SERVER: bool = false;
    fn next<const YIELD: bool, const CHECK: bool>(
        &mut self,
        ports: &mut Transport<'a, YIELD, CHECK>,
    ) -> Option<()> {
        self.next(ports)
    }
    fn begin(&mut self, scenario: &Scenario) -> Option<ExchangeId> {
        Some(self.request(scenario.request()).unwrap())
    }
    fn incoming(&mut self, _: ExchangeId, _: &Scenario) {}
    fn read(&mut self, completion: ReadCompletion<Vec<u8>>) {
        self.complete_read(completion).unwrap();
    }
    fn write(&mut self, completion: WriteCompletion<&'a [u8]>) {
        self.complete_write(completion).unwrap();
    }
    fn release(&mut self, completion: BodyCompletion<Vec<u8>>) {
        self.release_body(completion).unwrap();
    }
    fn credit(&mut self, exchange: ExchangeId, bytes: usize) {
        self.grant_body_credit(exchange, bytes).unwrap();
    }
    fn send(&mut self, body: SendBody<&'a [u8]>) {
        self.send_body(body).unwrap();
    }
}

impl<'a> Endpoint<'a> for Server<Vec<u8>, &'a [u8]> {
    const SERVER: bool = true;
    fn next<const YIELD: bool, const CHECK: bool>(
        &mut self,
        ports: &mut Transport<'a, YIELD, CHECK>,
    ) -> Option<()> {
        self.next(ports)
    }
    fn begin(&mut self, _: &Scenario) -> Option<ExchangeId> {
        None
    }
    fn incoming(&mut self, exchange: ExchangeId, scenario: &Scenario) {
        self.respond(exchange, scenario.response()).unwrap();
    }
    fn read(&mut self, completion: ReadCompletion<Vec<u8>>) {
        self.complete_read(completion).unwrap();
    }
    fn write(&mut self, completion: WriteCompletion<&'a [u8]>) {
        self.complete_write(completion).unwrap();
    }
    fn release(&mut self, completion: BodyCompletion<Vec<u8>>) {
        self.release_body(completion).unwrap();
    }
    fn credit(&mut self, exchange: ExchangeId, bytes: usize) {
        self.grant_body_credit(exchange, bytes).unwrap();
    }
    fn send(&mut self, body: SendBody<&'a [u8]>) {
        self.send_body(body).unwrap();
    }
}

pub struct Session<'a, M, const YIELD: bool, const CHECK: bool> {
    machine: M,
    scenario: &'a Scenario,
    transport: Transport<'a, YIELD, CHECK>,
}

fn config(scenario: &Scenario) -> Config {
    Config {
        max_buffer_bytes: IO_BYTES,
        max_body_bytes: scenario.body_bytes() as u64,
        max_requests: u64::MAX,
        head_timeout_ns: None,
        body_timeout_ns: None,
        idle_timeout_ns: None,
        continue_timeout_ns: None,
        ..Config::default()
    }
}

pub fn client<const YIELD: bool, const CHECK: bool>(
    scenario: &Scenario,
) -> Session<'_, Client<Vec<u8>, &[u8]>, YIELD, CHECK> {
    Session {
        machine: Client::with_output_type(
            ConnectionId {
                slot: 1,
                generation: 1,
            },
            config(scenario),
            vec![0; IO_BYTES],
            Tick(0),
        )
        .unwrap(),
        scenario,
        transport: Transport::new(scenario, false),
    }
}

pub fn server<const YIELD: bool, const CHECK: bool>(
    scenario: &Scenario,
) -> Session<'_, Server<Vec<u8>, &[u8]>, YIELD, CHECK> {
    Session {
        machine: Server::with_output_type(
            ConnectionId {
                slot: 2,
                generation: 1,
            },
            config(scenario),
            vec![0; IO_BYTES],
            Tick(0),
        )
        .unwrap(),
        scenario,
        transport: Transport::new(scenario, true),
    }
}

impl<'a, M: Endpoint<'a>, const YIELD: bool, const CHECK: bool> Session<'a, M, YIELD, CHECK> {
    pub fn round_trip(&mut self) -> Stats {
        let t = &mut self.transport;
        t.begin();
        t.exchange = self.machine.begin(self.scenario);
        loop {
            let yielded = self.machine.next(t).is_some();
            let mut progress = false;
            if let Some(exchange) = t.head.take() {
                self.machine.credit(exchange, IO_BYTES);
                progress = true;
            }
            if let Some((body, exchange, count)) = t.body.take() {
                self.machine.release(body);
                self.machine.credit(exchange, count);
                progress = true;
            }
            if let Some(write) = t.write.take() {
                self.machine.write(write);
                progress = true;
            }
            if let Some(exchange) = t.incoming.take() {
                self.machine.incoming(exchange, self.scenario);
                progress = true;
            }
            if let Some((exchange, capacity)) = t.demand.take() {
                let count = (t.outgoing_body.len() - t.send_at)
                    .min(capacity)
                    .min(self.scenario.chunk_bytes);
                assert!(count > 0, "empty or excess producer demand");
                let buffer = &t.outgoing_body[t.send_at..t.send_at + count];
                t.send_at += count;
                self.machine.send(SendBody {
                    exchange,
                    buffer,
                    range: 0..count,
                    end: t.send_at == t.outgoing_body.len(),
                });
                progress = true;
            }
            // The peer sends its response only after accepting the complete
            // request, including the final chunk terminator.
            if t.read.is_some()
                && t.read_at < t.input.len()
                && (M::SERVER || t.write_at == t.output.len())
            {
                let mut read = t.read.take().unwrap();
                let count = (t.input.len() - t.read_at)
                    .min(read.bytes_mut().len())
                    .min(t.read_limit);
                assert!(count > 0);
                read.bytes_mut()[..count]
                    .copy_from_slice(black_box(&t.input[t.read_at..t.read_at + count]));
                t.read_at += count;
                t.stats.reads += 1;
                self.machine.read(read.complete(Ok(count)));
                progress = true;
            }
            if let Some(finished) = t.finished.take() {
                assert_eq!(finished.result, Ok(()));
                assert!(finished.reusable);
                assert_eq!(t.read_at, t.input.len());
                assert_eq!(t.write_at, t.output.len());
                assert_eq!(t.stats.received, t.incoming_body.len());
                assert_eq!(t.stats.sent, t.outgoing_body.len());
                assert_eq!(
                    (t.stats.heads, t.stats.incoming, t.stats.sources),
                    (1, 1, 1)
                );
                assert_eq!(t.stats.trailers, usize::from(self.scenario.streaming));
                assert_eq!(
                    t.stats.receipts,
                    t.outgoing_body.len().div_ceil(self.scenario.chunk_bytes)
                );
                return t.stats;
            }
            assert!(
                progress || yielded,
                "simulated connection stalled before exchange completion"
            );
        }
    }
}

#[cfg(test)]
impl<'a, M: Endpoint<'a>, const YIELD: bool, const CHECK: bool> Session<'a, M, YIELD, CHECK> {
    // Cargo also sets cfg(test) for the harness-free benchmark.
    #[allow(dead_code)]
    pub fn fragment(&mut self, read: usize, write: usize) {
        assert!(read > 0 && write > 0);
        self.transport.read_limit = read;
        self.transport.write_limit = write;
    }
}
