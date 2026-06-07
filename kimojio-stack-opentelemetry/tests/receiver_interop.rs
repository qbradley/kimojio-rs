// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(not(feature = "tls"), allow(dead_code))]

use std::{
    fs,
    net::{Ipv4Addr, SocketAddrV4, TcpStream},
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use kimojio_stack::{AddressFamily, Runtime, RuntimeContext, SocketType, ipproto};
use kimojio_stack_http::HttpConfig;
use kimojio_stack_opentelemetry::{
    AnyValue, ExportClientConfig, GaugeDataPoint, InstrumentationScope, KeyValue, LogBatch,
    LogRecord, LogsClient, MetricBatch, MetricsClient, MonotonicSumDataPoint, NumberValue,
    Resource, SeverityNumber,
};
use serde_json::Value;
use tempfile::TempDir;

const ENABLE_RECEIVER_TESTS: &str = "KIMOJIO_STACK_OTEL_RECEIVER_TESTS";
const COLLECTOR_IMAGE: &str = "otel/opentelemetry-collector-contrib:0.114.0";

#[test]
fn collector_plaintext_receives_logs_and_metrics_when_enabled() {
    let Some(_guard) = opt_in() else {
        return;
    };
    let Some(collector) = DockerCollector::start(false) else {
        return;
    };

    Runtime::new().block_on(|cx| {
        export_logs_plaintext(cx, collector.port);
        export_metrics_plaintext(cx, collector.port);
    });

    collector.assert_output_contains_contract(
        &[
            "service.name",
            "kimojio-stack-opentelemetry-test",
            "kimojio-stack-opentelemetry-tests",
            "kimojio-log-line",
            "INFO",
            "receiver",
            "kimojio.requests",
            "kimojio.gauge",
        ],
        &[1.0, 2.0, 7.0, 42.0],
    );
}

#[cfg(feature = "tls")]
#[test]
fn collector_tls_receives_logs_and_metrics_when_enabled() {
    let Some(_guard) = opt_in() else {
        return;
    };
    let Some(collector) = DockerCollector::start(true) else {
        return;
    };

    Runtime::new().block_on(|cx| {
        export_logs_tls(cx, &collector);
        export_metrics_tls(cx, &collector);
    });

    collector.assert_output_contains_contract(
        &[
            "service.name",
            "kimojio-stack-opentelemetry-test",
            "kimojio-stack-opentelemetry-tests",
            "kimojio-log-line",
            "INFO",
            "receiver",
            "kimojio.requests",
            "kimojio.gauge",
        ],
        &[1.0, 2.0, 7.0, 42.0],
    );
}

#[test]
fn connection_unavailable_returns_explicit_error() {
    let result = Runtime::new().block_on(|cx| {
        let socket = cx
            .socket(AddressFamily::INET, SocketType::STREAM, Some(ipproto::TCP))
            .unwrap();
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
        cx.connect(&socket, &addr)
    });

    assert!(result.is_err());
}

#[test]
fn mid_export_close_returns_explicit_error() {
    let (client_fd, server_fd) = rustix::net::socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        rustix::net::SocketFlags::CLOEXEC,
        None,
    )
    .unwrap();

    let result = Runtime::new().block_on(|cx| {
        cx.scope(|scope| {
            let closer = scope.spawn(move |cx| {
                let mut byte = [0_u8; 1];
                let _ = cx.read(&server_fd, &mut byte).unwrap();
                cx.close(server_fd).unwrap();
            });
            let client = scope.spawn(move |cx| {
                let mut client = LogsClient::plaintext(
                    client_fd,
                    HttpConfig::default(),
                    ExportClientConfig::default(),
                );
                client.export(cx, log_batch())
            });

            let result = client.join(cx).unwrap();
            closer.join(cx).unwrap();
            result
        })
    });

    assert!(result.is_err());
}

fn opt_in() -> Option<()> {
    if std::env::var(ENABLE_RECEIVER_TESTS).ok().as_deref() == Some("1") {
        Some(())
    } else {
        eprintln!("skipping receiver interop; set {ENABLE_RECEIVER_TESTS}=1 to run");
        None
    }
}

fn export_logs_plaintext(cx: &RuntimeContext<'_>, port: u16) {
    let socket = connect_stackful(cx, port);
    let mut client =
        LogsClient::plaintext(socket, HttpConfig::default(), ExportClientConfig::default());
    let result = client.export(cx, log_batch()).unwrap();
    assert_eq!(result.rejected_log_records(), 0);
    client.finish(cx).unwrap();
}

fn export_metrics_plaintext(cx: &RuntimeContext<'_>, port: u16) {
    let socket = connect_stackful(cx, port);
    let mut client =
        MetricsClient::plaintext(socket, HttpConfig::default(), ExportClientConfig::default());
    let result = client.export(cx, metric_batch()).unwrap();
    assert_eq!(result.rejected_data_points(), 0);
    client.finish(cx).unwrap();
}

#[cfg(feature = "tls")]
fn export_logs_tls(cx: &RuntimeContext<'_>, collector: &DockerCollector) {
    use kimojio_stack_http::{h2, tls};

    let socket = connect_stackful(cx, collector.port);
    let transport = tls::client_transport(
        cx,
        &collector.client_tls_context(),
        16 * 1024,
        socket,
        "localhost",
        Some(tls::HttpProtocol::Http2),
    )
    .unwrap();
    let http = h2::ClientConnection::new(transport, HttpConfig::default());
    let mut client = LogsClient::new(http, ExportClientConfig::default());
    let result = client.export(cx, log_batch()).unwrap();
    assert_eq!(result.rejected_log_records(), 0);
    client.finish(cx).unwrap();
}

#[cfg(feature = "tls")]
fn export_metrics_tls(cx: &RuntimeContext<'_>, collector: &DockerCollector) {
    use kimojio_stack_http::{h2, tls};

    let socket = connect_stackful(cx, collector.port);
    let transport = tls::client_transport(
        cx,
        &collector.client_tls_context(),
        16 * 1024,
        socket,
        "localhost",
        Some(tls::HttpProtocol::Http2),
    )
    .unwrap();
    let http = h2::ClientConnection::new(transport, HttpConfig::default());
    let mut client = MetricsClient::new(http, ExportClientConfig::default());
    let result = client.export(cx, metric_batch()).unwrap();
    assert_eq!(result.rejected_data_points(), 0);
    client.finish(cx).unwrap();
}

fn connect_stackful(cx: &RuntimeContext<'_>, port: u16) -> kimojio_stack::OwnedFd {
    let socket = cx
        .socket(AddressFamily::INET, SocketType::STREAM, Some(ipproto::TCP))
        .unwrap();
    let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
    cx.connect(&socket, &addr).unwrap();
    socket
}

fn log_batch() -> LogBatch {
    LogBatch::new(resource(), scope()).with_record(
        LogRecord::new(
            1,
            SeverityNumber::Info,
            AnyValue::String("kimojio-log-line".into()),
        )
        .with_severity_text("INFO")
        .with_attribute(KeyValue::new("kind", AnyValue::String("receiver".into()))),
    )
}

fn metric_batch() -> MetricBatch {
    MetricBatch::new(resource(), scope())
        .with_monotonic_sum(MonotonicSumDataPoint::new(
            "kimojio.requests",
            1,
            2,
            NumberValue::I64(7),
        ))
        .with_gauge(GaugeDataPoint::new(
            "kimojio.gauge",
            2,
            NumberValue::F64(42.0),
        ))
}

fn resource() -> Resource {
    Resource::new().with_attribute(KeyValue::new(
        "service.name",
        AnyValue::String("kimojio-stack-opentelemetry-test".into()),
    ))
}

fn scope() -> InstrumentationScope {
    InstrumentationScope::new("kimojio-stack-opentelemetry-tests")
}

struct DockerCollector {
    name: String,
    port: u16,
    temp: TempDir,
}

impl DockerCollector {
    fn start(tls: bool) -> Option<Self> {
        if !Command::new("docker")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
        {
            eprintln!("skipping receiver interop; docker CLI is unavailable");
            return None;
        }

        let temp = TempDir::new().unwrap();
        let name = format!(
            "kimojio-stack-otel-{}-{}",
            std::process::id(),
            if tls { "tls" } else { "plain" }
        );
        #[cfg(feature = "tls")]
        if tls {
            generate_tls_material(temp.path());
        }
        #[cfg(not(feature = "tls"))]
        assert!(!tls, "TLS receiver tests require the tls feature");
        write_collector_config(temp.path(), tls);

        let mut command = Command::new("docker");
        command
            .args(["run", "--rm", "-d", "--name", &name])
            .args(["-p", "127.0.0.1::4317"])
            .args(["-v", &format!("{}:/cfg:ro", temp.path().display())])
            .args(["-v", &format!("{}:/out", temp.path().display())])
            .arg(COLLECTOR_IMAGE)
            .arg("--config=/cfg/otel.yaml");
        let output = command.output().unwrap();
        if !output.status.success() {
            eprintln!(
                "skipping receiver interop; failed to start collector: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            return None;
        }

        let port = docker_mapped_port(&name);
        wait_for_port(port);
        Some(Self { name, port, temp })
    }

    fn assert_output_contains_contract(&self, strings: &[&str], numbers: &[f64]) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let output = self.temp.path().join("otel.json");
        loop {
            if let Ok(contents) = fs::read_to_string(&output)
                && json_output_contains_contract(&contents, strings, numbers)
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "collector output did not contain expected strings {strings:?} and numbers {numbers:?}; current output: {:?}",
                fs::read_to_string(&output).ok()
            );
            thread::sleep(Duration::from_millis(100));
        }
    }

    #[cfg(feature = "tls")]
    fn client_tls_context(&self) -> kimojio_stack_tls::TlsContext {
        use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};

        let mut connector = SslConnector::builder(SslMethod::tls()).unwrap();
        connector.set_verify(SslVerifyMode::PEER);
        connector
            .set_ca_file(self.temp.path().join("server.crt"))
            .unwrap();
        connector.set_alpn_protos(b"\x02h2").unwrap();
        kimojio_stack_tls::TlsContext::from_openssl(connector.build().into_context())
    }
}

fn docker_mapped_port(name: &str) -> u16 {
    let output = Command::new("docker")
        .args(["port", name, "4317/tcp"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "failed to inspect collector port: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = String::from_utf8_lossy(&output.stdout);
    output
        .lines()
        .find_map(|line| {
            line.rsplit_once(':')
                .and_then(|(_, port)| port.parse().ok())
        })
        .expect("collector port mapping missing")
}

fn json_output_contains_contract(contents: &str, strings: &[&str], numbers: &[f64]) -> bool {
    let values: Vec<Value> = serde_json::from_str(contents)
        .map(|value| vec![value])
        .unwrap_or_else(|_| {
            contents
                .lines()
                .filter(|line| !line.trim().is_empty())
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect()
        });
    if values.is_empty() {
        return false;
    }

    strings.iter().all(|expected| {
        values
            .iter()
            .any(|value| json_contains_string(value, expected))
    }) && numbers.iter().all(|expected| {
        values
            .iter()
            .any(|value| json_contains_number(value, *expected))
    })
}

fn json_contains_string(value: &Value, expected: &str) -> bool {
    match value {
        Value::String(value) => value == expected,
        Value::Array(values) => values
            .iter()
            .any(|value| json_contains_string(value, expected)),
        Value::Object(values) => values
            .values()
            .any(|value| json_contains_string(value, expected)),
        _ => false,
    }
}

fn json_contains_number(value: &Value, expected: f64) -> bool {
    match value {
        Value::Number(value) => value
            .as_f64()
            .is_some_and(|actual| (actual - expected).abs() < f64::EPSILON),
        Value::Array(values) => values
            .iter()
            .any(|value| json_contains_number(value, expected)),
        Value::Object(values) => values
            .values()
            .any(|value| json_contains_number(value, expected)),
        _ => false,
    }
}

impl Drop for DockerCollector {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn write_collector_config(root: &Path, tls: bool) {
    let tls_config = if tls {
        "\n        tls:\n          cert_file: /cfg/server.crt\n          key_file: /cfg/server.key"
    } else {
        ""
    };
    fs::write(
        root.join("otel.yaml"),
        format!(
            r#"receivers:
  otlp:
    protocols:
      grpc:
        endpoint: 0.0.0.0:4317{tls_config}
exporters:
  file:
    path: /out/otel.json
service:
  pipelines:
    logs:
      receivers: [otlp]
      exporters: [file]
    metrics:
      receivers: [otlp]
      exporters: [file]
"#
        ),
    )
    .unwrap();
}

fn wait_for_port(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(20);
    let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port).into();
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("collector did not listen on port {port}");
}

#[cfg(feature = "tls")]
fn generate_tls_material(root: &Path) {
    let cert = root.join("server.crt");
    let key = root.join("server.key");
    let status = Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-subj",
            "/CN=localhost",
            "-addext",
            "subjectAltName=DNS:localhost,IP:127.0.0.1",
            "-days",
            "1",
            "-keyout",
        ])
        .arg(&key)
        .arg("-out")
        .arg(&cert)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "failed to generate TLS material");
}
