// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::{
    env,
    net::SocketAddr,
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use bytes::Bytes;

use super::{
    launch::{
        GoHostConfig, LocalGateway, LocalRuntime, TcpGateway, go_object_gateway_dir, launch_go_host,
    },
    model::{
        NamespaceMapping, ObjectErrorClass, ObjectOperation, ObjectSizeLimits, StorageFailureClass,
        TelemetryDiagnosticOutcome,
    },
    proto::{self, ObjectStatusCode, OperationStatus},
    stackful::StackfulGatewayConfig,
    telemetry::HermeticTelemetrySink,
    tokio_impl::{TokioGateway, TokioGatewayConfig},
};

/// Expected outcome category for a conformance scenario.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExpectedOutcome {
    Pass,
    Status(ObjectStatusCode),
}

/// One row in the shared conformance matrix.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScenarioSpec {
    pub label: &'static str,
    kind: ScenarioKind,
    pub expected: ExpectedOutcome,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ScenarioOutcome {
    detail: String,
    status: Option<ObjectStatusCode>,
}

impl ScenarioOutcome {
    fn pass(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
            status: None,
        }
    }

    fn status(status: ObjectStatusCode, detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
            status: Some(status),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScenarioKind {
    EmptyObject,
    StreamingLargeObject,
    LargeObjectSizeLimit,
    MissingObject,
    Overwrite,
    ListEmptyNamespace,
    CopyMissingSource,
    DeleteIdempotency,
    StorageTimeout,
    StorageRetriable,
    StorageNonRetriable,
    TelemetrySuccessRecords,
    TelemetryExportFailureDiagnostic,
    Deadline,
    Cancellation,
    ConcurrentIndependentNames,
    GrpcHealth,
    HttpAdminHealthStatus,
    FollowUpHealth,
}

/// Shared expected-result table used for every enabled implementation.
pub const DEFAULT_SCENARIOS: &[ScenarioSpec] = &[
    ScenarioSpec {
        label: "empty-object",
        kind: ScenarioKind::EmptyObject,
        expected: ExpectedOutcome::Status(ObjectStatusCode::Ok),
    },
    ScenarioSpec {
        label: "streaming-large-object",
        kind: ScenarioKind::StreamingLargeObject,
        expected: ExpectedOutcome::Status(ObjectStatusCode::Ok),
    },
    ScenarioSpec {
        label: "large-object-size-limit",
        kind: ScenarioKind::LargeObjectSizeLimit,
        expected: ExpectedOutcome::Status(ObjectStatusCode::SizeLimit),
    },
    ScenarioSpec {
        label: "missing-object",
        kind: ScenarioKind::MissingObject,
        expected: ExpectedOutcome::Status(ObjectStatusCode::NotFound),
    },
    ScenarioSpec {
        label: "overwrite",
        kind: ScenarioKind::Overwrite,
        expected: ExpectedOutcome::Status(ObjectStatusCode::Ok),
    },
    ScenarioSpec {
        label: "list-empty-namespace",
        kind: ScenarioKind::ListEmptyNamespace,
        expected: ExpectedOutcome::Status(ObjectStatusCode::Ok),
    },
    ScenarioSpec {
        label: "copy-missing-source",
        kind: ScenarioKind::CopyMissingSource,
        expected: ExpectedOutcome::Status(ObjectStatusCode::NotFound),
    },
    ScenarioSpec {
        label: "delete-idempotency",
        kind: ScenarioKind::DeleteIdempotency,
        expected: ExpectedOutcome::Pass,
    },
    ScenarioSpec {
        label: "storage-timeout",
        kind: ScenarioKind::StorageTimeout,
        expected: ExpectedOutcome::Status(ObjectStatusCode::StorageTimeout),
    },
    ScenarioSpec {
        label: "storage-retriable",
        kind: ScenarioKind::StorageRetriable,
        expected: ExpectedOutcome::Status(ObjectStatusCode::StorageRetriable),
    },
    ScenarioSpec {
        label: "storage-non-retriable",
        kind: ScenarioKind::StorageNonRetriable,
        expected: ExpectedOutcome::Status(ObjectStatusCode::StorageNonRetriable),
    },
    ScenarioSpec {
        label: "telemetry-success-records",
        kind: ScenarioKind::TelemetrySuccessRecords,
        expected: ExpectedOutcome::Pass,
    },
    ScenarioSpec {
        label: "telemetry-export-failure-diagnostic",
        kind: ScenarioKind::TelemetryExportFailureDiagnostic,
        expected: ExpectedOutcome::Status(ObjectStatusCode::Ok),
    },
    ScenarioSpec {
        label: "deadline",
        kind: ScenarioKind::Deadline,
        expected: ExpectedOutcome::Pass,
    },
    ScenarioSpec {
        label: "cancellation",
        kind: ScenarioKind::Cancellation,
        expected: ExpectedOutcome::Pass,
    },
    ScenarioSpec {
        label: "concurrent-independent-names",
        kind: ScenarioKind::ConcurrentIndependentNames,
        expected: ExpectedOutcome::Pass,
    },
    ScenarioSpec {
        label: "grpc-health",
        kind: ScenarioKind::GrpcHealth,
        expected: ExpectedOutcome::Pass,
    },
    ScenarioSpec {
        label: "http-admin-health-status",
        kind: ScenarioKind::HttpAdminHealthStatus,
        expected: ExpectedOutcome::Pass,
    },
    ScenarioSpec {
        label: "follow-up-health",
        kind: ScenarioKind::FollowUpHealth,
        expected: ExpectedOutcome::Pass,
    },
];

/// Per-scenario conformance result emitted by the runner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConformanceResult {
    pub implementation: String,
    pub scenario: String,
    pub passed: bool,
    pub detail: String,
}

/// Conformance report for one implementation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConformanceReport {
    pub implementation: String,
    pub results: Vec<ConformanceResult>,
}

impl ConformanceReport {
    pub fn passed(&self) -> bool {
        self.results.iter().all(|result| result.passed)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoConformanceOutcome {
    Skipped(String),
    Ran(ConformanceReport),
}

trait GatewayOps {
    fn put(&self, namespace: &str, object: &str, chunks: Vec<Bytes>) -> Result<proto::PutResponse>;
    fn get(
        &self,
        namespace: &str,
        object: &str,
        max_chunk_bytes: u32,
    ) -> Result<Vec<proto::GetChunk>>;
    fn delete(&self, namespace: &str, object: &str) -> Result<proto::DeleteResponse>;
    fn list(&self, namespace: &str, prefix: &str) -> Result<proto::ListResponse>;
    fn copy(&self, namespace: &str, source: &str, destination: &str)
    -> Result<proto::CopyResponse>;
    fn grpc_health(&self) -> Result<proto::HealthResponse>;
    fn admin_health(&self) -> Result<String>;
    fn admin_status(&self) -> Result<String>;
    fn cancel_get_after_first_chunk(&self, namespace: &str, object: &str) -> Result<()>;
    fn idle_grpc_request_times_out(&self) -> Result<()>;
    fn put_pair_concurrently(
        &self,
        namespace: &str,
        first: (&str, Bytes),
        second: (&str, Bytes),
    ) -> Result<(proto::PutResponse, proto::PutResponse)>;
    fn inject_storage_failure(&self, failure: StorageFailureClass);
    fn fail_next_telemetry_export(&self, message: &str);
    fn telemetry(&self) -> HermeticTelemetrySink;
}

impl GatewayOps for LocalGateway {
    fn put(&self, namespace: &str, object: &str, chunks: Vec<Bytes>) -> Result<proto::PutResponse> {
        self.put(namespace, object, chunks)
    }

    fn get(
        &self,
        namespace: &str,
        object: &str,
        max_chunk_bytes: u32,
    ) -> Result<Vec<proto::GetChunk>> {
        self.get(namespace, object, max_chunk_bytes)
    }

    fn delete(&self, namespace: &str, object: &str) -> Result<proto::DeleteResponse> {
        self.delete(namespace, object)
    }

    fn list(&self, namespace: &str, prefix: &str) -> Result<proto::ListResponse> {
        self.list(namespace, prefix)
    }

    fn copy(
        &self,
        namespace: &str,
        source: &str,
        destination: &str,
    ) -> Result<proto::CopyResponse> {
        self.copy(namespace, source, destination)
    }

    fn grpc_health(&self) -> Result<proto::HealthResponse> {
        self.grpc_health()
    }

    fn admin_health(&self) -> Result<String> {
        self.admin_health()
    }

    fn admin_status(&self) -> Result<String> {
        self.admin_status()
    }

    fn cancel_get_after_first_chunk(&self, namespace: &str, object: &str) -> Result<()> {
        self.cancel_get_after_first_chunk(namespace, object)
    }

    fn idle_grpc_request_times_out(&self) -> Result<()> {
        self.idle_grpc_request_times_out()
    }

    fn put_pair_concurrently(
        &self,
        namespace: &str,
        first: (&str, Bytes),
        second: (&str, Bytes),
    ) -> Result<(proto::PutResponse, proto::PutResponse)> {
        self.put_pair_concurrently(namespace, first, second)
    }

    fn inject_storage_failure(&self, failure: StorageFailureClass) {
        self.inject_storage_failure(failure);
    }

    fn fail_next_telemetry_export(&self, message: &str) {
        self.fail_next_telemetry_export(message);
    }

    fn telemetry(&self) -> HermeticTelemetrySink {
        self.telemetry()
    }
}

impl GatewayOps for TokioGateway {
    fn put(&self, namespace: &str, object: &str, chunks: Vec<Bytes>) -> Result<proto::PutResponse> {
        self.put(namespace, object, chunks)
    }

    fn get(
        &self,
        namespace: &str,
        object: &str,
        max_chunk_bytes: u32,
    ) -> Result<Vec<proto::GetChunk>> {
        self.get(namespace, object, max_chunk_bytes)
    }

    fn delete(&self, namespace: &str, object: &str) -> Result<proto::DeleteResponse> {
        self.delete(namespace, object)
    }

    fn list(&self, namespace: &str, prefix: &str) -> Result<proto::ListResponse> {
        self.list(namespace, prefix)
    }

    fn copy(
        &self,
        namespace: &str,
        source: &str,
        destination: &str,
    ) -> Result<proto::CopyResponse> {
        self.copy(namespace, source, destination)
    }

    fn grpc_health(&self) -> Result<proto::HealthResponse> {
        self.grpc_health()
    }

    fn admin_health(&self) -> Result<String> {
        self.admin_health()
    }

    fn admin_status(&self) -> Result<String> {
        self.admin_status()
    }

    fn cancel_get_after_first_chunk(&self, namespace: &str, object: &str) -> Result<()> {
        self.cancel_get_after_first_chunk(namespace, object)
    }

    fn idle_grpc_request_times_out(&self) -> Result<()> {
        self.idle_grpc_request_times_out()
    }

    fn put_pair_concurrently(
        &self,
        namespace: &str,
        first: (&str, Bytes),
        second: (&str, Bytes),
    ) -> Result<(proto::PutResponse, proto::PutResponse)> {
        self.put_pair_concurrently(namespace, first, second)
    }

    fn inject_storage_failure(&self, failure: StorageFailureClass) {
        self.inject_storage_failure(failure);
    }

    fn fail_next_telemetry_export(&self, message: &str) {
        self.fail_next_telemetry_export(message);
    }

    fn telemetry(&self) -> HermeticTelemetrySink {
        self.telemetry()
    }
}

/// Runs the default hermetic matrix for stackful, stack-steal, and Tokio implementations.
pub fn run_default_implementations() -> Vec<ConformanceReport> {
    let mut reports = [LocalRuntime::Stack, LocalRuntime::StackSteal]
        .into_iter()
        .map(run_implementation)
        .collect::<Vec<_>>();
    reports.push(run_tokio_implementation());
    reports
}

/// Runs the shared conformance matrix for one local implementation.
pub fn run_implementation(runtime: LocalRuntime) -> ConformanceReport {
    run_implementation_scenarios(runtime, DEFAULT_SCENARIOS)
}

/// Runs the shared conformance matrix for the Tokio comparison implementation.
pub fn run_tokio_implementation() -> ConformanceReport {
    run_tokio_implementation_scenarios(DEFAULT_SCENARIOS)
}

/// Runs selected scenarios for the Tokio comparison implementation.
pub fn run_tokio_implementation_scenarios(scenarios: &[ScenarioSpec]) -> ConformanceReport {
    let results = DEFAULT_SCENARIOS
        .iter()
        .filter(|scenario| {
            scenarios
                .iter()
                .any(|selected| selected.label == scenario.label)
        })
        .map(|scenario| run_tokio_one(*scenario))
        .collect();
    ConformanceReport {
        implementation: "tokio".to_owned(),
        results,
    }
}

/// Runs protocol-visible scenarios against an already-running TCP target.
pub fn run_target_implementation(
    implementation: impl Into<String>,
    grpc_addr: SocketAddr,
    admin_addr: SocketAddr,
) -> ConformanceReport {
    let scenarios = target_supported_scenarios();
    run_target_implementation_scenarios(implementation, grpc_addr, admin_addr, &scenarios)
}

/// Runs selected scenarios against an already-running TCP target.
pub fn run_target_implementation_scenarios(
    implementation: impl Into<String>,
    grpc_addr: SocketAddr,
    admin_addr: SocketAddr,
    scenarios: &[ScenarioSpec],
) -> ConformanceReport {
    let implementation = implementation.into();
    let gateway = TcpGateway::new(grpc_addr, admin_addr);
    let results = DEFAULT_SCENARIOS
        .iter()
        .filter(|scenario| {
            scenarios
                .iter()
                .any(|selected| selected.label == scenario.label)
        })
        .map(|scenario| run_target_one(&implementation, &gateway, *scenario))
        .collect();
    ConformanceReport {
        implementation,
        results,
    }
}

/// Environment variable that enables the optional Go comparison conformance test.
pub const GO_CONFORMANCE_ENV: &str = "KIMOJIO_OBJECT_GATEWAY_GO";

/// Explains why the optional Go conformance test should be skipped, if it should.
pub fn go_conformance_skip_reason() -> Option<String> {
    match env::var(GO_CONFORMANCE_ENV) {
        Ok(value) if value == "1" => {}
        _ => {
            return Some(format!(
                "set {GO_CONFORMANCE_ENV}=1 to build and launch the optional Go comparison host"
            ));
        }
    }
    let module_dir = go_object_gateway_dir();
    if !module_dir.join("go.mod").is_file() {
        return Some(format!(
            "Go object gateway module is missing at {}",
            module_dir.display()
        ));
    }
    match Command::new("go")
        .arg("version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) if status.success() => None,
        Ok(status) => Some(format!("go version exited with status {status}")),
        Err(error) => Some(format!("go toolchain unavailable: {error}")),
    }
}

/// Builds, launches, and tests the optional Go comparison host when enabled.
pub fn run_go_conformance_if_enabled() -> Result<GoConformanceOutcome> {
    if let Some(reason) = go_conformance_skip_reason() {
        return Ok(GoConformanceOutcome::Skipped(reason));
    }
    run_go_conformance().map(GoConformanceOutcome::Ran)
}

/// Returns the shared matrix rows that can run through only public gRPC/admin protocols.
pub fn target_supported_scenarios() -> Vec<ScenarioSpec> {
    DEFAULT_SCENARIOS
        .iter()
        .copied()
        .filter(|scenario| scenario.kind.is_target_supported())
        .collect()
}

/// Rows covered by the optional Go comparison host over public TCP surfaces.
pub fn go_supported_scenarios() -> Vec<ScenarioSpec> {
    DEFAULT_SCENARIOS
        .iter()
        .copied()
        .filter(|scenario| {
            scenario.kind.is_target_supported()
                || matches!(
                    scenario.kind,
                    ScenarioKind::Deadline
                        | ScenarioKind::Cancellation
                        | ScenarioKind::ConcurrentIndependentNames
                )
        })
        .collect()
}

fn run_go_conformance() -> Result<ConformanceReport> {
    let host = launch_go_host(GoHostConfig {
        max_object_bytes: 16,
        max_chunk_bytes: 4,
        request_deadline_ms: 50,
    })?;
    Ok(run_target_implementation_scenarios(
        "go",
        host.grpc_addr,
        host.admin_addr,
        &go_supported_scenarios(),
    ))
}

/// Runs a selected scenario subset for one local implementation.
pub fn run_implementation_scenarios(
    runtime: LocalRuntime,
    scenarios: &[ScenarioSpec],
) -> ConformanceReport {
    let results = DEFAULT_SCENARIOS
        .iter()
        .filter(|scenario| {
            scenarios
                .iter()
                .any(|selected| selected.label == scenario.label)
        })
        .map(|scenario| run_one(runtime, *scenario))
        .collect();
    ConformanceReport {
        implementation: runtime.label().to_owned(),
        results,
    }
}

/// Formats stable key-value output for CLI and workload tooling.
pub fn format_reports(reports: &[ConformanceReport]) -> String {
    let mut output = String::new();
    for report in reports {
        for result in &report.results {
            let status = if result.passed { "pass" } else { "fail" };
            output.push_str(&format!(
                "implementation={} scenario={} status={} detail={}\n",
                result.implementation, result.scenario, status, result.detail
            ));
        }
    }
    output
}

fn run_one(runtime: LocalRuntime, scenario: ScenarioSpec) -> ConformanceResult {
    let implementation = runtime.label().to_owned();
    let gateway = LocalGateway::new(runtime, config_for(scenario.kind));
    let result = run_scenario(&gateway, scenario)
        .with_context(|| format!("{} {}", runtime.label(), scenario.label));
    match result {
        Ok(outcome) => result_from_outcome(implementation, scenario, outcome),
        Err(error) => ConformanceResult {
            implementation,
            scenario: scenario.label.to_owned(),
            passed: false,
            detail: format!("{error:#}").replace('\n', " "),
        },
    }
}

fn run_tokio_one(scenario: ScenarioSpec) -> ConformanceResult {
    let gateway = TokioGateway::new(tokio_config_for(scenario.kind));
    let result =
        run_scenario(&gateway, scenario).with_context(|| format!("tokio {}", scenario.label));
    match result {
        Ok(outcome) => result_from_outcome("tokio".to_owned(), scenario, outcome),
        Err(error) => ConformanceResult {
            implementation: "tokio".to_owned(),
            scenario: scenario.label.to_owned(),
            passed: false,
            detail: format!("{error:#}").replace('\n', " "),
        },
    }
}

fn run_target_one(
    implementation: &str,
    gateway: &TcpGateway,
    scenario: ScenarioSpec,
) -> ConformanceResult {
    let result = run_target_scenario(gateway, scenario)
        .with_context(|| format!("{implementation} {}", scenario.label));
    match result {
        Ok(outcome) => result_from_outcome(implementation.to_owned(), scenario, outcome),
        Err(error) => ConformanceResult {
            implementation: implementation.to_owned(),
            scenario: scenario.label.to_owned(),
            passed: false,
            detail: format!("{error:#}").replace('\n', " "),
        },
    }
}

fn result_from_outcome(
    implementation: String,
    scenario: ScenarioSpec,
    outcome: ScenarioOutcome,
) -> ConformanceResult {
    match validate_expected_outcome(scenario.expected, &outcome) {
        Ok(()) => ConformanceResult {
            implementation,
            scenario: scenario.label.to_owned(),
            passed: true,
            detail: outcome.detail,
        },
        Err(error) => ConformanceResult {
            implementation,
            scenario: scenario.label.to_owned(),
            passed: false,
            detail: format!("{error:#}").replace('\n', " "),
        },
    }
}

fn validate_expected_outcome(expected: ExpectedOutcome, outcome: &ScenarioOutcome) -> Result<()> {
    match expected {
        ExpectedOutcome::Pass => Ok(()),
        ExpectedOutcome::Status(expected) => {
            let actual = outcome
                .status
                .context("scenario did not report a primary status")?;
            ensure!(
                actual == expected,
                "expected primary status {expected:?}, got {actual:?}"
            );
            Ok(())
        }
    }
}

impl ScenarioKind {
    const fn is_target_supported(self) -> bool {
        matches!(
            self,
            Self::EmptyObject
                | Self::StreamingLargeObject
                | Self::LargeObjectSizeLimit
                | Self::MissingObject
                | Self::Overwrite
                | Self::ListEmptyNamespace
                | Self::CopyMissingSource
                | Self::DeleteIdempotency
                | Self::GrpcHealth
                | Self::HttpAdminHealthStatus
                | Self::FollowUpHealth
        )
    }
}

fn run_scenario(gateway: &impl GatewayOps, scenario: ScenarioSpec) -> Result<ScenarioOutcome> {
    let namespace = format!("cf-{}", scenario.label);
    match scenario.kind {
        ScenarioKind::EmptyObject => {
            let response = gateway.put(&namespace, "empty", vec![Bytes::new()])?;
            assert_status(response.status.as_ref(), ObjectStatusCode::Ok)?;
            let chunks = gateway.get(&namespace, "empty", 4)?;
            assert_status(first_chunk_status(&chunks), ObjectStatusCode::Ok)?;
            ensure!(
                chunks.iter().all(|chunk| chunk.data.is_empty()),
                "empty object returned non-empty data"
            );
            Ok(ScenarioOutcome::status(
                ObjectStatusCode::Ok,
                "empty object round-tripped",
            ))
        }
        ScenarioKind::StreamingLargeObject => {
            let chunks = vec![
                Bytes::from_static(b"abcd"),
                Bytes::from_static(b"efgh"),
                Bytes::from_static(b"ijkl"),
                Bytes::from_static(b"mnop"),
            ];
            ensure!(chunks.iter().all(|chunk| chunk.len() <= 4));
            assert_status(
                gateway.put(&namespace, "large", chunks)?.status.as_ref(),
                ObjectStatusCode::Ok,
            )?;
            let response_chunks = gateway.get(&namespace, "large", 4)?;
            assert_status(first_chunk_status(&response_chunks), ObjectStatusCode::Ok)?;
            ensure!(
                response_chunks.iter().all(|chunk| chunk.data.len() <= 4),
                "GET emitted chunk larger than configured limit"
            );
            let body = collect_body(&response_chunks);
            ensure!(body == b"abcdefghijklmnop", "large object body mismatch");
            Ok(ScenarioOutcome::status(
                ObjectStatusCode::Ok,
                format!(
                    "streamed {} bytes with max chunk {}",
                    body.len(),
                    response_chunks
                        .iter()
                        .map(|chunk| chunk.data.len())
                        .max()
                        .unwrap_or_default()
                ),
            ))
        }
        ScenarioKind::LargeObjectSizeLimit => {
            let response = gateway.put(
                &namespace,
                "too-large",
                vec![
                    Bytes::from_static(b"abcd"),
                    Bytes::from_static(b"efgh"),
                    Bytes::from_static(b"ijkl"),
                    Bytes::from_static(b"mnop"),
                    Bytes::from_static(b"q"),
                ],
            )?;
            assert_status(response.status.as_ref(), ObjectStatusCode::SizeLimit)?;
            assert_telemetry(
                gateway,
                ObjectOperation::Put,
                Some(ObjectErrorClass::SizeLimit),
            )?;
            assert_follow_up_ready(gateway)?;
            Ok(ScenarioOutcome::status(
                ObjectStatusCode::SizeLimit,
                "size limit returned and follow-up health passed",
            ))
        }
        ScenarioKind::MissingObject => {
            let chunks = gateway.get(&namespace, "missing", 4)?;
            assert_status(first_chunk_status(&chunks), ObjectStatusCode::NotFound)?;
            assert_telemetry(
                gateway,
                ObjectOperation::Get,
                Some(ObjectErrorClass::NotFound),
            )?;
            Ok(ScenarioOutcome::status(
                ObjectStatusCode::NotFound,
                "missing object mapped to not-found",
            ))
        }
        ScenarioKind::Overwrite => {
            assert_status(
                gateway
                    .put(&namespace, "same", vec![Bytes::from_static(b"old")])?
                    .status
                    .as_ref(),
                ObjectStatusCode::Ok,
            )?;
            assert_status(
                gateway
                    .put(&namespace, "same", vec![Bytes::from_static(b"new")])?
                    .status
                    .as_ref(),
                ObjectStatusCode::Ok,
            )?;
            let chunks = gateway.get(&namespace, "same", 4)?;
            ensure!(
                collect_body(&chunks) == b"new",
                "overwrite did not replace body"
            );
            Ok(ScenarioOutcome::status(
                ObjectStatusCode::Ok,
                "overwrite replaced object body",
            ))
        }
        ScenarioKind::ListEmptyNamespace => {
            let response = gateway.list(&namespace, "")?;
            assert_status(response.status.as_ref(), ObjectStatusCode::Ok)?;
            ensure!(
                response.objects.is_empty(),
                "empty namespace listed objects"
            );
            Ok(ScenarioOutcome::status(
                ObjectStatusCode::Ok,
                "empty namespace listed zero objects",
            ))
        }
        ScenarioKind::CopyMissingSource => {
            let response = gateway.copy(&namespace, "missing", "dst")?;
            assert_status(response.status.as_ref(), ObjectStatusCode::NotFound)?;
            Ok(ScenarioOutcome::status(
                ObjectStatusCode::NotFound,
                "missing copy source mapped to not-found",
            ))
        }
        ScenarioKind::DeleteIdempotency => {
            assert_status(
                gateway
                    .put(&namespace, "delete-me", vec![Bytes::from_static(b"x")])?
                    .status
                    .as_ref(),
                ObjectStatusCode::Ok,
            )?;
            assert_status(
                gateway.delete(&namespace, "delete-me")?.status.as_ref(),
                ObjectStatusCode::Ok,
            )?;
            assert_status(
                gateway.delete(&namespace, "delete-me")?.status.as_ref(),
                ObjectStatusCode::NotFound,
            )?;
            Ok(ScenarioOutcome::pass(
                "second delete deterministically returned not-found",
            ))
        }
        ScenarioKind::StorageTimeout => storage_failure_scenario(
            gateway,
            StorageFailureClass::Timeout,
            ObjectStatusCode::StorageTimeout,
            ObjectErrorClass::StorageTimeout,
        ),
        ScenarioKind::StorageRetriable => storage_failure_scenario(
            gateway,
            StorageFailureClass::Retriable,
            ObjectStatusCode::StorageRetriable,
            ObjectErrorClass::StorageRetriable,
        ),
        ScenarioKind::StorageNonRetriable => storage_failure_scenario(
            gateway,
            StorageFailureClass::NonRetriable,
            ObjectStatusCode::StorageNonRetriable,
            ObjectErrorClass::StorageNonRetriable,
        ),
        ScenarioKind::TelemetrySuccessRecords => {
            assert_status(
                gateway
                    .put(&namespace, "telemetry", vec![Bytes::from_static(b"ok")])?
                    .status
                    .as_ref(),
                ObjectStatusCode::Ok,
            )?;
            let chunks = gateway.get(&namespace, "telemetry", 4)?;
            assert_status(first_chunk_status(&chunks), ObjectStatusCode::Ok)?;
            let health = gateway.grpc_health()?;
            ensure!(health.ready, "gRPC health not ready");
            assert_telemetry(gateway, ObjectOperation::Put, None)?;
            assert_telemetry(gateway, ObjectOperation::Get, None)?;
            assert_telemetry(gateway, ObjectOperation::Health, None)?;
            let telemetry = gateway.telemetry();
            ensure!(
                telemetry.encoded_log_exports().len() >= 3
                    && telemetry.encoded_metric_exports().len() >= 3,
                "expected encoded OTLP log and metric exports"
            );
            Ok(ScenarioOutcome::pass(
                "success and health operations recorded logs, metrics, and OTLP bytes",
            ))
        }
        ScenarioKind::TelemetryExportFailureDiagnostic => {
            gateway.fail_next_telemetry_export("collector unavailable");
            let response = gateway.put(
                &namespace,
                "telemetry-failure",
                vec![Bytes::from_static(b"ok")],
            )?;
            assert_status(response.status.as_ref(), ObjectStatusCode::Ok)?;
            let telemetry = gateway.telemetry();
            ensure!(
                telemetry.diagnostics().iter().any(|diagnostic| matches!(
                    diagnostic,
                    TelemetryDiagnosticOutcome::ExportFailed(message)
                        if message == "collector unavailable"
                )),
                "expected telemetry export failure diagnostic"
            );
            Ok(ScenarioOutcome::status(
                ObjectStatusCode::Ok,
                "telemetry export failure was diagnostic-only",
            ))
        }
        ScenarioKind::Deadline => {
            gateway.idle_grpc_request_times_out()?;
            assert_follow_up_ready(gateway)?;
            Ok(ScenarioOutcome::pass(
                "deadline fired and follow-up health passed",
            ))
        }
        ScenarioKind::Cancellation => {
            assert_status(
                gateway
                    .put(
                        &namespace,
                        "cancel",
                        vec![
                            Bytes::from_static(b"abcd"),
                            Bytes::from_static(b"efgh"),
                            Bytes::from_static(b"ijkl"),
                        ],
                    )?
                    .status
                    .as_ref(),
                ObjectStatusCode::Ok,
            )?;
            gateway.cancel_get_after_first_chunk(&namespace, "cancel")?;
            assert_follow_up_ready(gateway)?;
            Ok(ScenarioOutcome::pass(
                "cancelled stream left follow-up health available",
            ))
        }
        ScenarioKind::ConcurrentIndependentNames => {
            let (first, second) = gateway.put_pair_concurrently(
                &namespace,
                ("a", Bytes::from_static(b"a")),
                ("b", Bytes::from_static(b"b")),
            )?;
            assert_status(first.status.as_ref(), ObjectStatusCode::Ok)?;
            assert_status(second.status.as_ref(), ObjectStatusCode::Ok)?;
            ensure!(collect_body(&gateway.get(&namespace, "a", 4)?) == b"a");
            ensure!(collect_body(&gateway.get(&namespace, "b", 4)?) == b"b");
            Ok(ScenarioOutcome::pass(
                "concurrent independent object names both succeeded",
            ))
        }
        ScenarioKind::GrpcHealth => {
            let health = gateway.grpc_health()?;
            ensure!(health.ready, "gRPC health not ready");
            Ok(ScenarioOutcome::pass(format!(
                "grpc health ready version={}",
                health.version
            )))
        }
        ScenarioKind::HttpAdminHealthStatus => {
            let health = gateway.admin_health()?;
            let status = gateway.admin_status()?;
            ensure!(health.contains("ready=true"), "admin /health not ready");
            ensure!(status.contains("ready=true"), "admin /status not ready");
            Ok(ScenarioOutcome::pass(
                "admin /health and /status reported ready",
            ))
        }
        ScenarioKind::FollowUpHealth => {
            assert_status(
                gateway
                    .put(&namespace, "follow-up", vec![Bytes::from_static(b"ok")])?
                    .status
                    .as_ref(),
                ObjectStatusCode::Ok,
            )?;
            let missing = gateway.get(&namespace, "missing", 4)?;
            assert_status(first_chunk_status(&missing), ObjectStatusCode::NotFound)?;
            assert_follow_up_ready(gateway)?;
            Ok(ScenarioOutcome::pass(
                "follow-up health succeeded after ordinary failure",
            ))
        }
    }
}

fn run_target_scenario(gateway: &TcpGateway, scenario: ScenarioSpec) -> Result<ScenarioOutcome> {
    if !(scenario.kind.is_target_supported()
        || matches!(
            scenario.kind,
            ScenarioKind::Deadline
                | ScenarioKind::Cancellation
                | ScenarioKind::ConcurrentIndependentNames
        ))
    {
        bail!(
            "scenario {} requires local fixture injection or inspection and cannot run against a TCP target",
            scenario.label
        );
    }
    let namespace = format!("cf-target-{}", scenario.label);
    match scenario.kind {
        ScenarioKind::EmptyObject => {
            let response = gateway.put(&namespace, "empty", vec![Bytes::new()])?;
            assert_status(response.status.as_ref(), ObjectStatusCode::Ok)?;
            let chunks = gateway.get(&namespace, "empty", 4)?;
            assert_status(first_chunk_status(&chunks), ObjectStatusCode::Ok)?;
            ensure!(
                chunks.iter().all(|chunk| chunk.data.is_empty()),
                "empty object returned non-empty data"
            );
            Ok(ScenarioOutcome::status(
                ObjectStatusCode::Ok,
                "empty object round-tripped through TCP target",
            ))
        }
        ScenarioKind::StreamingLargeObject => {
            let chunks = vec![
                Bytes::from_static(b"abcd"),
                Bytes::from_static(b"efgh"),
                Bytes::from_static(b"ijkl"),
                Bytes::from_static(b"mnop"),
            ];
            assert_status(
                gateway.put(&namespace, "large", chunks)?.status.as_ref(),
                ObjectStatusCode::Ok,
            )?;
            let response_chunks = gateway.get(&namespace, "large", 4)?;
            assert_status(first_chunk_status(&response_chunks), ObjectStatusCode::Ok)?;
            ensure!(
                response_chunks.iter().all(|chunk| chunk.data.len() <= 4),
                "GET emitted chunk larger than configured limit"
            );
            let body = collect_body(&response_chunks);
            ensure!(body == b"abcdefghijklmnop", "large object body mismatch");
            Ok(ScenarioOutcome::status(
                ObjectStatusCode::Ok,
                format!("streamed {} bytes through TCP target", body.len()),
            ))
        }
        ScenarioKind::LargeObjectSizeLimit => {
            let response = gateway.put(
                &namespace,
                "too-large",
                vec![
                    Bytes::from_static(b"abcd"),
                    Bytes::from_static(b"efgh"),
                    Bytes::from_static(b"ijkl"),
                    Bytes::from_static(b"mnop"),
                    Bytes::from_static(b"q"),
                ],
            )?;
            assert_status(response.status.as_ref(), ObjectStatusCode::SizeLimit)?;
            assert_target_follow_up_ready(gateway)?;
            Ok(ScenarioOutcome::status(
                ObjectStatusCode::SizeLimit,
                "size limit returned and target follow-up health passed",
            ))
        }
        ScenarioKind::MissingObject => {
            let chunks = gateway.get(&namespace, "missing", 4)?;
            assert_status(first_chunk_status(&chunks), ObjectStatusCode::NotFound)?;
            Ok(ScenarioOutcome::status(
                ObjectStatusCode::NotFound,
                "missing object mapped to not-found through TCP target",
            ))
        }
        ScenarioKind::Overwrite => {
            assert_status(
                gateway
                    .put(&namespace, "same", vec![Bytes::from_static(b"old")])?
                    .status
                    .as_ref(),
                ObjectStatusCode::Ok,
            )?;
            assert_status(
                gateway
                    .put(&namespace, "same", vec![Bytes::from_static(b"new")])?
                    .status
                    .as_ref(),
                ObjectStatusCode::Ok,
            )?;
            let chunks = gateway.get(&namespace, "same", 4)?;
            ensure!(
                collect_body(&chunks) == b"new",
                "overwrite did not replace body"
            );
            Ok(ScenarioOutcome::status(
                ObjectStatusCode::Ok,
                "overwrite replaced object body through TCP target",
            ))
        }
        ScenarioKind::ListEmptyNamespace => {
            let response = gateway.list(&namespace, "")?;
            assert_status(response.status.as_ref(), ObjectStatusCode::Ok)?;
            ensure!(
                response.objects.is_empty(),
                "empty namespace listed objects"
            );
            Ok(ScenarioOutcome::status(
                ObjectStatusCode::Ok,
                "empty namespace listed zero objects through TCP target",
            ))
        }
        ScenarioKind::CopyMissingSource => {
            let response = gateway.copy(&namespace, "missing", "dst")?;
            assert_status(response.status.as_ref(), ObjectStatusCode::NotFound)?;
            Ok(ScenarioOutcome::status(
                ObjectStatusCode::NotFound,
                "missing copy source mapped to not-found through TCP target",
            ))
        }
        ScenarioKind::DeleteIdempotency => {
            assert_status(
                gateway
                    .put(&namespace, "delete-me", vec![Bytes::from_static(b"x")])?
                    .status
                    .as_ref(),
                ObjectStatusCode::Ok,
            )?;
            assert_status(
                gateway.delete(&namespace, "delete-me")?.status.as_ref(),
                ObjectStatusCode::Ok,
            )?;
            assert_status(
                gateway.delete(&namespace, "delete-me")?.status.as_ref(),
                ObjectStatusCode::NotFound,
            )?;
            Ok(ScenarioOutcome::pass(
                "second delete deterministically returned not-found through TCP target",
            ))
        }
        ScenarioKind::GrpcHealth => {
            let health = gateway.grpc_health()?;
            ensure!(health.ready, "gRPC health not ready");
            Ok(ScenarioOutcome::pass(format!(
                "grpc health ready version={}",
                health.version
            )))
        }
        ScenarioKind::HttpAdminHealthStatus => {
            let health = gateway.admin_health()?;
            let status = gateway.admin_status()?;
            ensure!(health.contains("ready=true"), "admin /health not ready");
            ensure!(status.contains("ready=true"), "admin /status not ready");
            Ok(ScenarioOutcome::pass(
                "admin /health and /status reported ready through TCP target",
            ))
        }
        ScenarioKind::Deadline => {
            gateway.idle_grpc_request_times_out()?;
            assert_target_follow_up_ready(gateway)?;
            Ok(ScenarioOutcome::pass(
                "deadline fired and follow-up health passed through TCP target",
            ))
        }
        ScenarioKind::Cancellation => {
            assert_status(
                gateway
                    .put(
                        &namespace,
                        "cancel",
                        vec![
                            Bytes::from_static(b"abcd"),
                            Bytes::from_static(b"efgh"),
                            Bytes::from_static(b"ijkl"),
                        ],
                    )?
                    .status
                    .as_ref(),
                ObjectStatusCode::Ok,
            )?;
            gateway.cancel_get_after_first_chunk(&namespace, "cancel")?;
            assert_target_follow_up_ready(gateway)?;
            Ok(ScenarioOutcome::pass(
                "cancelled stream left follow-up health available through TCP target",
            ))
        }
        ScenarioKind::ConcurrentIndependentNames => {
            let (first, second) = gateway.put_pair_concurrently(
                &namespace,
                ("a", Bytes::from_static(b"a")),
                ("b", Bytes::from_static(b"b")),
            )?;
            assert_status(first.status.as_ref(), ObjectStatusCode::Ok)?;
            assert_status(second.status.as_ref(), ObjectStatusCode::Ok)?;
            ensure!(collect_body(&gateway.get(&namespace, "a", 4)?) == b"a");
            ensure!(collect_body(&gateway.get(&namespace, "b", 4)?) == b"b");
            Ok(ScenarioOutcome::pass(
                "concurrent independent object names both succeeded through TCP target",
            ))
        }
        ScenarioKind::FollowUpHealth => {
            assert_status(
                gateway
                    .put(&namespace, "follow-up", vec![Bytes::from_static(b"ok")])?
                    .status
                    .as_ref(),
                ObjectStatusCode::Ok,
            )?;
            let missing = gateway.get(&namespace, "missing", 4)?;
            assert_status(first_chunk_status(&missing), ObjectStatusCode::NotFound)?;
            assert_target_follow_up_ready(gateway)?;
            Ok(ScenarioOutcome::pass(
                "follow-up health succeeded after ordinary failure through TCP target",
            ))
        }
        ScenarioKind::StorageTimeout
        | ScenarioKind::StorageRetriable
        | ScenarioKind::StorageNonRetriable
        | ScenarioKind::TelemetrySuccessRecords
        | ScenarioKind::TelemetryExportFailureDiagnostic => {
            unreachable!("target support checked above")
        }
    }
}

fn config_for(kind: ScenarioKind) -> StackfulGatewayConfig {
    let request_timeout = match kind {
        ScenarioKind::Deadline => Some(Duration::from_millis(50)),
        _ => Some(Duration::from_secs(1)),
    };
    StackfulGatewayConfig {
        size_limits: ObjectSizeLimits::new(16, 4),
        namespace_mapping: NamespaceMapping::HermeticContainerPerNamespace {
            account: "devstore".to_owned(),
        },
        version: "object-gateway-conformance".to_owned(),
        request_timeout,
    }
}

fn tokio_config_for(kind: ScenarioKind) -> TokioGatewayConfig {
    let config = config_for(kind);
    TokioGatewayConfig {
        size_limits: config.size_limits,
        namespace_mapping: config.namespace_mapping,
        version: "object-gateway-tokio-conformance".to_owned(),
        request_timeout: config.request_timeout,
        ..TokioGatewayConfig::default()
    }
}

fn storage_failure_scenario(
    gateway: &impl GatewayOps,
    failure: StorageFailureClass,
    expected_status: ObjectStatusCode,
    expected_error: ObjectErrorClass,
) -> Result<ScenarioOutcome> {
    gateway.inject_storage_failure(failure);
    let chunks = gateway.get("cf-storage-failure", "object", 4)?;
    assert_status(first_chunk_status(&chunks), expected_status)?;
    assert_telemetry(gateway, ObjectOperation::Get, Some(expected_error))?;
    Ok(ScenarioOutcome::status(
        expected_status,
        format!("storage failure mapped to {expected_status:?}"),
    ))
}

fn assert_follow_up_ready(gateway: &impl GatewayOps) -> Result<()> {
    ensure!(
        gateway.grpc_health()?.ready,
        "follow-up gRPC health not ready"
    );
    ensure!(
        gateway.admin_health()?.contains("ready=true"),
        "follow-up admin health not ready"
    );
    Ok(())
}

fn assert_target_follow_up_ready(gateway: &TcpGateway) -> Result<()> {
    ensure!(
        gateway.grpc_health()?.ready,
        "target follow-up gRPC health not ready"
    );
    ensure!(
        gateway.admin_health()?.contains("ready=true"),
        "target follow-up admin health not ready"
    );
    Ok(())
}

fn assert_telemetry(
    gateway: &impl GatewayOps,
    operation: ObjectOperation,
    error: Option<ObjectErrorClass>,
) -> Result<()> {
    ensure!(
        gateway.telemetry().has_log_and_metric(operation, error),
        "missing telemetry for {operation:?} {error:?}"
    );
    Ok(())
}

fn assert_status(status: Option<&OperationStatus>, expected: ObjectStatusCode) -> Result<()> {
    let actual = status_code(status)?;
    if actual == expected {
        Ok(())
    } else {
        bail!("expected status {expected:?}, got {actual:?}")
    }
}

fn first_chunk_status(chunks: &[proto::GetChunk]) -> Option<&OperationStatus> {
    chunks.first().and_then(|chunk| chunk.status.as_ref())
}

fn status_code(status: Option<&OperationStatus>) -> Result<ObjectStatusCode> {
    let status = status.context("missing operation status")?;
    ObjectStatusCode::try_from(status.code).context("unknown object status code")
}

fn collect_body(chunks: &[proto::GetChunk]) -> Vec<u8> {
    chunks
        .iter()
        .flat_map(|chunk| chunk.data.iter().copied())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_gateway_conformance_expected_status_controls_pass_fail() {
        let mut scenario = DEFAULT_SCENARIOS
            .iter()
            .copied()
            .find(|scenario| scenario.label == "empty-object")
            .expect("empty-object scenario should exist");
        scenario.expected = ExpectedOutcome::Status(ObjectStatusCode::NotFound);

        let result = run_one(LocalRuntime::Stack, scenario);

        assert!(!result.passed);
        assert!(result.detail.contains("expected primary status"));
    }

    #[test]
    fn object_gateway_conformance_target_subset_excludes_fixture_only_scenarios() {
        let labels = target_supported_scenarios()
            .into_iter()
            .map(|scenario| scenario.label)
            .collect::<Vec<_>>();

        assert!(labels.contains(&"empty-object"));
        assert!(labels.contains(&"grpc-health"));
        assert!(!labels.contains(&"storage-timeout"));
        assert!(!labels.contains(&"telemetry-export-failure-diagnostic"));
    }
}
