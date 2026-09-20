use std::{
    cell::{Cell, RefCell},
    future::Future,
    pin::pin,
    rc::Rc,
    task::{Poll, Waker},
    time::{Duration, Instant},
};

use futures::{future::try_join_all, stream};
use kimojio::{OwnedFdStream, operations};
use kimojio_http2::{
    Client, Config, Error, IncomingBody, IncomingFrame, OutgoingBody, OutgoingFrame, Shutdown,
    StreamId, StreamOutcome, StreamReport, connect, connect_native,
    http::{
        HeaderValue, Method, Request, Response, StatusCode, Uri, Version, header::CONTENT_LENGTH,
    },
    serve_connection_native_with_shutdown, serve_connection_with_shutdown,
};
use serde_json::{Value, json};

const BLOCK: usize = 16384;
const fn pattern(reverse: bool) -> [u8; BLOCK] {
    let mut bytes = [0; BLOCK];
    let mut index = 0;
    while index < BLOCK {
        bytes[index] = if reverse {
            250 - (index % 251) as u8
        } else {
            (index % 251) as u8
        };
        index += 1;
    }
    bytes
}
static UPLOAD: [u8; BLOCK] = pattern(false);
static DOWNLOAD: [u8; BLOCK] = pattern(true);

#[derive(Clone, Copy)]
pub enum Tag {
    Application = 1,
    Connection = 2,
}

pub trait Meter: Copy + 'static {
    fn begin(self);
    fn end(self) -> Value;
    fn cancel(self);
    fn enter<T>(self, tag: Tag, action: impl FnOnce() -> T) -> T;
}

#[derive(Clone, Copy)]
pub struct NoMeter;
impl Meter for NoMeter {
    fn begin(self) {}
    fn end(self) -> Value {
        Value::Null
    }
    fn cancel(self) {}
    #[inline(always)]
    fn enter<T>(self, _: Tag, action: impl FnOnce() -> T) -> T {
        action()
    }
}

async fn scoped<M: Meter, F: Future>(meter: M, tag: Tag, future: F) -> F::Output {
    let mut future = pin!(future);
    futures::future::poll_fn(|cx| meter.enter(tag, || future.as_mut().poll(cx))).await
}

#[derive(Clone, Debug)]
pub struct Options {
    pub backend: String,
    pub case: String,
    pub phase: String,
    pub owned: bool,
    pub concurrency: usize,
    pub cohorts: usize,
    pub warmup: usize,
    pub bytes: usize,
    pub chunk: usize,
    pub timeout_seconds: u64,
    #[cfg(test)]
    pub corrupt_response: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            backend: "native".into(),
            case: "empty".into(),
            phase: "steady".into(),
            owned: false,
            concurrency: 1,
            cohorts: 2,
            warmup: 2,
            bytes: 1024 * 1024,
            chunk: BLOCK,
            timeout_seconds: 60,
            #[cfg(test)]
            corrupt_response: false,
        }
    }
}

impl Options {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut result = Self::default();
        let mut args = args.into_iter();
        while let Some(key) = args.next() {
            if key == "--bench" || key == "--test" {
                continue;
            }
            let value = args
                .next()
                .ok_or_else(|| format!("missing value for {key}"))?;
            match key.as_str() {
                "--backend" => result.backend = value,
                "--case" => result.case = value,
                "--phase" => result.phase = value,
                "--payload" if value == "static" || value == "owned" => {
                    result.owned = value == "owned"
                }
                "--concurrency" => result.concurrency = value.parse().map_err(|_| key)?,
                "--cohorts" => result.cohorts = value.parse().map_err(|_| key)?,
                "--warmup" => result.warmup = value.parse().map_err(|_| key)?,
                "--bytes" => result.bytes = value.parse().map_err(|_| key)?,
                "--chunk" => result.chunk = value.parse().map_err(|_| key)?,
                "--timeout-seconds" => result.timeout_seconds = value.parse().map_err(|_| key)?,
                _ => return Err(format!("unknown option or value: {key} {value}")),
            }
        }
        result.validate()?;
        Ok(result)
    }

    pub fn validate(&self) -> Result<(), String> {
        if !["native", "generic"].contains(&self.backend.as_str())
            || !["empty", "fixed", "stream", "duplex"].contains(&self.case.as_str())
            || !["cold", "steady"].contains(&self.phase.as_str())
            || ![1, 8, 32, 128].contains(&self.concurrency)
            || !(1..=100000).contains(&self.cohorts)
            || self.warmup > 10000
            || !(1..=1024 * 1024).contains(&self.bytes)
            || !(1..=BLOCK).contains(&self.chunk)
            || !(1..=300).contains(&self.timeout_seconds)
            || (self.phase == "cold" && self.cohorts != 1)
            || (self.case == "duplex" && self.bytes <= self.chunk)
        {
            return Err("benchmark option is outside its documented bound".into());
        }
        Ok(())
    }

    fn sizes(&self) -> (usize, usize) {
        match self.case.as_str() {
            "empty" => (0, 128),
            "fixed" => (4096, 4096),
            _ => (self.bytes, self.bytes),
        }
    }
    fn streaming(&self) -> bool {
        self.case == "stream" || self.case == "duplex"
    }
}

#[derive(Default)]
struct Gate {
    produced: Cell<usize>,
    open: Cell<bool>,
    waiter: RefCell<Option<Waker>>,
}

impl Gate {
    fn release(&self) {
        self.open.set(true);
        let waiter = self.waiter.borrow_mut().take();
        if let Some(waker) = waiter {
            waker.wake();
        }
    }
}

struct Slot {
    uri: Uri,
    gate: Rc<Gate>,
    server_id: Cell<Option<StreamId>>,
    previous_id: Cell<Option<StreamId>>,
    server_seen: Cell<bool>,
    client_retired: Cell<bool>,
    server_retired: Cell<bool>,
}

enum PendingServer {
    Received(usize, IncomingBody),
    Draining(usize, operations::TaskHandle<Result<IncomingBody, Error>>),
}

struct Shared {
    slots: Vec<Slot>,
    pending: RefCell<Vec<PendingServer>>,
    request_length: HeaderValue,
    response_length: HeaderValue,
    witnesses: Cell<usize>,
    client_retirements: Cell<usize>,
    server_retirements: Cell<usize>,
}

impl Shared {
    fn new(options: &Options) -> Self {
        let (request, response) = options.sizes();
        Self {
            slots: (0..options.concurrency)
                .map(|index| Slot {
                    uri: format!("http://wrapper.test/bench/{index}")
                        .parse()
                        .unwrap(),
                    gate: Rc::new(Gate::default()),
                    server_id: Cell::new(None),
                    previous_id: Cell::new(None),
                    server_seen: Cell::new(false),
                    client_retired: Cell::new(false),
                    server_retired: Cell::new(false),
                })
                .collect(),
            pending: RefCell::new(Vec::with_capacity(options.concurrency)),
            request_length: request.to_string().parse().unwrap(),
            response_length: response.to_string().parse().unwrap(),
            witnesses: Cell::new(0),
            client_retirements: Cell::new(0),
            server_retirements: Cell::new(0),
        }
    }

    fn begin_cohort(&self) {
        assert!(self.pending.borrow().is_empty());
        for slot in &self.slots {
            slot.gate.produced.set(0);
            slot.gate.open.set(false);
            assert!(slot.gate.waiter.borrow().is_none());
            slot.server_id.set(None);
            slot.server_seen.set(false);
            slot.client_retired.set(false);
            slot.server_retired.set(false);
        }
    }

    fn release_producers(&self) {
        for slot in &self.slots {
            slot.gate.release();
        }
    }
}

fn failure(message: &str) -> Error {
    Error::Application(message.into())
}

pub fn compare(bytes: &[u8], offset: usize, expected: usize, upload: bool) -> Result<(), Error> {
    if offset
        .checked_add(bytes.len())
        .is_none_or(|end| end > expected)
    {
        return Err(failure("excess body payload"));
    }
    let pattern = if upload { &UPLOAD } else { &DOWNLOAD };
    let mut matched = 0;
    while matched < bytes.len() {
        let start = (offset + matched) % BLOCK;
        let count = (BLOCK - start).min(bytes.len() - matched);
        if bytes[matched..matched + count] != pattern[start..start + count] {
            return Err(failure("body payload mismatch"));
        }
        matched += count;
    }
    Ok(())
}

fn source<M: Meter>(
    options: &Options,
    upload: bool,
    gate: Option<Rc<Gate>>,
    meter: M,
) -> OutgoingBody {
    let size = if upload {
        options.sizes().0
    } else {
        options.sizes().1
    };
    let bytes = if upload { &UPLOAD } else { &DOWNLOAD };
    #[cfg(test)]
    let bytes = if !upload && options.corrupt_response {
        &UPLOAD
    } else {
        bytes
    };
    if size == 0 {
        return OutgoingBody::empty();
    }
    if !options.streaming() {
        if let Some(gate) = gate {
            gate.produced.set(size);
        }
        return if options.owned {
            OutgoingBody::full(bytes[..size].to_vec())
        } else {
            OutgoingBody::from_static(&bytes[..size])
        };
    }
    let chunk = options.chunk;
    let owned = options.owned;
    let gated = upload && options.case == "duplex";
    let mut offset = 0;
    OutgoingBody::from_stream(stream::poll_fn(move |cx| {
        meter.enter(Tag::Application, || {
            if offset == size {
                return Poll::Ready(None);
            }
            if gated && offset > 0 && !gate.as_ref().unwrap().open.get() {
                *gate.as_ref().unwrap().waiter.borrow_mut() = Some(cx.waker().clone());
                return Poll::Pending;
            }
            let start = offset % BLOCK;
            let count = chunk.min(size - offset).min(BLOCK - start);
            let slice = &bytes[start..start + count];
            offset += count;
            if let Some(gate) = &gate {
                gate.produced.set(offset);
            }
            Poll::Ready(Some(Ok(if owned {
                OutgoingFrame::Data(slice.to_vec())
            } else {
                OutgoingFrame::Static(slice)
            })))
        })
    }))
}

async fn receive(
    mut body: IncomingBody,
    expected: usize,
    upload: bool,
    witness: Option<(&Shared, usize, &Options)>,
) -> Result<IncomingBody, Error> {
    let mut received = 0;
    while let Some(frame) = body.frame().await? {
        let IncomingFrame::Data(chunk) = frame else {
            return Err(failure("unexpected trailers"));
        };
        compare(&chunk, received, expected, upload)?;
        if received == 0
            && !chunk.is_empty()
            && let Some((shared, index, options)) = witness
        {
            let gate = &shared.slots[index].gate;
            let early = gate.produced.get() < options.sizes().0;
            if options.case == "duplex" && (!early || gate.open.get()) {
                return Err(failure(
                    "response payload did not precede the withheld upload tail",
                ));
            }
            if early {
                shared.witnesses.set(shared.witnesses.get() + 1);
            }
            gate.release();
        }
        received += chunk.len();
        drop(chunk);
    }
    if received != expected || body.receive_outcome() != Some(StreamOutcome::Complete) {
        return Err(failure("receive EOF did not complete the exact payload"));
    }
    Ok(body)
}

fn retirement(report: &StreamReport, id: StreamId) -> Result<(), Error> {
    if report.stream != id
        || report.outcome != StreamOutcome::Complete
        || report.receive_outcome != Some(StreamOutcome::Complete)
        || report.send_failure.is_some()
        || report.error.is_some()
    {
        return Err(failure("stream did not retire successfully"));
    }
    Ok(())
}

async fn handler<M: Meter>(
    request: Request<IncomingBody>,
    shared: Rc<Shared>,
    options: Rc<Options>,
    meter: M,
) -> Result<Response<OutgoingBody>, Error> {
    if request.method() != Method::POST
        || request.version() != Version::HTTP_2
        || request.uri().authority().map(|a| a.as_str()) != Some("wrapper.test")
        || request.headers().get(CONTENT_LENGTH) != Some(&shared.request_length)
    {
        return Err(failure("request metadata mismatch"));
    }
    let index: usize = request
        .uri()
        .path()
        .strip_prefix("/bench/")
        .and_then(|value| value.parse().ok())
        .filter(|index| *index < options.concurrency)
        .ok_or_else(|| failure("request slot mismatch"))?;
    let slot = &shared.slots[index];
    if slot.server_seen.replace(true) {
        return Err(failure("duplicate request in cohort"));
    }
    slot.server_id.set(Some(request.body().stream_id()));
    let bytes = options.sizes().0;
    let pending = if options.streaming() {
        let body = request.into_body();
        PendingServer::Draining(
            index,
            operations::spawn_task(scoped(meter, Tag::Application, async move {
                receive(body, bytes, true, None).await
            })),
        )
    } else {
        PendingServer::Received(
            index,
            receive(request.into_body(), bytes, true, None).await?,
        )
    };
    shared.pending.borrow_mut().push(pending);
    let mut response = Response::new(source(&options, false, None, meter));
    *response.version_mut() = Version::HTTP_2;
    response
        .headers_mut()
        .insert(CONTENT_LENGTH, shared.response_length.clone());
    Ok(response)
}

async fn exchange<M: Meter>(
    client: &Client,
    shared: &Shared,
    options: &Options,
    index: usize,
    meter: M,
) -> Result<(), Error> {
    let slot = &shared.slots[index];
    let mut request = Request::new(source(options, true, Some(slot.gate.clone()), meter));
    *request.method_mut() = Method::POST;
    *request.version_mut() = Version::HTTP_2;
    *request.uri_mut() = slot.uri.clone();
    request
        .headers_mut()
        .insert(CONTENT_LENGTH, shared.request_length.clone());
    let response = client.send(request).await?;
    if response.status() != StatusCode::OK
        || response.version() != Version::HTTP_2
        || response.headers().get(CONTENT_LENGTH) != Some(&shared.response_length)
    {
        return Err(failure("response metadata mismatch"));
    }
    let id = response.body().stream_id();
    if slot.server_id.get() != Some(id)
        || slot
            .previous_id
            .get()
            .is_some_and(|old| old.get() >= id.get())
    {
        return Err(failure("stream identity was mismatched or reused"));
    }
    let mut body = receive(
        response.into_body(),
        options.sizes().1,
        false,
        Some((shared, index, options)),
    )
    .await?;
    retirement(&body.retirement().await?, id)?;
    slot.previous_id.set(Some(id));
    assert!(!slot.client_retired.replace(true));
    shared
        .client_retirements
        .set(shared.client_retirements.get() + 1);
    Ok(())
}

async fn settle_server(shared: &Shared, successful: bool) -> Result<(), Error> {
    let mut first_error = None;
    loop {
        let next = shared.pending.borrow_mut().pop();
        let Some(pending) = next else { break };
        let (index, result) = match pending {
            PendingServer::Received(index, body) => (index, Ok(body)),
            PendingServer::Draining(index, task) => (
                index,
                task.await
                    .map_err(|_| failure("request-drain task failed"))
                    .and_then(|r| r),
            ),
        };
        let result = async {
            let mut body = result?;
            let report = body.retirement().await?;
            if successful {
                retirement(&report, shared.slots[index].server_id.get().unwrap())?;
                assert!(!shared.slots[index].server_retired.replace(true));
                shared
                    .server_retirements
                    .set(shared.server_retirements.get() + 1);
            }
            Ok::<_, Error>(())
        }
        .await;
        if first_error.is_none() {
            first_error = result.err();
        }
    }
    first_error.map_or(Ok(()), Err)
}

async fn cohort<M: Meter>(
    client: &Client,
    shared: &Shared,
    options: &Options,
    meter: M,
) -> Result<(), Error> {
    shared.begin_cohort();
    if options.concurrency == 1 {
        exchange(client, shared, options, 0, meter).await?;
    } else {
        try_join_all((0..options.concurrency).map(|i| exchange(client, shared, options, i, meter)))
            .await?;
    }
    settle_server(shared, true).await?;
    for slot in &shared.slots {
        if !slot.server_seen.get() || !slot.client_retired.get() || !slot.server_retired.get() {
            return Err(failure("cohort lacks authoritative retirement"));
        }
    }
    Ok(())
}

fn configuration(options: &Options) -> Config {
    let mut config = Config {
        max_queued_requests: options.concurrency,
        ..Config::default()
    };
    config.protocol.http = config.protocol.http.set_max_active_streams(128);
    config
}

fn cpu_ns() -> u128 {
    let time = rustix::time::clock_gettime(rustix::time::ClockId::ProcessCPUTime);
    time.tv_sec as u128 * 1_000_000_000 + time.tv_nsec as u128
}

struct Start {
    wall: Instant,
    cpu: u128,
}
impl Start {
    fn now() -> Self {
        Self {
            wall: Instant::now(),
            cpu: cpu_ns(),
        }
    }
    fn end<M: Meter>(self, meter: M) -> Measurement {
        let elapsed_ns = self.wall.elapsed().as_nanos();
        let cpu_ns = cpu_ns() - self.cpu;
        Measurement {
            elapsed_ns,
            cpu_ns,
            allocations: meter.end(),
        }
    }
}

struct Measurement {
    elapsed_ns: u128,
    cpu_ns: u128,
    allocations: Value,
}

async fn drive<M: Meter>(
    client: Client,
    connection: impl Future<Output = Result<(), Error>>,
    server: impl Future<Output = Result<(), Error>>,
    server_control: Shutdown,
    shared: Rc<Shared>,
    options: &Options,
    meter: M,
) -> Result<Option<Measurement>, Error> {
    let abort_client = client.control();
    let abort_server = server_control.clone();
    let app = async {
        let result = async {
            if options.phase == "steady" {
                for _ in 0..options.warmup {
                    cohort(&client, &shared, options, meter).await?;
                }
                meter.begin();
            }
            let start = Start::now();
            for _ in 0..options.cohorts {
                cohort(&client, &shared, options, meter).await?;
            }
            let measurement = (options.phase == "steady").then(|| start.end(meter));
            Ok::<_, Error>(measurement)
        }
        .await;
        if result.is_ok() {
            client.control().graceful();
        } else {
            client.control().abort();
            server_control.abort();
            shared.release_producers();
        }
        result
    };
    let joined = async {
        futures::join!(
            scoped(meter, Tag::Application, app),
            scoped(meter, Tag::Connection, connection),
            scoped(meter, Tag::Connection, server),
        )
    };
    let mut joined = pin!(joined);
    let timer = operations::sleep_until(
        kimojio::clock_now() + Duration::from_secs(options.timeout_seconds),
    );
    let timer = pin!(timer);
    let (app, connection, server) = match futures::future::select(joined.as_mut(), timer).await {
        futures::future::Either::Left((result, _)) => result,
        futures::future::Either::Right((_, remaining)) => {
            abort_client.abort();
            abort_server.abort();
            shared.release_producers();
            let settled =
                operations::timeout_at(kimojio::clock_now() + Duration::from_secs(5), remaining)
                    .await;
            if settled.is_ok() {
                let _ = settle_server(&shared, false).await;
            }
            return Err(failure(
                "watchdog expired; run invalid even after abort settlement",
            ));
        }
    };
    // A failed client cohort can leave application drain tasks to join after abort.
    let cleanup = scoped(meter, Tag::Application, settle_server(&shared, false)).await;
    let measured = app?;
    connection?;
    server?;
    cleanup?;
    Ok(measured)
}

async fn run_pair<M: Meter>(options: Options, meter: M) -> Result<Value, Error> {
    if options.phase == "cold" {
        meter.begin();
    }
    let cold = Start::now();
    let options = meter.enter(Tag::Application, || Rc::new(options));
    let shared = meter.enter(Tag::Application, || Rc::new(Shared::new(&options)));
    let (fd, peer) = meter
        .enter(Tag::Connection, || {
            rustix::net::socketpair(
                rustix::net::AddressFamily::UNIX,
                rustix::net::SocketType::STREAM,
                rustix::net::SocketFlags::CLOEXEC,
                None,
            )
        })
        .map_err(Error::Transport)?;
    let server_control = Shutdown::default();
    let server_options = options.clone();
    let server_shared = shared.clone();
    let handler = move |request| {
        let shared = server_shared.clone();
        let options = server_options.clone();
        scoped(
            meter,
            Tag::Application,
            handler(request, shared, options, meter),
        )
    };
    let result = if options.backend == "native" {
        let (client, connection) = meter.enter(Tag::Connection, || {
            connect_native(fd, configuration(&options))
        });
        drive(
            client,
            connection.run(),
            serve_connection_native_with_shutdown(
                peer,
                configuration(&options),
                server_control.clone(),
                handler,
            ),
            server_control,
            shared.clone(),
            &options,
            meter,
        )
        .await
    } else {
        let (client, connection) = meter.enter(Tag::Connection, || {
            connect(OwnedFdStream::new(fd), configuration(&options))
        });
        drive(
            client,
            connection.run(),
            serve_connection_with_shutdown(
                OwnedFdStream::new(peer),
                configuration(&options),
                server_control.clone(),
                handler,
            ),
            server_control,
            shared.clone(),
            &options,
            meter,
        )
        .await
    };
    let warmup = if options.phase == "steady" {
        options.warmup
    } else {
        0
    };
    let total = (warmup + options.cohorts) * options.concurrency;
    let actual = (
        shared.client_retirements.get(),
        shared.server_retirements.get(),
        shared.witnesses.get(),
    );
    if result.is_ok()
        && (actual.0 != total
            || actual.1 != total
            || (options.case == "duplex" && actual.2 != total))
    {
        return Err(failure("final retirement or overlap count mismatch"));
    }
    drop(shared);
    let measurement = match result? {
        Some(measurement) => measurement,
        None => cold.end(meter),
    };
    let exchanges = options.cohorts * options.concurrency;
    let (request, response) = options.sizes();
    Ok(json!({
        "schema": 1, "valid": true, "backend": options.backend, "case": options.case,
        "phase": options.phase, "concurrency": options.concurrency,
        "cohorts": options.cohorts, "warmup_cohorts": warmup, "measured_exchanges": exchanges,
        "client_retirements": actual.0, "server_retirements": actual.1,
        "early_response_witnesses_including_warmup": actual.2,
        "overlap_required": options.case == "duplex",
        "request_bytes": request, "response_bytes": response, "chunk_bytes": options.chunk,
        "payload_storage": if options.owned { "owned-per-chunk" } else { "static" },
        "checked_payload_bytes_including_warmup": total * (request + response),
        "elapsed_ns": measurement.elapsed_ns, "process_cpu_ns": measurement.cpu_ns,
        "ns_per_exchange": measurement.elapsed_ns as f64 / exchanges as f64,
        "allocations": measurement.allocations, "transport": "real_unix_stream_socketpair",
        "connections_created": 1, "reconnects": 0, "drivers_closed_successfully": 2,
        "scope": if options.phase == "cold" { "socket_creation_through_retirement_and_close" }
                 else { "warmed_cohorts_through_both_endpoint_retirements_excluding_close" },
        "max_queued_requests": options.concurrency, "max_active_streams": 128,
    }))
}

pub async fn run<M: Meter>(options: Options, meter: M) -> Result<Value, Error> {
    options.validate().map_err(Error::Application)?;
    let result = run_pair(options, meter).await;
    if result.is_err() {
        meter.cancel();
    }
    result
}

pub async fn entry<M: Meter>(meter: M) -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help") {
        println!(
            "--backend native|generic --case empty|fixed|stream|duplex --phase cold|steady \
            --concurrency 1|8|32|128 --cohorts N --warmup N --bytes 1..1048576 \
            --chunk 1..16384 --payload static|owned --timeout-seconds 1..300"
        );
        return Ok(());
    }
    match run(Options::parse(args)?, meter).await {
        Ok(report) => {
            println!("{report}");
            Ok(())
        }
        Err(error) => {
            println!(
                "{}",
                json!({"schema":1,"valid":false,"error":error.to_string()})
            );
            Err(error.into())
        }
    }
}
