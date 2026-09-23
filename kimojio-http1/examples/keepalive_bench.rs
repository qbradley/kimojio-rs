use std::{
    cell::Cell,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

use futures::FutureExt;
use kimojio::{OwnedFdStream, operations};
use kimojio_http1::{
    Client, Config, ConnectionId, Error, IncomingBody, IncomingFrame, OutgoingBody, OutgoingFrame,
    connect, connect_native,
    http::{Request, Response, StatusCode},
    serve_connection, serve_connection_native,
};
use serde_json::{Value, json};

const BUFFER_BYTES: usize = 16 * 1024;
const MAX_PAYLOAD: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug)]
struct Options {
    iterations: u64,
    warmup: u64,
    request_bytes: usize,
    response_bytes: usize,
    chunk_bytes: usize,
    chunked: bool,
    native: bool,
    duplex: bool,
    copy_forward: bool,
    coalesce_full_bodies: bool,
    timeout_seconds: u64,
    output: Option<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            iterations: 10_000,
            warmup: 1_000,
            request_bytes: 0,
            response_bytes: 128,
            chunk_bytes: BUFFER_BYTES,
            chunked: false,
            native: false,
            duplex: false,
            copy_forward: false,
            coalesce_full_bodies: false,
            timeout_seconds: 60,
            output: None,
        }
    }
}

impl Options {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut options = Self::default();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--chunked" => {
                    options.chunked = true;
                    continue;
                }
                "--native" => {
                    options.native = true;
                    continue;
                }
                "--duplex" => {
                    options.duplex = true;
                    continue;
                }
                "--copy-forward" => {
                    options.copy_forward = true;
                    continue;
                }
                "--coalesce-full-bodies" => {
                    options.coalesce_full_bodies = true;
                    continue;
                }
                _ => {}
            }
            let value = args
                .next()
                .ok_or_else(|| format!("missing value for {arg}"))?;
            match arg.as_str() {
                "--iterations" => options.iterations = value.parse().map_err(|_| arg)?,
                "--warmup" => options.warmup = value.parse().map_err(|_| arg)?,
                "--request-bytes" => options.request_bytes = value.parse().map_err(|_| arg)?,
                "--response-bytes" => options.response_bytes = value.parse().map_err(|_| arg)?,
                "--chunk-bytes" => options.chunk_bytes = value.parse().map_err(|_| arg)?,
                "--timeout-seconds" => options.timeout_seconds = value.parse().map_err(|_| arg)?,
                "--json" => options.output = Some(value.into()),
                _ => return Err(format!("unknown option {arg}")),
            }
        }
        if options.iterations == 0
            || options.iterations > 10_000_000
            || options.warmup > 1_000_000
            || options.request_bytes > MAX_PAYLOAD
            || options.response_bytes > MAX_PAYLOAD
            || options.chunk_bytes == 0
            || options.chunk_bytes > BUFFER_BYTES
            || options.timeout_seconds == 0
            || options.timeout_seconds > 3600
        {
            return Err("benchmark option is outside its documented bound".into());
        }
        if options.duplex && options.request_bytes != options.response_bytes {
            return Err("duplex echo requires equal request and response sizes".into());
        }
        if options.copy_forward && !options.duplex {
            return Err("--copy-forward requires --duplex".into());
        }
        Ok(options)
    }
}

fn config(slot: u64, options: &Options) -> Config {
    let mut config = Config::new(ConnectionId {
        slot,
        generation: 1,
    });
    config.protocol.max_requests = options.warmup + options.iterations + 1;
    config.protocol.max_buffer_bytes = BUFFER_BYTES;
    config.protocol.max_chunk_metadata_bytes = 256 * 1024 * 1024;
    config.protocol.max_body_bytes = MAX_PAYLOAD as u64;
    config.coalesce_full_bodies = options.coalesce_full_bodies;
    config
}

fn source(bytes: Rc<[u8]>, options: &Options) -> OutgoingBody {
    if !options.chunked && bytes.len() <= options.chunk_bytes {
        return OutgoingBody::full(bytes.as_ref().to_vec());
    }
    let length = (!options.chunked).then_some(bytes.len() as u64);
    let chunk_bytes = options.chunk_bytes;
    let source = futures::stream::unfold((bytes, 0usize), move |(bytes, offset)| async move {
        if offset == bytes.len() {
            None
        } else {
            let end = (offset + chunk_bytes).min(bytes.len());
            let frame = OutgoingFrame::Data(bytes[offset..end].to_vec());
            Some((Ok(frame), (bytes, end)))
        }
    });
    OutgoingBody::from_stream(length, source)
}

fn compare_chunk(expected: &[u8], received: &[u8], offset: &mut usize) -> Result<(), Error> {
    let end = offset.checked_add(received.len()).ok_or(Error::Limit)?;
    if expected.get(*offset..end) != Some(received) {
        return Err(Error::Application("benchmark payload mismatch".into()));
    }
    *offset = end;
    Ok(())
}

async fn receive(body: &mut IncomingBody, expected: &[u8]) -> Result<(), Error> {
    let mut offset = 0;
    while let Some(frame) = body.frame().await? {
        match frame {
            IncomingFrame::Data(chunk) => compare_chunk(expected, &chunk, &mut offset)?,
            IncomingFrame::Trailers(headers) if headers.is_empty() => {}
            IncomingFrame::Trailers(_) => {
                return Err(Error::Application("unexpected benchmark trailers".into()));
            }
        }
    }
    if offset != expected.len() {
        return Err(Error::Application("incomplete benchmark payload".into()));
    }
    Ok(())
}

fn forward(
    incoming: IncomingBody,
    expected: Rc<[u8]>,
    requests: Rc<Cell<u64>>,
    copy: bool,
) -> OutgoingBody {
    let source = futures::stream::try_unfold(
        (incoming, expected, requests, 0usize),
        move |(mut incoming, expected, requests, mut offset)| async move {
            loop {
                match incoming.frame().await? {
                    Some(IncomingFrame::Data(chunk)) => {
                        compare_chunk(&expected, &chunk, &mut offset)?;
                        let frame = if copy {
                            OutgoingFrame::Data(chunk.to_vec())
                        } else {
                            OutgoingFrame::Forward(chunk)
                        };
                        return Ok(Some((frame, (incoming, expected, requests, offset))));
                    }
                    Some(IncomingFrame::Trailers(headers)) if headers.is_empty() => {}
                    Some(IncomingFrame::Trailers(_)) => {
                        return Err(Error::Application("unexpected benchmark trailers".into()));
                    }
                    None => {
                        if offset != expected.len() {
                            return Err(Error::Application("incomplete benchmark upload".into()));
                        }
                        requests.set(requests.get() + 1);
                        return Ok(None);
                    }
                }
            }
        },
    );
    OutgoingBody::from_stream(None, source).continue_request_body()
}

async fn exchange(
    client: &mut Client,
    options: &Options,
    request_bytes: Rc<[u8]>,
    response_bytes: &[u8],
) -> Result<(), Error> {
    let request = Request::builder()
        .uri("/keepalive")
        .header("host", "benchmark")
        .body(source(request_bytes, options))
        .map_err(|error| Error::Application(error.to_string()))?;
    let mut response = client.send(request).await?;
    if response.status() != StatusCode::OK {
        return Err(Error::Application("unexpected benchmark status".into()));
    }
    receive(response.body_mut(), response_bytes).await
}

fn cpu_seconds() -> f64 {
    let time = rustix::time::clock_gettime(rustix::time::ClockId::ProcessCPUTime);
    time.tv_sec as f64 + time.tv_nsec as f64 / 1_000_000_000.0
}

async fn run_pair(options: Rc<Options>) -> Result<Value, Error> {
    let (client_fd, server_fd) = rustix::net::socketpair(
        rustix::net::AddressFamily::UNIX,
        rustix::net::SocketType::STREAM,
        rustix::net::SocketFlags::CLOEXEC,
        None,
    )
    .map_err(Error::Transport)?;
    let (mut client, driver) = if options.native {
        let (client, connection) = connect_native(client_fd, config(1, &options));
        (client, connection.run().boxed_local())
    } else {
        let (client, connection) = connect(OwnedFdStream::new(client_fd), config(1, &options));
        (client, connection.run().boxed_local())
    };
    let requests = Rc::new(Cell::new(0u64));
    let start = Rc::new(Cell::new(None));
    let request_bytes: Rc<[u8]> = (0..options.request_bytes)
        .map(|index| (index % 251) as u8)
        .collect();
    let response_bytes: Rc<[u8]> = if options.duplex {
        request_bytes.clone()
    } else {
        (0..options.response_bytes)
            .map(|index| (250 - index % 251) as u8)
            .collect()
    };
    let server = {
        let server_config = config(2, &options);
        let native = options.native;
        let requests = requests.clone();
        let options = options.clone();
        let request_bytes = request_bytes.clone();
        let response_bytes = response_bytes.clone();
        let handler = move |mut request: Request<IncomingBody>| {
            let requests = requests.clone();
            let options = options.clone();
            let request_bytes = request_bytes.clone();
            let response_bytes = response_bytes.clone();
            async move {
                if options.duplex {
                    request.body_mut().accept().await?;
                    return Ok(Response::new(forward(
                        request.into_body(),
                        request_bytes,
                        requests,
                        options.copy_forward,
                    )));
                }
                receive(request.body_mut(), &request_bytes).await?;
                requests.set(requests.get() + 1);
                Ok(Response::new(source(response_bytes, &options)))
            }
        };
        if native {
            serve_connection_native(server_fd, server_config, handler).boxed_local()
        } else {
            serve_connection(OwnedFdStream::new(server_fd), server_config, handler).boxed_local()
        }
    };
    let application = {
        let start = start.clone();
        let options = options.clone();
        async move {
            for _ in 0..options.warmup {
                exchange(
                    &mut client,
                    &options,
                    request_bytes.clone(),
                    &response_bytes,
                )
                .await?;
            }
            start.set(Some((Instant::now(), cpu_seconds())));
            for _ in 0..options.iterations {
                exchange(
                    &mut client,
                    &options,
                    request_bytes.clone(),
                    &response_bytes,
                )
                .await?;
            }
            client.shutdown().await
        }
    };
    let (application, driver, server) = futures::join!(application, driver, server);
    let cpu_end = cpu_seconds();
    let end = Instant::now();
    application?;
    driver?;
    server?;
    let expected_requests = options.warmup + options.iterations;
    if requests.get() != expected_requests {
        return Err(Error::Application(
            "benchmark exchange count mismatch".into(),
        ));
    }
    let (start, cpu_start) = start
        .get()
        .ok_or_else(|| Error::Application("benchmark did not start measurement".into()))?;
    let elapsed = end.duration_since(start).as_secs_f64();
    Ok(json!({
        "schema": 1,
        "valid": true,
        "transport": "unix_socketpair",
        "backend": if options.native { "native" } else { "stream" },
        "duplex": options.duplex,
        "forwarding": if options.duplex { if options.copy_forward { "copy" } else { "lease" } } else { "none" },
        "coalesce_full_bodies": options.coalesce_full_bodies,
        "connections_created": 1,
        "reconnects": 0,
        "warmup_exchanges": options.warmup,
        "measured_exchanges": options.iterations,
        "server_exchanges": requests.get(),
        "request_bytes": options.request_bytes,
        "response_bytes": options.response_bytes,
        "chunk_bytes": options.chunk_bytes,
        "chunked": options.chunked,
        "response_chunked": options.duplex || options.chunked,
        "validated_payload_bytes": options.iterations
            * (options.request_bytes as u64 + options.response_bytes as u64),
        "elapsed_seconds": elapsed,
        "process_cpu_seconds": cpu_end - cpu_start,
        "nanoseconds_per_exchange": elapsed * 1_000_000_000.0 / options.iterations as f64,
        "exchanges_per_second": options.iterations as f64 / elapsed,
        "measurement_scope": "after_warmup_through_successful_connection_shutdown",
        "validation": "complete_payload_equality_and_status_in_both_directions",
    }))
}

async fn run(options: Rc<Options>) -> Result<Value, String> {
    let deadline = kimojio::clock_now() + Duration::from_secs(options.timeout_seconds);
    operations::timeout_at(deadline, run_pair(options))
        .await
        .map_err(|_| {
            "benchmark watchdog expired; incomplete settlement invalidates the run".to_owned()
        })?
        .map_err(|error| error.to_string())
}

#[kimojio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help") {
        println!(
            "keepalive_bench [--iterations 1..10000000] [--warmup 0..1000000] \
             [--request-bytes 0..16777216] [--response-bytes 0..16777216] \
             [--chunk-bytes 1..16384] [--chunked] [--timeout-seconds 1..3600] [--json FILE]"
        );
        println!(
            "Backend: --native. Reusable echo: --duplex [--copy-forward], with equal body sizes."
        );
        println!("Optional combined metadata/payload writes: --coalesce-full-bodies.");
        return Ok(());
    }
    let options = Rc::new(Options::parse(args)?);
    let result = run(options.clone()).await;
    let report = match &result {
        Ok(report) => report.clone(),
        Err(error) => json!({
            "schema": 1,
            "valid": false,
            "error": error,
            "exchanges_per_second": null,
            "nanoseconds_per_exchange": null,
        }),
    };
    let report = serde_json::to_string(&report)?;
    println!("{report}");
    if let Some(path) = &options.output {
        std::fs::write(path, format!("{report}\n"))?;
    }
    result.map(|_| ()).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_workload_bounds() {
        for args in [
            ["--iterations", "0"],
            ["--chunk-bytes", "0"],
            ["--response-bytes", "16777217"],
            ["--timeout-seconds", "0"],
        ] {
            assert!(Options::parse(args.map(str::to_owned)).is_err());
        }
        assert!(Options::parse(["--duplex".to_owned()]).is_err());
        assert!(Options::parse(["--copy-forward".to_owned()]).is_err());
    }

    #[test]
    fn payload_comparison_rejects_corruption_and_excess() {
        let mut offset = 0;
        compare_chunk(b"abcdef", b"abc", &mut offset).unwrap();
        assert_eq!(offset, 3);
        assert!(compare_chunk(b"abcdef", b"deg", &mut offset).is_err());
        assert_eq!(offset, 3);
        assert!(compare_chunk(b"abcdef", b"defg", &mut offset).is_err());
        compare_chunk(b"abcdef", b"def", &mut offset).unwrap();
        assert_eq!(offset, 6);
    }

    #[test]
    fn parses_explicit_backend_and_coalescing_policy() {
        assert!(!Options::default().coalesce_full_bodies);
        let options =
            Options::parse(["--native", "--coalesce-full-bodies"].map(str::to_owned)).unwrap();
        assert!(options.native);
        assert!(options.coalesce_full_bodies);
    }

    #[kimojio::test]
    async fn reuses_one_connection_for_fixed_and_chunked_round_trips() {
        for (native, chunked) in [(false, false), (false, true), (true, false), (true, true)] {
            let report = run(Rc::new(Options {
                iterations: 32,
                warmup: 3,
                request_bytes: 257,
                response_bytes: 32_769,
                chunk_bytes: 4096,
                chunked,
                native,
                ..Options::default()
            }))
            .await
            .unwrap();
            assert_eq!(report["server_exchanges"], 35);
            assert_eq!(report["measured_exchanges"], 32);
            assert_eq!(report["connections_created"], 1);
            assert_eq!(report["reconnects"], 0);
            assert_eq!(report["validated_payload_bytes"], 32 * (257 + 32_769));
        }
    }

    #[kimojio::test]
    async fn reuses_one_connection_for_duplex_forwarding() {
        for native in [false, true] {
            for (chunked, copy_forward) in [(false, false), (true, false), (true, true)] {
                let report = run(Rc::new(Options {
                    iterations: 8,
                    warmup: 2,
                    request_bytes: 65_537,
                    response_bytes: 65_537,
                    chunk_bytes: 4096,
                    chunked,
                    native,
                    duplex: true,
                    copy_forward,
                    ..Options::default()
                }))
                .await
                .unwrap();
                assert_eq!(report["server_exchanges"], 10);
                assert_eq!(report["reconnects"], 0);
                assert_eq!(report["duplex"], true);
                assert_eq!(report["validated_payload_bytes"], 8 * 2 * 65_537);
            }
        }
    }

    #[kimojio::test]
    async fn reuses_one_connection_with_explicit_full_body_coalescing() {
        for native in [false, true] {
            let report = run(Rc::new(Options {
                iterations: 32,
                warmup: 3,
                request_bytes: 128,
                response_bytes: 128,
                native,
                coalesce_full_bodies: true,
                ..Options::default()
            }))
            .await
            .unwrap();
            assert_eq!(report["coalesce_full_bodies"], true);
            assert_eq!(report["server_exchanges"], 35);
            assert_eq!(report["reconnects"], 0);
            assert_eq!(report["validated_payload_bytes"], 32 * 256);
        }
    }
}
