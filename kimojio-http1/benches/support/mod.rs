use std::{
    cell::Cell,
    hint::black_box,
    rc::Rc,
    time::{Duration, Instant},
};

use futures::FutureExt;
use kimojio::{OwnedFdStream, operations};
use kimojio_http1::{
    Client, Config, ConnectionId, Error, IncomingBody, IncomingFrame, OutgoingBody, OutgoingFrame,
    connect, connect_native,
    http::{Method, Request, Response, StatusCode},
    serve_connection, serve_connection_native,
};

pub const IO_BYTES: usize = 16 * 1024;
pub const WARMUP: u64 = 8;
const MAX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Backend {
    Native,
    Stream,
}
impl Backend {
    pub fn name(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Stream => "stream",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Exchange,
    Forward,
    CopyForward,
}

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub backend: Backend,
    pub deadlines: bool,
    pub coalesce: bool,
    pub mode: Mode,
}
impl Options {
    pub fn new(backend: Backend) -> Self {
        Self {
            backend,
            deadlines: true,
            coalesce: false,
            mode: Mode::Exchange,
        }
    }
}

#[derive(Clone)]
pub struct Fixture {
    request: Rc<[u8]>,
    response: Rc<[u8]>,
    chunked: bool,
    chunk_bytes: usize,
}
impl Fixture {
    pub fn new(bytes: usize, chunked: bool, chunk_bytes: usize) -> Self {
        assert!(bytes <= MAX_BYTES && (1..=IO_BYTES).contains(&chunk_bytes));
        Self {
            request: (0..bytes)
                .map(|i| ((i % 251 * 31 + 17) % 251) as u8)
                .collect(),
            response: (0..bytes)
                .map(|i| ((i % 251 * 19 + 7) % 251) as u8)
                .collect(),
            chunked,
            chunk_bytes,
        }
    }
    pub fn bytes(&self) -> usize {
        self.request.len()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Counts {
    pub exchanges: u64,
    pub bytes: u64,
    pub frames: u64,
}
impl Counts {
    fn received(&mut self, bytes: usize, frames: u64) {
        self.exchanges += 1;
        self.bytes += bytes as u64;
        self.frames += frames;
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Diagnostics {
    pub reads: u64,
    pub writes: u64,
    pub deadlines: u64,
    pub body_deliveries: u64,
    pub receipts: u64,
    pub retired: u64,
}

#[derive(Debug)]
pub struct Outcome {
    pub elapsed: Duration,
    pub measured: u64,
    pub server: Counts,
    pub client: Counts,
    pub diagnostics: Option<Diagnostics>,
}

fn config(slot: u64, options: Options, diagnostics: Option<Rc<Cell<Diagnostics>>>) -> Config {
    let mut config = Config::new(ConnectionId {
        slot,
        generation: 1,
    });
    config.protocol.max_requests = u64::MAX;
    config.protocol.max_buffer_bytes = IO_BYTES;
    config.protocol.max_body_bytes = MAX_BYTES as u64;
    // Also admit the tiny-frame qualification cases without changing framing.
    config.protocol.max_chunk_metadata_bytes = 256 * 1024 * 1024;
    config.coalesce_full_bodies = options.coalesce;
    config.observation = diagnostics.map(|counts| {
        kimojio_http1::Observation::with_logger(move |_, _, event| {
            use kimojio_fsm_http1::{LogEvent, OperationKind};
            let mut value = counts.get();
            match event {
                LogEvent::OperationIssued(id) => match id.kind() {
                    OperationKind::Read => value.reads += 1,
                    OperationKind::Write => value.writes += 1,
                    _ => {}
                },
                LogEvent::DeadlineChanged(_) => value.deadlines += 1,
                LogEvent::BodyOffered { .. } => value.body_deliveries += 1,
                LogEvent::BodyReturned { .. } => value.receipts += 1,
                LogEvent::ExchangeFinished(_) => value.retired += 1,
                _ => {}
            }
            counts.set(value);
        })
    });
    if !options.deadlines {
        config.protocol.head_timeout_ns = None;
        config.protocol.body_timeout_ns = None;
        config.protocol.idle_timeout_ns = None;
        config.protocol.continue_timeout_ns = None;
    }
    config
}

fn source(bytes: Rc<[u8]>, fixture: &Fixture) -> OutgoingBody {
    if !fixture.chunked && bytes.len() <= fixture.chunk_bytes {
        return OutgoingBody::full(bytes.as_ref().to_vec());
    }
    let length = (!fixture.chunked).then_some(bytes.len() as u64);
    let chunk_bytes = fixture.chunk_bytes;
    OutgoingBody::from_stream(
        length,
        futures::stream::unfold((bytes, 0), move |(bytes, at)| async move {
            if at == bytes.len() {
                return None;
            }
            let end = (at + chunk_bytes).min(bytes.len());
            let frame = OutgoingFrame::Data(bytes[at..end].to_vec());
            Some((Ok(frame), (bytes, end)))
        }),
    )
}

pub fn check_chunk<const CHECK: bool>(
    expected: &[u8],
    bytes: &[u8],
    at: &mut usize,
) -> Result<(), Error> {
    let end = at.checked_add(bytes.len()).ok_or(Error::Limit)?;
    if end > expected.len() || (CHECK && expected[*at..end] != *bytes) {
        return Err(Error::Application("benchmark payload mismatch".into()));
    }
    *at = end;
    Ok(())
}

async fn receive<const CHECK: bool>(
    body: &mut IncomingBody,
    expected: &[u8],
) -> Result<u64, Error> {
    let mut at = 0;
    let mut frames = 0;
    while let Some(frame) = body.frame().await? {
        match frame {
            IncomingFrame::Data(chunk) => {
                check_chunk::<CHECK>(expected, black_box(chunk.as_ref()), &mut at)?;
                frames += 1;
            }
            IncomingFrame::Trailers(_) => {
                return Err(Error::Application("unexpected trailers".into()));
            }
        }
    }
    if at != expected.len() {
        return Err(Error::Application("incomplete benchmark body".into()));
    }
    Ok(frames)
}

fn forward<const CHECK: bool>(
    incoming: IncomingBody,
    expected: Rc<[u8]>,
    counts: Rc<Cell<Counts>>,
    copy: bool,
) -> OutgoingBody {
    OutgoingBody::from_stream(
        None,
        futures::stream::try_unfold(
            (incoming, expected, counts, 0usize, 0u64),
            move |(mut incoming, expected, counts, mut at, mut frames)| async move {
                match incoming.frame().await? {
                    Some(IncomingFrame::Data(chunk)) => {
                        check_chunk::<CHECK>(&expected, black_box(chunk.as_ref()), &mut at)?;
                        frames += 1;
                        let frame = if copy {
                            OutgoingFrame::Data(chunk.to_vec())
                        } else {
                            OutgoingFrame::Forward(chunk)
                        };
                        Ok(Some((frame, (incoming, expected, counts, at, frames))))
                    }
                    Some(IncomingFrame::Trailers(_)) => {
                        Err(Error::Application("unexpected trailers".into()))
                    }
                    None => {
                        if at != expected.len() {
                            return Err(Error::Application("incomplete benchmark upload".into()));
                        }
                        let mut value = counts.get();
                        value.received(at, frames);
                        counts.set(value);
                        Ok(None)
                    }
                }
            },
        ),
    )
    .continue_request_body()
}

async fn exchange<const CHECK: bool>(
    client: &mut Client,
    fixture: &Fixture,
    mode: Mode,
) -> Result<u64, Error> {
    let request = Request::builder()
        .method(Method::POST)
        .uri("/bench")
        .header("host", "benchmark")
        .header("content-type", "application/octet-stream")
        .body(source(fixture.request.clone(), fixture))
        .unwrap();
    let mut response = client.send(request).await?;
    if response.status() != StatusCode::OK
        || response.headers()["content-type"] != "application/octet-stream"
    {
        return Err(Error::Application("unexpected response head".into()));
    }
    let expected = if mode == Mode::Exchange {
        &fixture.response
    } else {
        &fixture.request
    };
    receive::<CHECK>(response.body_mut(), expected).await
}

async fn pair<const CHECK: bool>(
    fixture: Fixture,
    options: Options,
    iterations: u64,
) -> Result<Outcome, Error> {
    let total = iterations.checked_add(WARMUP).ok_or(Error::Limit)?;
    let expected_bytes = total
        .checked_mul(fixture.bytes() as u64)
        .ok_or(Error::Limit)?;
    if iterations == 0 {
        return Err(Error::Limit);
    }
    // Exactly one established socket pair per batch. There is no reconnect path.
    let (client_fd, server_fd) = rustix::net::socketpair(
        rustix::net::AddressFamily::UNIX,
        rustix::net::SocketType::STREAM,
        rustix::net::SocketFlags::CLOEXEC,
        None,
    )
    .map_err(Error::Transport)?;
    // Diagnostic logging is qualification-only, never part of timed runs.
    let diagnostics = Rc::new(Cell::new(Diagnostics::default()));
    let client_config = config(1, options, CHECK.then(|| diagnostics.clone()));
    let server_config = config(2, options, CHECK.then(|| diagnostics.clone()));
    let (mut client, driver) = match options.backend {
        Backend::Native => {
            let (c, driver) = connect_native(client_fd, client_config);
            (c, driver.run().boxed_local())
        }
        Backend::Stream => {
            let (c, driver) = connect(OwnedFdStream::new(client_fd), client_config);
            (c, driver.run().boxed_local())
        }
    };
    let server_counts = Rc::new(Cell::new(Counts::default()));
    let server = {
        let fixture = fixture.clone();
        let counts = server_counts.clone();
        let handler = move |mut request: Request<IncomingBody>| {
            let fixture = fixture.clone();
            let counts = counts.clone();
            async move {
                if request.method() != Method::POST
                    || request.uri() != "/bench"
                    || request.headers()["host"] != "benchmark"
                {
                    return Err(Error::Application("unexpected request head".into()));
                }
                let body = match options.mode {
                    Mode::Exchange => {
                        let frames = receive::<CHECK>(request.body_mut(), &fixture.request).await?;
                        let mut value = counts.get();
                        value.received(fixture.bytes(), frames);
                        counts.set(value);
                        source(fixture.response.clone(), &fixture)
                    }
                    Mode::Forward | Mode::CopyForward => {
                        request.body_mut().accept().await?;
                        forward::<CHECK>(
                            request.into_body(),
                            fixture.request.clone(),
                            counts,
                            options.mode == Mode::CopyForward,
                        )
                    }
                };
                Ok(Response::builder()
                    .header("content-type", "application/octet-stream")
                    .body(body)
                    .unwrap())
            }
        };
        match options.backend {
            Backend::Native => {
                serve_connection_native(server_fd, server_config, handler).boxed_local()
            }
            Backend::Stream => {
                serve_connection(OwnedFdStream::new(server_fd), server_config, handler)
                    .boxed_local()
            }
        }
    };
    let start = Rc::new(Cell::new(None));
    let application = {
        let start = start.clone();
        async move {
            let mut counts = Counts::default();
            let result = async {
                for _ in 0..WARMUP {
                    let frames = exchange::<CHECK>(&mut client, &fixture, options.mode).await?;
                    counts.received(fixture.bytes(), frames);
                }
                start.set(Some(Instant::now()));
                for _ in 0..iterations {
                    let frames = exchange::<CHECK>(&mut client, &fixture, options.mode).await?;
                    counts.received(fixture.bytes(), frames);
                }
                Ok::<_, Error>(counts)
            }
            .await;
            if result.is_err() {
                client.control().abort();
            }
            let shutdown = client.shutdown().await;
            result.and_then(|counts| shutdown.map(|()| counts))
        }
    };
    let (application, driver, server) = futures::join!(application, driver, server);
    // Include successful shutdown/settlement once per batch, not per exchange.
    let end = Instant::now();
    let client_counts = application?;
    driver?;
    server?;
    let server_counts = server_counts.get();
    for counts in [client_counts, server_counts] {
        if counts.exchanges != total || counts.bytes != expected_bytes {
            return Err(Error::Application("benchmark count mismatch".into()));
        }
    }
    if CHECK && diagnostics.get().retired != total.saturating_mul(2) {
        return Err(Error::Application(
            "benchmark retirement count mismatch".into(),
        ));
    }
    Ok(Outcome {
        elapsed: end.duration_since(start.get().ok_or(Error::Closed)?),
        measured: iterations,
        client: client_counts,
        server: server_counts,
        diagnostics: CHECK.then(|| diagnostics.get()),
    })
}

pub fn run<const CHECK: bool>(fixture: &Fixture, options: Options, iterations: u64) -> Outcome {
    let fixture = fixture.clone();
    match kimojio::run(0, async move {
        operations::timeout_at(
            kimojio::clock_now() + Duration::from_secs(120),
            pair::<CHECK>(fixture, options, iterations),
        )
        .await
        .map_err(|_| Error::Application("benchmark settlement watchdog expired".into()))?
    }) {
        Some(Ok(Ok(outcome))) => outcome,
        Some(Ok(Err(error))) => panic!("invalid wrapper benchmark: {error}"),
        Some(Err(panic)) => std::panic::resume_unwind(panic),
        None => panic!("runtime stopped without settling the wrapper benchmark"),
    }
}
