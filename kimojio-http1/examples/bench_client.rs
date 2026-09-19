use std::{
    cell::Cell,
    io::Write,
    net::SocketAddr,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

use kimojio::{
    CancellationToken, OwnedFdStream, operations, socket_helpers::create_client_socket,
    task_pool::TaskPool,
};
use kimojio_http1::{
    Client, Config, ConnectionId, IncomingFrame, OutgoingBody, connect,
    http::{Request, Uri, Version},
};
use serde_json::{Value, json};

const HISTOGRAM_SUB_BUCKETS: usize = 32;
const HISTOGRAM_BUCKETS: usize = 64 * HISTOGRAM_SUB_BUCKETS;
const MAX_ERROR_DETAILS: usize = 8;

#[derive(Debug)]
struct Options {
    url: String,
    address: SocketAddr,
    authority: String,
    target: String,
    connections: usize,
    warmup: Duration,
    duration: Duration,
    size: usize,
    output: Option<PathBuf>,
    fresh: bool,
    timeout: Duration,
    max_requests: u64,
}

impl Options {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut args = args.into_iter();
        let mut url = None;
        let mut size = None;
        let mut connections = 1;
        let mut warmup_ms = 500;
        let mut duration_ms = 2000;
        let mut timeout_ms = 5000;
        let mut max_requests = 1000;
        let mut output = None;
        let mut fresh = false;
        let mut seen = std::collections::HashSet::new();
        while let Some(flag) = args.next() {
            if !seen.insert(flag.clone()) {
                return Err(format!("duplicate argument: {flag}"));
            }
            let value = args
                .next()
                .ok_or_else(|| format!("missing value: {flag}"))?;
            match flag.as_str() {
                "--url" => url = Some(value),
                "--size" => size = Some(number(&value, 0, 16 * 1024 * 1024)? as usize),
                "--connections" => connections = number(&value, 1, 256)? as usize,
                "--warmup-ms" => warmup_ms = number(&value, 0, 600_000)?,
                "--duration-ms" => duration_ms = number(&value, 1, 600_000)?,
                "--timeout-ms" => timeout_ms = number(&value, 1, 60_000)?,
                "--max-requests-per-connection" => max_requests = number(&value, 1, 1_000_000_000)?,
                "--json" => output = Some(PathBuf::from(value)),
                "--mode" => match value.as_str() {
                    "keepalive" => fresh = false,
                    "fresh" => fresh = true,
                    _ => return Err("--mode must be keepalive or fresh".into()),
                },
                _ => return Err(format!("unknown argument: {flag}")),
            }
        }
        let url = url.ok_or("--url is required")?;
        let uri: Uri = url.parse().map_err(|_| "invalid HTTP URL")?;
        if uri.scheme_str() != Some("http") || url.contains('#') {
            return Err("use plain http without a fragment".into());
        }
        let authority = uri.authority().ok_or("URL needs an authority")?.to_string();
        let address: SocketAddr = authority
            .parse()
            .map_err(|_| "URL needs a numeric IP and explicit port; DNS and TLS are unsupported")?;
        if address.port() == 0 {
            return Err("URL port must be nonzero".into());
        }
        let target = uri
            .path_and_query()
            .map_or("/", |value| value.as_str())
            .to_owned();
        Ok(Self {
            url,
            address,
            authority,
            target,
            connections,
            warmup: Duration::from_millis(warmup_ms),
            duration: Duration::from_millis(duration_ms),
            size: size.ok_or("--size is required")?,
            output,
            fresh,
            timeout: Duration::from_millis(timeout_ms),
            max_requests,
        })
    }
}

fn number(value: &str, min: u64, max: u64) -> Result<u64, String> {
    let result: u64 = value
        .parse()
        .map_err(|_| format!("invalid integer: {value}"))?;
    if !(min..=max).contains(&result) {
        return Err(format!("value must be in {min}..={max}: {value}"));
    }
    Ok(result)
}

struct Histogram {
    buckets: Box<[u64; HISTOGRAM_BUCKETS]>,
    samples: u64,
    overflows: u64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            buckets: Box::new([0; HISTOGRAM_BUCKETS]),
            samples: 0,
            overflows: 0,
        }
    }
}

impl Histogram {
    fn record(&mut self, elapsed: Duration) {
        let ns = u64::try_from(elapsed.as_nanos()).unwrap_or_else(|_| {
            self.overflows += 1;
            u64::MAX
        });
        let ns = ns.max(1);
        let exponent = 63 - ns.leading_zeros() as usize;
        let base = 1u128 << exponent;
        let sub = ((u128::from(ns) - base) * HISTOGRAM_SUB_BUCKETS as u128 / base) as usize;
        self.buckets[exponent * HISTOGRAM_SUB_BUCKETS + sub] += 1;
        self.samples += 1;
    }

    fn merge(&mut self, other: &Self) {
        for (a, b) in self.buckets.iter_mut().zip(other.buckets.iter()) {
            *a += b;
        }
        self.samples += other.samples;
        self.overflows += other.overflows;
    }

    fn percentile_us(&self, percent: u64) -> Option<f64> {
        if self.samples == 0 {
            return None;
        }
        let rank = (self.samples * percent).div_ceil(100);
        let mut total = 0;
        for (index, count) in self.buckets.iter().enumerate() {
            total += count;
            if total >= rank {
                let base = (1u128 << (index / HISTOGRAM_SUB_BUCKETS)) as f64;
                let upper = base
                    + (index % HISTOGRAM_SUB_BUCKETS + 1) as f64 * base
                        / HISTOGRAM_SUB_BUCKETS as f64;
                return Some(upper / 1000.0);
            }
        }
        unreachable!("histogram count disagrees with its buckets")
    }
}

#[derive(Default)]
struct Stats {
    count: u64,
    warmup_count: u64,
    errors: u64,
    warmup_errors: u64,
    measured_errors: u64,
    driver_errors: u64,
    connections: u64,
    reconnects: u64,
    retired_connections: u64,
    limit_retirements: u64,
    fresh_closes: u64,
    forced_driver_drops: u64,
    details: Vec<String>,
    latency: Histogram,
}

impl Stats {
    fn error(&mut self, message: impl AsRef<str>) {
        self.errors += 1;
        self.detail(message);
    }

    fn detail(&mut self, message: impl AsRef<str>) {
        if self.details.len() < MAX_ERROR_DETAILS {
            self.details
                .push(message.as_ref().chars().take(256).collect());
        }
    }

    fn request_result(
        &mut self,
        start: Instant,
        measurement: &Measurement,
        result: &Result<bool, String>,
    ) {
        let measured = measurement
            .window
            .get()
            .is_some_and(|window| start >= window.measure);
        match result {
            Ok(_) if measured => {
                self.count += 1;
                self.latency
                    .record(kimojio::clock_now().duration_since(start));
            }
            Ok(_) => self.warmup_count += 1,
            Err(error) => {
                if measured {
                    self.measured_errors += 1;
                } else {
                    self.warmup_errors += 1;
                }
                self.error(error);
            }
        }
    }

    fn merge(&mut self, other: Self) {
        self.count += other.count;
        self.warmup_count += other.warmup_count;
        self.errors += other.errors;
        self.warmup_errors += other.warmup_errors;
        self.measured_errors += other.measured_errors;
        self.driver_errors += other.driver_errors;
        self.connections += other.connections;
        self.reconnects += other.reconnects;
        self.retired_connections += other.retired_connections;
        self.limit_retirements += other.limit_retirements;
        self.fresh_closes += other.fresh_closes;
        self.forced_driver_drops += other.forced_driver_drops;
        self.latency.merge(&other.latency);
        for detail in other.details {
            self.detail(detail);
        }
    }
}

#[derive(Clone, Copy)]
struct Window {
    measure: Instant,
    end: Instant,
}

#[derive(Default)]
struct Measurement {
    window: Cell<Option<Window>>,
    started: CancellationToken,
}

impl Measurement {
    fn begin(&self, duration: Duration) -> (Window, f64) {
        assert!(self.window.get().is_none());
        let cpu_start = cpu_seconds();
        let measure = kimojio::clock_now();
        let window = Window {
            measure,
            end: measure + duration,
        };
        self.window.set(Some(window));
        self.started.cancel();
        (window, cpu_start)
    }

    fn admits(&self, start: Instant) -> bool {
        self.window.get().is_none_or(|window| start < window.end)
    }

    async fn settlement_deadline(&self, timeout: Duration) -> Result<(), kimojio::Errno> {
        let _ = self.started.cancelled().await;
        let window = self.window.get().expect("published measurement window");
        operations::sleep_until(window.end + timeout * 3).await
    }
}

async fn finish_warmup(
    measurement: &Measurement,
    nominal_end: Instant,
    duration: Duration,
) -> Result<(Window, f64), kimojio::Errno> {
    operations::sleep_until(nominal_end).await?;
    Ok(measurement.begin(duration))
}

fn validate_chunk(bytes: &[u8], received: &mut usize, expected: usize) -> Result<(), String> {
    if bytes.len() > expected.saturating_sub(*received) {
        return Err(format!("response exceeds expected length {expected}"));
    }
    if !bytes.iter().all(|byte| *byte == b'x') {
        return Err(format!(
            "payload mismatch after {received} bytes; expected ASCII x"
        ));
    }
    *received += bytes.len();
    Ok(())
}

async fn exchange(client: &mut Client, options: &Options) -> Result<bool, String> {
    let mut request = Request::builder()
        .uri(options.target.as_str())
        .header("host", options.authority.as_str());
    if options.fresh {
        request = request.header("connection", "close");
    }
    let request = request
        .body(OutgoingBody::empty())
        .map_err(|e| e.to_string())?;
    let mut response = client
        .send(request)
        .await
        .map_err(|e| format!("send: {e}"))?;
    if response.status() != 200 {
        return Err(format!("unexpected status: {}", response.status()));
    }
    let mut close = false;
    let mut keepalive = false;
    for value in response.headers().get_all("connection") {
        let value = value.to_str().map_err(|_| "non-ASCII Connection field")?;
        for token in value.split(',').map(str::trim) {
            close |= token.eq_ignore_ascii_case("close");
            keepalive |= token.eq_ignore_ascii_case("keep-alive");
        }
    }
    close |= response.version() == Version::HTTP_10 && !keepalive;
    let mut received = 0;
    while let Some(frame) = response
        .body_mut()
        .frame()
        .await
        .map_err(|e| format!("body: {e}"))?
    {
        match frame {
            IncomingFrame::Data(chunk) => validate_chunk(&chunk, &mut received, options.size)?,
            IncomingFrame::Trailers(trailers) => {
                if !trailers.is_empty() {
                    return Err("unexpected response trailers".into());
                }
            }
        }
    }
    if received != options.size {
        return Err(format!(
            "response length {received}, expected {}",
            options.size
        ));
    }
    Ok(close)
}

async fn session(
    socket: kimojio::OwnedFd,
    options: &Options,
    id: ConnectionId,
    measurement: &Measurement,
    first_start: Instant,
    stats: &mut Stats,
) -> bool {
    let mut config = Config::new(id);
    config.protocol.max_requests = options.max_requests;
    let timeout_ns = options.timeout.as_nanos() as u64;
    config.protocol.head_timeout_ns = Some(timeout_ns);
    config.protocol.body_timeout_ns = Some(timeout_ns);
    config.protocol.idle_timeout_ns = Some(timeout_ns);
    let (mut client, connection) = connect(OwnedFdStream::new(socket), config);
    let control = client.control();
    let finished = Cell::new(false);
    let application = async {
        let mut first = Some(first_start);
        let mut used = 0;
        let mut failed = false;
        loop {
            let start = first.take().unwrap_or_else(kimojio::clock_now);
            if !measurement.admits(start) {
                break;
            }
            if finished.get() {
                stats.retired_connections += 1;
                break;
            }
            let result =
                operations::timeout_at(start + options.timeout, exchange(&mut client, options))
                    .await
                    .unwrap_or_else(|error| Err(format!("request timeout: {error:?}")));
            stats.request_result(start, measurement, &result);
            match result {
                Err(_) => {
                    failed = true;
                    control.abort();
                    break;
                }
                Ok(retired) => {
                    used += 1;
                    if options.fresh {
                        stats.fresh_closes += 1;
                        break;
                    }
                    if retired || used == options.max_requests {
                        stats.retired_connections += 1;
                        stats.limit_retirements += u64::from(used == options.max_requests);
                        break;
                    }
                }
            }
        }
        if !failed {
            match operations::timeout_at(kimojio::clock_now() + options.timeout, client.shutdown())
                .await
            {
                Ok(Ok(())) => {}
                result => {
                    stats.error(format!("shutdown: {result:?}"));
                    failed = true;
                    control.abort();
                }
            }
        }
        failed
    };
    let driver = async {
        let result = connection.run().await;
        finished.set(true);
        result
    };
    let (failed, driver) = futures::join!(application, driver);
    if let Err(error) = driver {
        stats.driver_errors += 1;
        if failed {
            stats.detail(format!("driver after request/shutdown failure: {error}"));
        } else {
            stats.error(format!("driver: {error}"));
        }
        return false;
    }
    !failed
}

async fn worker(options: Rc<Options>, slot: u64, measurement: Rc<Measurement>) -> Stats {
    let mut stats = Stats::default();
    let mut generation = 0;
    loop {
        let start = kimojio::clock_now();
        if !measurement.admits(start) {
            break;
        }
        let socket = operations::io_scope(async || {
            operations::timeout_at(
                start + options.timeout,
                create_client_socket(&options.address),
            )
            .await
        })
        .await;
        let socket = match socket {
            Ok(Ok(socket)) => socket,
            result => {
                stats.request_result(start, &measurement, &Err(format!("connect: {result:?}")));
                break;
            }
        };
        generation += 1;
        stats.connections += 1;
        stats.reconnects += u64::from(generation > 1);
        // This watchdog invalidates the run if normal abort/settlement does not finish.
        let result = {
            let session = session(
                socket,
                &options,
                ConnectionId { slot, generation },
                &measurement,
                start,
                &mut stats,
            );
            let deadline = measurement.settlement_deadline(options.timeout);
            futures::pin_mut!(session, deadline);
            match futures::future::select(session, deadline).await {
                futures::future::Either::Left((result, _)) => Ok(result),
                futures::future::Either::Right((deadline, _)) => Err(deadline),
            }
        };
        match result {
            Ok(true) => {}
            Ok(false) => break,
            Err(error) => {
                stats.forced_driver_drops += 1;
                stats.error(format!("driver settlement watchdog: {error:?}"));
                break;
            }
        }
    }
    stats
}

fn cpu_seconds() -> f64 {
    let time = rustix::time::clock_gettime(rustix::time::ClockId::ProcessCPUTime);
    time.tv_sec as f64 + time.tv_nsec as f64 / 1_000_000_000.0
}

fn requests_per_second(count: u64, errors: u64, elapsed: f64) -> Option<f64> {
    (errors == 0 && elapsed > 0.0).then(|| count as f64 / elapsed)
}

async fn run(options: Rc<Options>) -> Value {
    let pool = TaskPool::new(options.connections);
    let start = Rc::new(CancellationToken::new());
    let measurement = Rc::new(Measurement::default());
    let mut tasks = Vec::with_capacity(options.connections);
    for index in 0..options.connections {
        let start = start.clone();
        let measurement = measurement.clone();
        let options = options.clone();
        tasks.push(
            pool.spawn_task(async move {
                let _ = start.cancelled().await;
                worker(options, index as u64 + 1, measurement).await
            })
            .await
            .expect("benchmark task admission"),
        );
    }
    let warmup_start = kimojio::clock_now();
    let mut stats = Stats::default();
    let (window, cpu_start) = if options.warmup.is_zero() {
        let boundary = measurement.begin(options.duration);
        start.cancel();
        boundary
    } else {
        start.cancel();
        match finish_warmup(
            &measurement,
            warmup_start + options.warmup,
            options.duration,
        )
        .await
        {
            Ok(boundary) => boundary,
            Err(error) => {
                stats.error(format!("warmup timer: {error}"));
                measurement.begin(options.duration)
            }
        }
    };
    for task in tasks {
        match task.await {
            Ok(worker) => stats.merge(worker),
            Err(error) => stats.error(format!("worker task: {error:?}")),
        }
    }
    if stats.count == 0 && stats.errors == 0 {
        stats.error("no successful measured requests");
    }
    let elapsed = kimojio::clock_now()
        .saturating_duration_since(window.measure)
        .as_secs_f64();
    let cpu = (cpu_seconds() - cpu_start).max(0.0);
    let histogram = json!({
        "kind": "log2_32_subbuckets",
        "buckets": HISTOGRAM_BUCKETS,
        "unit": "nanoseconds",
        "quantiles": "nearest_rank_exclusive_bucket_upper_bound",
        "maximum_relative_bucket_width": 0.03125,
        "input_resolution_ns": 1,
        "zero_ns_clamped_to": 1,
        "overflow_count": stats.latency.overflows,
    });
    let config = json!({
        "url": options.url,
        "connections": options.connections,
        "warmup_ms": options.warmup.as_millis(),
        "duration_ms": options.duration.as_millis(),
        "size": options.size,
        "payload": "ascii_x",
        "mode": if options.fresh { "fresh" } else { "keepalive" },
        "timeout_ms": options.timeout.as_millis(),
        "max_requests_per_connection": options.max_requests,
        "tcp_nodelay": true,
        "tcp_keepalive": { "enabled": true, "idle_seconds": 30, "interval_seconds": 1, "probes": 30 },
        "validation": "status_200_exact_length_all_bytes_x_no_nonempty_trailers",
    });
    json!({
        "schema": 1,
        "valid": stats.errors == 0,
        "attempts": stats.count + stats.measured_errors,
        "count": stats.count,
        "validated_payload_bytes": stats.count * options.size as u64,
        "errors": stats.errors,
        "error_details": stats.details,
        "warmup_count": stats.warmup_count,
        "warmup_attempts": stats.warmup_count + stats.warmup_errors,
        "warmup_elapsed_seconds": window.measure.duration_since(warmup_start).as_secs_f64(),
        "warmup_errors": stats.warmup_errors,
        "measured_errors": stats.measured_errors,
        "driver_errors": stats.driver_errors,
        "elapsed_seconds": elapsed,
        "requests_per_second": requests_per_second(stats.count, stats.errors, elapsed),
        "process_cpu_seconds": cpu,
        "rusage": {
            "available": false,
            "scope": "user/system CPU, peak RSS, context switches require external per-process wait4",
        },
        "latency_us": {
            "p50": stats.latency.percentile_us(50),
            "p95": stats.latency.percentile_us(95),
            "p99": stats.latency.percentile_us(99),
        },
        "histogram": histogram,
        "connections": stats.connections,
        "reconnects": stats.reconnects,
        "retired_connections": stats.retired_connections,
        "limit_retirements": stats.limit_retirements,
        "fresh_closes": stats.fresh_closes,
        "forced_driver_drops": stats.forced_driver_drops,
        "counter_scope": "connections_and_errors_include_warmup_and_cleanup",
        "measurement_scope": "requests_started_in_window; elapsed_and_cpu_include_drain_and_cleanup",
        "config": config,
    })
}

#[kimojio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = Rc::new(Options::parse(std::env::args().skip(1))?);
    let report = run(options.clone()).await;
    let mut encoded = serde_json::to_vec(&report)?;
    encoded.push(b'\n');
    if let Some(path) = &options.output {
        std::fs::write(path, &encoded)?;
    }
    std::io::stdout().write_all(&encoded)?;
    if report["errors"].as_u64().unwrap_or(1) != 0 {
        return Err("benchmark failed; see JSON error_details".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(extra: &[&str]) -> Result<Options, String> {
        let mut args = vec![
            "--url",
            "http://127.0.0.1:8080/bytes/4096",
            "--size",
            "4096",
        ];
        args.extend_from_slice(extra);
        Options::parse(args.into_iter().map(str::to_owned))
    }

    #[kimojio::test]
    async fn delayed_warmup_poll_cannot_admit_unaccounted_measured_work() {
        operations::timeout_at(kimojio::clock_now() + Duration::from_secs(2), async {
            let measurement = Measurement::default();
            let duration = Duration::from_millis(1);
            let nominal = kimojio::clock_now() + Duration::from_millis(20);
            let warmup = finish_warmup(&measurement, nominal, duration);
            futures::pin_mut!(warmup);
            assert!(futures::poll!(warmup.as_mut()).is_pending());

            // Let the timer expire without polling its measurement-publication future.
            operations::sleep_until(nominal + Duration::from_millis(3))
                .await
                .unwrap();
            let late_start = kimojio::clock_now();
            assert!(late_start > nominal + duration);
            assert!(measurement.window.get().is_none());
            assert!(measurement.admits(late_start));
            let mut stats = Stats::default();
            stats.request_result(late_start, &measurement, &Ok(false));
            assert_eq!((stats.count, stats.warmup_count), (0, 1));

            let cpu_before_publication = cpu_seconds();
            let (window, cpu_start) = warmup.await.unwrap();
            assert!(cpu_start >= cpu_before_publication);
            assert!(window.measure > late_start);
            assert_eq!(window.end.duration_since(window.measure), duration);
            assert!(!measurement.admits(window.end));
            stats.request_result(late_start, &measurement, &Ok(false));
            stats.request_result(window.measure, &measurement, &Ok(false));
            assert_eq!((stats.count, stats.warmup_count), (1, 2));
        })
        .await
        .unwrap();
    }

    #[test]
    fn inputs_are_explicit_and_bounded() {
        let parsed = options(&["--connections", "16", "--mode", "fresh"]).unwrap();
        assert_eq!(parsed.size, 4096);
        assert!(parsed.fresh);
        for args in [
            vec!["--connections", "0"],
            vec!["--connections", "257"],
            vec!["--duration-ms", "0"],
            vec!["--timeout-ms", "60001"],
            vec!["--mode", "pool"],
            vec!["--size", "4096"],
            vec!["--unknown", "x"],
        ] {
            assert!(options(&args).is_err(), "{args:?}");
        }
        for url in [
            "https://127.0.0.1:80/",
            "http://localhost:80/",
            "http://127.0.0.1/",
            "http://127.0.0.1:0/",
        ] {
            assert!(Options::parse(["--url", url, "--size", "0"].map(str::to_owned)).is_err());
        }
    }

    #[test]
    fn histogram_is_bounded_mergeable_and_reports_overflow() {
        let mut histogram = Histogram::default();
        assert_eq!(histogram.percentile_us(50), None);
        histogram.record(Duration::from_nanos(16));
        histogram.record(Duration::from_nanos(31));
        assert_eq!(histogram.percentile_us(50), Some(0.0165));
        assert_eq!(histogram.percentile_us(99), Some(0.0315));
        let mut other = Histogram::default();
        other.record(Duration::from_secs(u64::MAX));
        histogram.merge(&other);
        assert_eq!(histogram.samples, 3);
        assert_eq!(histogram.overflows, 1);
        assert_eq!(histogram.buckets.iter().sum::<u64>(), 3);
        let mut rounded = Histogram::default();
        rounded.record(Duration::from_nanos(32));
        assert_eq!(rounded.percentile_us(50), Some(0.033));
    }

    #[test]
    fn merged_error_diagnostics_remain_bounded() {
        let mut total = Stats::default();
        for _ in 0..4 {
            let mut worker = Stats::default();
            for _ in 0..32 {
                worker.error("x".repeat(512));
            }
            total.merge(worker);
        }
        assert_eq!(total.errors, 128);
        assert_eq!(total.details.len(), MAX_ERROR_DETAILS);
        assert!(total.details.iter().all(|detail| detail.len() == 256));
    }

    #[test]
    fn errors_in_any_phase_invalidate_throughput() {
        assert_eq!(requests_per_second(10, 0, 2.0), Some(5.0));
        assert_eq!(requests_per_second(10, 1, 2.0), None);
        assert_eq!(requests_per_second(0, 1, 0.0), None);
        assert_eq!(requests_per_second(10, 0, 0.0), None);
    }

    #[test]
    fn payload_validation_rejects_wrong_bytes_and_excess() {
        let mut received = 0;
        validate_chunk(b"xx", &mut received, 3).unwrap();
        assert!(validate_chunk(b"z", &mut received, 3).is_err());
        assert!(validate_chunk(b"xx", &mut received, 3).is_err());
        validate_chunk(b"x", &mut received, 3).unwrap();
        assert_eq!(received, 3);
    }
}
