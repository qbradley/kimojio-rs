mod schema;
mod transport;

use kimojio_fsm_http2::*;
use schema::*;
use sha2::Digest;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::File,
    io::{Read, Write},
    net::{IpAddr, SocketAddr, TcpListener, TcpStream},
    time::Duration,
};
use transport::{Buffer, Event, Transport, checked};

const CHUNK: usize = 16 * 1024;
const MAX_PENDING_FRAGMENTS: usize = 8192;
const MAX_PENDING_CAPACITY: usize = 8 * 1024 * 1024;

struct Producer {
    remaining: u64,
    trailers: Fields,
    echo: bool,
    input_ended: bool,
    stopped: bool,
    busy: bool,
    permit: Option<SendPermit>,
    pending: VecDeque<(BodyOp, usize)>,
}

impl Producer {
    fn bytes(remaining: u64, trailers: Fields) -> Self {
        Self {
            remaining,
            trailers,
            echo: false,
            input_ended: false,
            stopped: false,
            busy: false,
            permit: None,
            pending: VecDeque::new(),
        }
    }
    fn echo() -> Self {
        Self {
            echo: true,
            ..Self::bytes(0, Vec::new())
        }
    }
    fn release(&mut self, core: &mut Connection<Buffer>) -> Result<(), String> {
        while let Some((body, _)) = self.pending.pop_front() {
            checked(core.release_body(body.release()))?;
        }
        self.permit = None;
        self.stopped = true;
        Ok(())
    }
    fn drive(&mut self, core: &mut Connection<Buffer>, id: StreamId) -> Result<(), String> {
        if self.stopped || self.busy {
            return Ok(());
        }
        if !self.echo && self.remaining == 0 && !self.trailers.is_empty() {
            checked(core.trailers(id, &fields(&self.trailers)))?;
            self.stopped = true;
            self.permit = None;
            return Ok(());
        }
        if self.echo && self.pending.is_empty() && !self.input_ended {
            return Ok(());
        }
        let Some(permit) = self.permit.take() else {
            return Ok(());
        };
        let (buffer, end) = if self.echo {
            if let Some((body, start)) = self.pending.pop_front() {
                if body.retained_capacity() > permit.max_retained_capacity() {
                    checked(core.release_body(body.release()))?;
                    return Err("echo page exceeds SendPermit retained capacity".into());
                }
                let end = (start + permit.max_bytes()).min(body.bytes().len());
                (Buffer::Lease { body, start, end }, false)
            } else {
                (Buffer::Bytes(Vec::new()), true)
            }
        } else {
            let n = chunk_len(
                self.remaining,
                permit.max_bytes(),
                permit.max_retained_capacity(),
            );
            if n == 0 && self.remaining != 0 {
                return Err("nonempty producer received a zero-byte permit".into());
            }
            self.remaining -= n as u64;
            (
                Buffer::Bytes(vec![(id.get() % 251) as u8; n]),
                self.remaining == 0 && self.trailers.is_empty(),
            )
        };
        match core.send(permit, buffer, end) {
            Ok(()) => {
                self.busy = true;
                self.stopped = end;
                Ok(())
            }
            Err(rejected) => {
                if let Buffer::Lease { body, .. } = rejected.value.1 {
                    checked(core.release_body(body.release()))?;
                }
                Err(format!("send rejected: {:?}", rejected.error))
            }
        }
    }
    fn sent(&mut self, core: &mut Connection<Buffer>, sent: Sent<Buffer>) -> Result<(), String> {
        self.busy = false;
        if let Buffer::Lease { body, end, .. } = sent.buffer {
            if sent.result.is_ok() && !self.stopped && end < body.bytes().len() {
                self.pending.push_front((body, end));
            } else {
                checked(core.release_body(body.release()))?;
            }
        }
        if sent.result.is_err() {
            self.release(core)?;
        }
        Ok(())
    }
}

fn chunk_len(remaining: u64, max_bytes: usize, capacity: usize) -> usize {
    remaining.min(CHUNK.min(max_bytes).min(capacity) as u64) as usize
}

fn fields(values: &Fields) -> Vec<H2HeaderField> {
    values
        .iter()
        .map(|(name, value)| H2HeaderField::new(name.as_bytes(), value.as_bytes()))
        .collect()
}

fn field<'a>(fields: &'a Fields, name: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

fn release_sent(core: &mut Connection<Buffer>, sent: Sent<Buffer>) -> Result<(), String> {
    if let Buffer::Lease { body, .. } = sent.buffer {
        checked(core.release_body(body.release()))?;
    }
    Ok(())
}

fn connection_error(result: ConnectionResult) -> Option<Error> {
    let code = match result {
        ConnectionResult::Graceful | ConnectionResult::PeerClosed => return None,
        ConnectionResult::Protocol(error) => {
            eprintln!("peer protocol error: {error:?}");
            error.code.as_u32()
        }
        ConnectionResult::IoFailed => 2,
        ConnectionResult::ResourceExhausted => 11,
    };
    Some(Error {
        scope: "connection",
        code,
    })
}

fn stream_error(outcome: StreamOutcome) -> Option<Error> {
    let code = match outcome {
        StreamOutcome::Complete | StreamOutcome::ConnectionFailed => return None,
        StreamOutcome::Reset(code) => code,
        StreamOutcome::Unprocessed => 7,
        StreamOutcome::Deadline => 8,
    };
    Some(Error {
        scope: "stream",
        code,
    })
}

fn begin_shutdown(core: &mut Connection<Buffer>) -> Result<(), String> {
    match core.shutdown() {
        Ok(()) | Err(CommandError::InvalidState) => Ok(()),
        Err(error) => Err(format!("shutdown rejected: {error:?}")),
    }
}

pub fn main() -> Result<(), String> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("client") if args.len() == 3 => client(&args[1], &args[2]),
        Some("server") if args.len() == 2 && !args[1].starts_with("--") => {
            let input: ServerInput = checked(serde_json::from_slice(&read_input(&args[1])?))?;
            if input.schema != 1 || !(1..=120_000).contains(&input.timeout_ms) {
                return Err("invalid server schema or timeout".into());
            }
            server(input.config.config()?, Duration::from_millis(input.timeout_ms))
        }
        Some("server") => {
            let windows = server_args(&args[1..])?;
            server(windows.config()?, Duration::from_secs(120))
        }
        _ => Err("usage: interop client REQUEST_JSON RESULT_JSON | server REQUEST_JSON | server [--stream-window N/default] [--connection-window N/default]".into()),
    }
}

fn server_args(args: &[String]) -> Result<Windows, String> {
    let mut windows = Windows::default();
    let mut seen = BTreeSet::new();
    for pair in args.chunks(2) {
        if pair.len() != 2 || !seen.insert(&pair[0]) {
            return Err("server flags require one value each".into());
        }
        let value = if pair[1] == "default" {
            None
        } else {
            Some(checked(pair[1].parse::<u32>())?)
        };
        match pair[0].as_str() {
            "--stream-window" => windows.stream_window = value,
            "--connection-window" => windows.connection_window = value,
            _ => return Err(format!("unknown server flag: {}", pair[0])),
        }
    }
    Ok(windows)
}

fn read_input(request_file: &str) -> Result<Vec<u8>, String> {
    let file = checked(File::open(request_file))?;
    let mut bytes = Vec::new();
    checked(file.take(MAX_INPUT + 1).read_to_end(&mut bytes))?;
    if bytes.len() as u64 > MAX_INPUT {
        return Err("request file exceeds fixture size limit".into());
    }
    Ok(bytes)
}

fn client(request_file: &str, result_file: &str) -> Result<(), String> {
    let input: Input = checked(serde_json::from_slice(&read_input(request_file)?))?;
    input.validate()?;
    let address: IpAddr = checked(input.host.parse())?;
    if !address.is_loopback() {
        return Err("fixture permits loopback IP literals only".into());
    }
    let socket = checked(TcpStream::connect_timeout(
        &SocketAddr::new(address, input.port),
        Duration::from_millis(input.timeout_ms),
    ))?;
    let mut transport = Transport::new(socket)?;
    let mut core = checked(Client::<Buffer>::new(
        input.config.config()?,
        Duration::ZERO,
    ))?;
    let mut producers = BTreeMap::new();
    let mut reports = Vec::with_capacity(input.request_count);
    let mut held = VecDeque::new();
    let result = client_loop(
        &input,
        &mut core,
        &mut transport,
        &mut producers,
        &mut reports,
        &mut held,
    );
    if result.is_err() {
        for producer in producers.values_mut() {
            producer.release(&mut core)?;
        }
        while let Some(body) = held.pop_front() {
            checked(core.release_body(body.release()))?;
        }
        transport.drain(&mut core)?;
    }
    let connection = result?;
    for report in &mut reports {
        report.finish();
    }
    let report = Report {
        schema: 1,
        streams: reports,
        connection,
    };
    let mut file = checked(File::create(result_file))?;
    checked(serde_json::to_writer(&mut file, &report))?;
    checked(file.write_all(b"\n"))
}

fn client_loop(
    input: &Input,
    core: &mut Client<Buffer>,
    transport: &mut Transport,
    producers: &mut BTreeMap<StreamId, Producer>,
    reports: &mut Vec<StreamReport>,
    held: &mut VecDeque<BodyOp>,
) -> Result<ConnectionReport, String> {
    let timeout = Duration::from_millis(input.timeout_ms);
    let mut next_request = 0;
    let mut shutdown = false;
    let mut ended = BTreeSet::new();
    let mut reset = BTreeSet::new();
    let mut report_metadata = 0usize;
    let mut settings_processed = false;
    loop {
        if transport.now() > timeout {
            let state: Vec<_> = producers
                .iter()
                .map(|(id, p)| (id.get(), p.remaining, p.busy, p.stopped, p.permit.is_some()))
                .collect();
            return Err(format!(
                "client watchdog expired (not a successful EOF); producers (id, remaining, busy, stopped, permit): {state:?}"
            ));
        }
        while !shutdown
            && next_request < input.requests.len()
            && producers.len() < input.concurrency
        {
            let request = &input.requests[next_request];
            let mut headers = vec![
                H2HeaderField::new(b":method", request.method.as_bytes()),
                H2HeaderField::new(
                    b":authority",
                    format!("{}:{}", input.host, input.port).as_bytes(),
                ),
            ];
            if request.method != "CONNECT" {
                headers.push(H2HeaderField::new(b":scheme", b"http"));
                headers.push(H2HeaderField::new(b":path", request.path.as_bytes()));
                headers.push(H2HeaderField::new(
                    b"content-length",
                    request.body_bytes.to_string().as_bytes(),
                ));
            }
            let end = request.body_bytes == 0 && request.trailers.is_empty();
            let id = match core.request(&headers, end) {
                Ok(id) => id,
                Err(CommandError::Capacity) => break,
                Err(CommandError::InvalidState) => {
                    begin_shutdown(core)?;
                    shutdown = true;
                    break;
                }
                Err(error) => return Err(format!("request rejected: {error:?}")),
            };
            if id.get() != 2 * next_request as u32 + 1 {
                return Err("unexpected stream allocation order".into());
            }
            let mut producer = Producer::bytes(request.body_bytes, request.trailers.clone());
            producer.stopped = end;
            producers.insert(id, producer);
            reports.push(StreamReport::new(id.get()));
            next_request += 1;
        }
        if !shutdown && next_request == input.requests.len() && producers.is_empty() {
            begin_shutdown(core)?;
            shutdown = true;
        }
        let progress = transport.io(core)?;
        match core.next(transport) {
            Some(Event::Headers(id, kind, headers)) => {
                let report = &mut reports[(id.get() / 2) as usize];
                match kind {
                    HeadKind::Response(status) => {
                        report.status = Some(status);
                        report.content_length =
                            field(&headers, "content-length").and_then(|s| s.parse().ok());
                    }
                    HeadKind::Informational(status) => {
                        if report.informational.len() == 64 {
                            return Err(
                                "informational responses exceed fixture report limit".into()
                            );
                        }
                        report.informational.push(status);
                    }
                    HeadKind::Trailers => {
                        report_metadata += headers
                            .iter()
                            .map(|(n, v)| n.len() + v.len())
                            .sum::<usize>();
                        if report_metadata > MAX_PENDING_CAPACITY {
                            return Err("trailer metadata exceeds fixture report limit".into());
                        }
                        report.trailers = headers;
                    }
                    HeadKind::Request => return Err("request headers received by client".into()),
                }
            }
            Some(Event::Body(body)) => {
                let id = body.stream();
                let report = &mut reports[(id.get() / 2) as usize];
                report.bytes += body.bytes().len() as u64;
                report.digest.update(body.bytes());
                let pause = input.actions.iter().any(|action| {
                    matches!(action, Action::Pause {
                    stream_id, until_stream_ended
                } if *stream_id == id.get() && !ended.contains(until_stream_ended))
                });
                if pause {
                    if held.len() >= MAX_PENDING_FRAGMENTS
                        || held
                            .iter()
                            .map(SendBuffer::retained_capacity)
                            .sum::<usize>()
                            + body.retained_capacity()
                            > MAX_PENDING_CAPACITY
                    {
                        checked(core.release_body(body.release()))?;
                        return Err("pause exceeded bounded retained-body budget".into());
                    }
                    held.push_back(body);
                } else {
                    checked(core.release_body(body.release()))?;
                }
                if input.actions.iter().any(|action| {
                    matches!(action, Action::Reset {
                    stream_id, after_bytes, ..
                } if *stream_id == id.get() && report.bytes >= *after_bytes)
                }) && reset.insert(id.get())
                {
                    checked(core.reset(id, H2ErrorCode::Cancel))?;
                }
            }
            Some(Event::Permit(permit)) => {
                if let Some(producer) = producers.get_mut(&permit.stream()) {
                    producer.permit = Some(permit);
                }
            }
            Some(Event::Sent(sent)) => {
                if let Some(producer) = producers.get_mut(&sent.stream) {
                    producer.sent(core, sent)?;
                } else {
                    release_sent(core, sent)?;
                }
            }
            Some(Event::Stopped(id)) => {
                if let Some(producer) = producers.get_mut(&id) {
                    producer.release(core)?;
                }
            }
            Some(Event::End(end)) => {
                let report = &mut reports[(end.stream.get() / 2) as usize];
                report.ended = end.outcome == StreamOutcome::Complete;
                report.error = stream_error(end.outcome);
                ended.insert(end.stream.get());
                let mut index = 0;
                while index < held.len() {
                    let paused = input.actions.iter().any(|action| matches!(action, Action::Pause {
                        stream_id, until_stream_ended
                    } if *stream_id == held[index].stream().get() && !ended.contains(until_stream_ended)));
                    if !paused
                        || end.outcome != StreamOutcome::Complete
                            && held[index].stream() == end.stream
                    {
                        let body = held.remove(index).unwrap();
                        checked(core.release_body(body.release()))?;
                    } else {
                        index += 1;
                    }
                }
                if !shutdown
                    && input.actions.iter().any(|action| {
                        matches!(action, Action::GracefulClose {
                    after_streams
                } if ended.len() >= *after_streams)
                    })
                {
                    begin_shutdown(core)?;
                    shutdown = true;
                }
            }
            Some(Event::Retired(result)) => {
                if let Some(mut producer) = producers.remove(&result.stream) {
                    producer.release(core)?;
                }
                if result.outcome == StreamOutcome::Unprocessed {
                    begin_shutdown(core)?;
                    shutdown = true;
                }
                if result.outcome != StreamOutcome::Complete {
                    reports[(result.stream.get() / 2) as usize].error =
                        stream_error(result.outcome);
                }
            }
            Some(Event::Cancel(cancel)) => transport.cancel(core, cancel)?,
            Some(Event::Close(close)) => transport.close(core, close)?,
            Some(Event::Closed(result)) => {
                return Ok(ConnectionReport {
                    error: connection_error(result),
                    closed: transport.physically_closed,
                });
            }
            Some(Event::Again) => (),
            None => {
                settings_processed |= transport.initial_settings_received();
                if !progress
                    && (!settings_processed
                        || producers
                            .values()
                            .all(|p| p.permit.is_none() || p.stopped || p.busy))
                {
                    transport.wait(timeout)?;
                }
            }
        }
        if settings_processed {
            for (&id, producer) in producers.iter_mut() {
                producer.drive(core, id)?;
            }
        }
    }
}

fn server(config: Config, timeout: Duration) -> Result<(), String> {
    let listener = checked(TcpListener::bind(("127.0.0.1", 0)))?;
    println!("LISTEN {}", checked(listener.local_addr())?);
    checked(std::io::stdout().flush())?;
    for socket in listener.incoming() {
        let socket = checked(socket)?;
        if let Err(error) = server_connection(socket, config.clone(), timeout) {
            eprintln!("interop connection: {error}");
        }
    }
    Ok(())
}

fn server_connection(socket: TcpStream, config: Config, timeout: Duration) -> Result<(), String> {
    let mut transport = Transport::new(socket)?;
    let mut core = checked(Server::<Buffer>::new(config, Duration::ZERO))?;
    let mut producers = BTreeMap::new();
    let result = server_loop(&mut core, &mut transport, &mut producers, timeout);
    if result.is_err() {
        for producer in producers.values_mut() {
            producer.release(&mut core)?;
        }
        transport.drain(&mut core)?;
    }
    result
}

fn respond(core: &mut Server<Buffer>, id: StreamId, headers: &Fields) -> Result<Producer, String> {
    let method = field(headers, ":method").ok_or("missing method")?;
    let path = field(headers, ":path").unwrap_or("");
    if method == "HEAD" && path == "/echo" {
        let length = field(headers, "content-length").unwrap_or("0");
        checked(core.respond(
            id,
            &[
                H2HeaderField::new(b":status", b"200"),
                H2HeaderField::new(b"content-length", length.as_bytes()),
            ],
            true,
        ))?;
        let mut producer = Producer::bytes(0, Vec::new());
        producer.stopped = true;
        return Ok(producer);
    }
    if method == "CONNECT" || path == "/echo" {
        checked(core.respond(id, &[H2HeaderField::new(b":status", b"200")], false))?;
        return Ok(Producer::echo());
    }
    let (status, n, trailers, informational) = if path == "/no-content" {
        (204, 0, false, false)
    } else if path == "/early" {
        (413, 0, false, false)
    } else {
        let mut parts = path.trim_start_matches('/').split('/');
        let route = parts.next().unwrap_or("");
        let n: u64 = checked(parts.next().ok_or("route requires byte count")?.parse())?;
        if n > MAX_BODY
            || parts.next().is_some()
            || !matches!(route, "bytes" | "trailers" | "informational")
        {
            return Err("unknown route or byte count exceeds fixture bounds".into());
        }
        (200, n, route == "trailers", route == "informational")
    };
    if informational {
        checked(core.respond(id, &[H2HeaderField::new(b":status", b"103")], false))?;
    }
    let mut response = vec![H2HeaderField::new(
        b":status",
        status.to_string().as_bytes(),
    )];
    if status != 204 {
        response.push(H2HeaderField::new(
            b"content-length",
            n.to_string().as_bytes(),
        ));
    }
    let end = status != 200 || method == "HEAD" || n == 0 && !trailers;
    checked(core.respond(id, &response, end))?;
    let mut producer = Producer::bytes(
        n,
        if trailers {
            vec![("x-end".into(), "done".into())]
        } else {
            Vec::new()
        },
    );
    producer.stopped = end;
    Ok(producer)
}

fn server_loop(
    core: &mut Server<Buffer>,
    transport: &mut Transport,
    producers: &mut BTreeMap<StreamId, Producer>,
    timeout: Duration,
) -> Result<(), String> {
    loop {
        if transport.now() > timeout {
            return Err("server connection watchdog expired".into());
        }
        let progress = transport.io(core)?;
        match core.next(transport) {
            Some(Event::Headers(id, HeadKind::Request, headers)) => {
                producers.insert(id, respond(core, id, &headers)?);
            }
            Some(Event::Headers(_, _, _)) => (),
            Some(Event::Body(body)) => {
                let id = body.stream();
                let pending_count: usize = producers.values().map(|p| p.pending.len()).sum();
                let pending_capacity: usize = producers
                    .values()
                    .flat_map(|p| &p.pending)
                    .map(|(b, _)| b.retained_capacity())
                    .sum();
                if let Some(producer) = producers.get_mut(&id).filter(|p| p.echo && !p.stopped) {
                    if pending_count >= MAX_PENDING_FRAGMENTS
                        || pending_capacity + body.retained_capacity() > MAX_PENDING_CAPACITY
                    {
                        checked(core.release_body(body.release()))?;
                        return Err("echo exceeded bounded retained-body budget".into());
                    }
                    if body.bytes().is_empty() {
                        checked(core.release_body(body.release()))?;
                    } else {
                        producer.pending.push_back((body, 0));
                    }
                } else {
                    checked(core.release_body(body.release()))?;
                }
            }
            Some(Event::Permit(permit)) => {
                if let Some(producer) = producers.get_mut(&permit.stream()) {
                    producer.permit = Some(permit);
                }
            }
            Some(Event::Sent(sent)) => {
                if let Some(producer) = producers.get_mut(&sent.stream) {
                    producer.sent(core, sent)?;
                } else {
                    release_sent(core, sent)?;
                }
            }
            Some(Event::Stopped(id)) => {
                if let Some(producer) = producers.get_mut(&id) {
                    producer.release(core)?;
                }
            }
            Some(Event::End(end)) => {
                if let Some(producer) = producers.get_mut(&end.stream) {
                    producer.input_ended = true;
                    if end.outcome != StreamOutcome::Complete {
                        producer.release(core)?;
                    }
                }
            }
            Some(Event::Retired(result)) => {
                if let Some(mut producer) = producers.remove(&result.stream) {
                    producer.release(core)?;
                }
            }
            Some(Event::Cancel(cancel)) => transport.cancel(core, cancel)?,
            Some(Event::Close(close)) => transport.close(core, close)?,
            Some(Event::Closed(_)) => return Ok(()),
            Some(Event::Again) => (),
            None if !progress => transport.wait(timeout)?,
            None => (),
        }
        for (&id, producer) in producers.iter_mut() {
            producer.drive(core, id)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn producer_bounds_do_not_depend_on_body_length_or_wire_credit() {
        assert_eq!(chunk_len(16 * 1024 * 1024 + 17, 65536, 65536), CHUNK);
        assert_eq!(chunk_len(30000, 1024, 32768), 1024);
        assert_eq!(chunk_len(30000, 32768, 512), 512);
        assert_eq!(chunk_len(17, 32768, 65536), 17);
        assert_eq!(chunk_len(0, 32768, 65536), 0);
    }

    #[test]
    fn zero_length_producer_submits_end_exactly_once() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let socket = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (_peer, _) = listener.accept().unwrap();
        let mut transport = Transport::new(socket).unwrap();
        let config = Config {
            shutdown_timeout: Duration::from_millis(10),
            ..Config::default()
        };
        let mut core = Client::<Buffer>::new(config, Duration::ZERO).unwrap();
        let headers = [
            H2HeaderField::new(b":method", b"POST"),
            H2HeaderField::new(b":scheme", b"http"),
            H2HeaderField::new(b":authority", b"localhost"),
            H2HeaderField::new(b":path", b"/"),
        ];
        let id = core.request(&headers, false).unwrap();
        let Some(Event::Permit(permit)) = core.next(&mut transport) else {
            panic!("request must offer one bounded source reservation");
        };
        let mut producer = Producer::bytes(0, Vec::new());
        producer.permit = Some(permit);
        producer.drive(&mut core, id).unwrap();
        assert!(producer.busy && producer.stopped);
        assert!(producer.permit.is_none());
        producer.drive(&mut core, id).unwrap();
        assert!(producer.busy && producer.stopped);
        transport.drain(&mut core).unwrap();
    }

    #[test]
    fn server_flags_are_strict() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(server_args(&args(&["--stream-window", "default"])).is_ok());
        assert!(server_args(&args(&["--stream-window"])).is_err());
        assert!(server_args(&args(&["--unknown", "1"])).is_err());
        assert!(server_args(&args(&["--stream-window", "1", "--stream-window", "2"])).is_err());
    }
}
