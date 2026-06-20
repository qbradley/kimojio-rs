// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Normalized Stackful Object Gateway workload profiles and result reporting.
//!
//! The workload harness keeps inputs fixed and output fields stable across the
//! stack, stack-steal, Tokio, and optional Go implementations. It is intended for
//! diagnostic comparison rather than absolute pass/fail performance thresholds.
//! Latency percentiles are computed by the harness from operation timings; they
//! are not OpenTelemetry histograms.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use bytes::Bytes;

use std::num::NonZeroUsize;

use kimojio_stack_steal::SchedulerConfig;

use super::{
    launch::{LocalGateway, LocalRuntime, TcpGateway},
    model::{NamespaceMapping, ObjectSizeLimits},
    proto::{self, ObjectStatusCode, OperationStatus},
    stackful::StackfulGatewayConfig,
    tokio_impl::{TokioGateway, TokioGatewayConfig},
};

const MEASUREMENT_MODE: &str = "cold-start-per-operation";
const TAIL_LATENCY_RELIABLE_SAMPLES: usize = 100;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkloadKind {
    SmallObject,
    StreamingLargeObject,
    Concurrent,
    TenantSkew,
    CancellationHeavy,
    ErrorHeavy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkloadProfile {
    pub label: &'static str,
    pub kind: WorkloadKind,
    pub iterations: usize,
    pub max_object_bytes: u64,
    pub max_chunk_bytes: usize,
    pub memory_budget_bytes: usize,
}

pub const DEFAULT_WORKLOADS: &[WorkloadProfile] = &[
    WorkloadProfile {
        label: "small-object",
        kind: WorkloadKind::SmallObject,
        iterations: 8,
        max_object_bytes: 1024,
        max_chunk_bytes: 256,
        memory_budget_bytes: 1024,
    },
    WorkloadProfile {
        label: "streaming-large-object",
        kind: WorkloadKind::StreamingLargeObject,
        iterations: 4,
        max_object_bytes: 4096,
        max_chunk_bytes: 1024,
        memory_budget_bytes: 4096,
    },
    WorkloadProfile {
        label: "concurrent",
        kind: WorkloadKind::Concurrent,
        iterations: 8,
        max_object_bytes: 1024,
        max_chunk_bytes: 256,
        memory_budget_bytes: 2048,
    },
    WorkloadProfile {
        label: "tenant-skew",
        kind: WorkloadKind::TenantSkew,
        iterations: 1,
        max_object_bytes: 8192,
        max_chunk_bytes: 1024,
        memory_budget_bytes: 8192,
    },
    WorkloadProfile {
        label: "cancellation-heavy",
        kind: WorkloadKind::CancellationHeavy,
        iterations: 4,
        max_object_bytes: 1024,
        max_chunk_bytes: 256,
        memory_budget_bytes: 1024,
    },
    WorkloadProfile {
        label: "error-heavy",
        kind: WorkloadKind::ErrorHeavy,
        iterations: 4,
        max_object_bytes: 1024,
        max_chunk_bytes: 256,
        memory_budget_bytes: 1024,
    },
];

pub fn find_workload(label: &str) -> Option<WorkloadProfile> {
    DEFAULT_WORKLOADS
        .iter()
        .copied()
        .find(|profile| profile.label == label)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkloadImplementation {
    Stack,
    StackSteal,
    StackStealSfq,
    Tokio,
}

impl WorkloadImplementation {
    pub const ALL: [Self; 4] = [
        Self::Stack,
        Self::StackSteal,
        Self::StackStealSfq,
        Self::Tokio,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Stack => "stack",
            Self::StackSteal => "stack-steal",
            Self::StackStealSfq => "stack-steal-sfq",
            Self::Tokio => "tokio",
        }
    }

    const fn runtime_mode(self) -> &'static str {
        match self {
            Self::Stack => "pinned",
            Self::StackSteal => "work-stealing",
            Self::StackStealSfq => "work-stealing-sfq",
            Self::Tokio => "multi-thread",
        }
    }

    const fn storage_client(self) -> &'static str {
        match self {
            Self::Stack | Self::StackSteal | Self::StackStealSfq => "kimojio-stack-storage",
            Self::Tokio => "hermetic-storage-fixture",
        }
    }

    const fn backend_mode(self) -> &'static str {
        "hermetic"
    }

    const fn telemetry_sink_mode(self) -> &'static str {
        match self {
            Self::Stack | Self::StackSteal | Self::StackStealSfq => {
                "kimojio-stack-opentelemetry-hermetic"
            }
            Self::Tokio => "opentelemetry_sdk-hermetic",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkloadTargetInfo<'a> {
    pub implementation: &'a str,
    pub runtime_mode: &'a str,
    pub storage_client: &'a str,
    pub backend_mode: &'a str,
    pub telemetry_sink_mode: &'a str,
}

impl WorkloadTargetInfo<'_> {
    fn owned(self) -> WorkloadTargetMetadata {
        WorkloadTargetMetadata {
            implementation: self.implementation.to_owned(),
            runtime_mode: self.runtime_mode.to_owned(),
            storage_client: self.storage_client.to_owned(),
            backend_mode: self.backend_mode.to_owned(),
            telemetry_sink_mode: self.telemetry_sink_mode.to_owned(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WorkloadTargetMetadata {
    implementation: String,
    runtime_mode: String,
    storage_client: String,
    backend_mode: String,
    telemetry_sink_mode: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkloadSummary {
    pub implementation: String,
    pub runtime_mode: String,
    pub storage_client: String,
    pub backend_mode: String,
    pub telemetry_sink_mode: String,
    pub measurement_mode: String,
    pub workload: String,
    pub request_count: usize,
    pub success_count: usize,
    pub error_count: usize,
    pub canceled_count: usize,
    pub throughput_per_sec: f64,
    pub tail_latency_samples: usize,
    pub tail_latency_confidence: String,
    pub p50_us: u128,
    pub p95_us: u128,
    pub p99_us: u128,
    pub fairness_ratio: Option<f64>,
    pub max_chunk_bytes: usize,
    pub memory_budget_bytes: usize,
}

impl WorkloadSummary {
    pub fn key_value_line(&self) -> String {
        format!(
            "object-gateway-workload implementation={} runtime_mode={} storage_client={} backend_mode={} telemetry_sink_mode={} measurement_mode={} workload={} request_count={} success_count={} error_count={} canceled_count={} throughput_per_sec={:.3} tail_latency_samples={} tail_latency_confidence={} p50_us={} p95_us={} p99_us={} fairness_ratio={} max_chunk_bytes={} memory_budget_bytes={}",
            self.implementation,
            self.runtime_mode,
            self.storage_client,
            self.backend_mode,
            self.telemetry_sink_mode,
            self.measurement_mode,
            self.workload,
            self.request_count,
            self.success_count,
            self.error_count,
            self.canceled_count,
            self.throughput_per_sec,
            self.tail_latency_samples,
            self.tail_latency_confidence,
            self.p50_us,
            self.p95_us,
            self.p99_us,
            self.fairness_ratio
                .map(|ratio| format!("{ratio:.3}"))
                .unwrap_or_else(|| "n/a".to_owned()),
            self.max_chunk_bytes,
            self.memory_budget_bytes
        )
    }
}

#[derive(Clone, Debug)]
enum GatewayTarget {
    Local(LocalGateway),
    Tokio(TokioGateway),
    Tcp(TcpGateway),
}

pub fn run_implementation_profile(
    implementation: WorkloadImplementation,
    profile: WorkloadProfile,
) -> Result<WorkloadSummary> {
    let target = match implementation {
        WorkloadImplementation::Stack => GatewayTarget::Local(LocalGateway::new(
            LocalRuntime::Stack,
            stackful_config_for(profile, "object-gateway-workload-stack"),
        )),
        WorkloadImplementation::StackSteal => GatewayTarget::Local(LocalGateway::new(
            LocalRuntime::StackSteal,
            stackful_config_for(profile, "object-gateway-workload-stack-steal"),
        )),
        WorkloadImplementation::StackStealSfq => {
            GatewayTarget::Local(LocalGateway::new_with_steal_scheduler(
                LocalRuntime::StackStealSfq,
                stackful_config_for(profile, "object-gateway-workload-stack-steal-sfq"),
                SchedulerConfig::stochastic_fair(NonZeroUsize::new(8).unwrap()),
            ))
        }
        WorkloadImplementation::Tokio => GatewayTarget::Tokio(TokioGateway::new(tokio_config_for(
            profile,
            "object-gateway-workload-tokio",
        ))),
    };
    let info = WorkloadTargetInfo {
        implementation: implementation.label(),
        runtime_mode: implementation.runtime_mode(),
        storage_client: implementation.storage_client(),
        backend_mode: implementation.backend_mode(),
        telemetry_sink_mode: implementation.telemetry_sink_mode(),
    };
    run_profile_for_target(info, target, profile)
}

pub fn run_tcp_profile(
    info: WorkloadTargetInfo<'_>,
    gateway: TcpGateway,
    profile: WorkloadProfile,
) -> Result<WorkloadSummary> {
    run_profile_for_target(info, GatewayTarget::Tcp(gateway), profile)
}

fn run_profile_for_target(
    info: WorkloadTargetInfo<'_>,
    target: GatewayTarget,
    profile: WorkloadProfile,
) -> Result<WorkloadSummary> {
    ensure!(
        profile.max_chunk_bytes > 0,
        "max chunk size must be positive"
    );
    ensure!(
        profile.memory_budget_bytes >= profile.max_chunk_bytes,
        "memory budget must cover at least one chunk"
    );
    let started = Instant::now();
    let mut recorder = WorkloadRecorder::default();
    let namespace = format!("wl-{}-{}", info.implementation, profile.label);
    let fairness_ratio = match profile.kind {
        WorkloadKind::SmallObject => {
            run_small_object(&target, &namespace, profile, &mut recorder)?;
            None
        }
        WorkloadKind::StreamingLargeObject => {
            run_streaming_large_object(&target, &namespace, profile, &mut recorder)?;
            None
        }
        WorkloadKind::Concurrent => {
            run_concurrent(&target, &namespace, profile, &mut recorder)?;
            None
        }
        WorkloadKind::TenantSkew => Some(run_tenant_skew(
            &target,
            &namespace,
            profile,
            &mut recorder,
        )?),
        WorkloadKind::CancellationHeavy => {
            run_cancellation_heavy(&target, &namespace, profile, &mut recorder)?;
            None
        }
        WorkloadKind::ErrorHeavy => {
            run_error_heavy(&target, &namespace, profile, &mut recorder)?;
            None
        }
    };
    Ok(recorder.finish(info.owned(), profile, started.elapsed(), fairness_ratio))
}

fn run_small_object(
    target: &GatewayTarget,
    namespace: &str,
    profile: WorkloadProfile,
    recorder: &mut WorkloadRecorder,
) -> Result<()> {
    for index in 0..profile.iterations {
        let object = format!("small-{index}");
        let body = Bytes::from(format!("hello-object-{index}"));
        let put = timed(|| target.put(namespace, &object, vec![body.clone()]))?;
        recorder.record_status(put.value, put.elapsed);
        let get = timed(|| target.get(namespace, &object, profile.max_chunk_bytes as u32))?;
        recorder.record_status(get_status(&get.value), get.elapsed);
        ensure!(
            collect_body(&get.value) == body,
            "small-object workload body mismatch for {object}"
        );
    }
    Ok(())
}

fn run_streaming_large_object(
    target: &GatewayTarget,
    namespace: &str,
    profile: WorkloadProfile,
    recorder: &mut WorkloadRecorder,
) -> Result<()> {
    for index in 0..profile.iterations {
        let object = format!("large-{index}");
        let chunks = fixed_chunks(profile.max_object_bytes as usize, profile.max_chunk_bytes);
        let expected = chunk_digest(&chunks);
        ensure!(
            expected.bytes <= profile.memory_budget_bytes,
            "streaming workload body exceeds configured memory budget"
        );
        let put = timed(|| target.put(namespace, &object, chunks))?;
        recorder.record_status(put.value, put.elapsed);
        let get = timed(|| target.get(namespace, &object, profile.max_chunk_bytes as u32))?;
        ensure!(
            get.value
                .iter()
                .all(|chunk| chunk.data.len() <= profile.max_chunk_bytes),
            "GET emitted a chunk larger than the configured limit"
        );
        let actual = get_chunk_digest(&get.value);
        ensure!(
            actual.bytes <= profile.memory_budget_bytes,
            "GET response exceeded configured memory budget"
        );
        recorder.record_status(get_status(&get.value), get.elapsed);
        ensure!(
            actual == expected,
            "streaming workload body mismatch for {object}"
        );
    }
    Ok(())
}

fn run_concurrent(
    target: &GatewayTarget,
    namespace: &str,
    profile: WorkloadProfile,
    recorder: &mut WorkloadRecorder,
) -> Result<()> {
    for index in 0..profile.iterations {
        let first = format!("concurrent-{index}-a");
        let second = format!("concurrent-{index}-b");
        let elapsed = Instant::now();
        let (first_response, second_response) = target.put_pair_concurrently(
            namespace,
            (&first, Bytes::from_static(b"a")),
            (&second, Bytes::from_static(b"b")),
        )?;
        let elapsed = elapsed.elapsed();
        recorder.record_status(status_code(first_response.status.as_ref()), elapsed);
        recorder.record_status(status_code(second_response.status.as_ref()), elapsed);
    }

    Ok(())
}

fn run_tenant_skew(
    target: &GatewayTarget,
    namespace: &str,
    profile: WorkloadProfile,
    recorder: &mut WorkloadRecorder,
) -> Result<f64> {
    let mut completion_rank_total = 0_usize;
    let mut light_completion_total = 0_usize;
    for index in 0..profile.iterations {
        let mut puts = Vec::with_capacity(136);
        for heavy_index in 0..128 {
            let object = format!("heavy-{index}-{heavy_index}");
            puts.push(TenantSkewPut {
                namespace: namespace.to_owned(),
                object,
                chunks: fixed_chunks(profile.max_object_bytes as usize, profile.max_chunk_bytes),
                tenant_key: 0,
                light: false,
            });
        }
        for light_index in 0..8 {
            let tenant_namespace = format!("{namespace}-light-{light_index}");
            let object = format!("light-{index}-{light_index}");
            puts.push(TenantSkewPut {
                namespace: tenant_namespace,
                object,
                chunks: fixed_chunks(profile.max_object_bytes as usize, profile.max_chunk_bytes),
                tenant_key: light_index + 1,
                light: true,
            });
        }
        let light_flags = puts.iter().map(|put| put.light).collect::<Vec<_>>();
        let results = target.put_many_concurrently(puts)?;
        let mut batch_results = light_flags.into_iter().zip(results).collect::<Vec<_>>();
        batch_results.sort_by_key(|(_, put)| put.elapsed);
        let mut lights_seen = 0_usize;
        for (rank, (is_light, _)) in batch_results.iter().enumerate() {
            if *is_light {
                lights_seen += 1;
                if lights_seen == 8 {
                    completion_rank_total += rank + 1;
                    light_completion_total += lights_seen;
                    break;
                }
            }
        }
        for (_, put) in batch_results {
            recorder.record_status(put.value, put.elapsed);
        }
    }
    if light_completion_total == 0 {
        return Ok(0.0);
    }
    Ok(completion_rank_total as f64 / light_completion_total as f64)
}

struct TenantSkewPut {
    namespace: String,
    object: String,
    chunks: Vec<Bytes>,
    tenant_key: usize,
    light: bool,
}

fn run_cancellation_heavy(
    target: &GatewayTarget,
    namespace: &str,
    profile: WorkloadProfile,
    recorder: &mut WorkloadRecorder,
) -> Result<()> {
    for index in 0..profile.iterations {
        let object = format!("cancel-{index}");
        let chunks = fixed_chunks(profile.max_object_bytes as usize, profile.max_chunk_bytes);
        let put = timed(|| target.put(namespace, &object, chunks))?;
        recorder.record_status(put.value, put.elapsed);
        let cancel = timed(|| target.cancel_get_after_first_chunk(namespace, &object))?;
        recorder.record_canceled(cancel.elapsed);
    }
    Ok(())
}

fn run_error_heavy(
    target: &GatewayTarget,
    namespace: &str,
    profile: WorkloadProfile,
    recorder: &mut WorkloadRecorder,
) -> Result<()> {
    for index in 0..profile.iterations {
        let missing = timed(|| {
            target.get(
                namespace,
                &format!("missing-{index}"),
                profile.max_chunk_bytes as u32,
            )
        })?;
        recorder.record_status(get_status(&missing.value), missing.elapsed);

        let oversize = fixed_chunks(
            profile.max_object_bytes as usize + 1,
            profile.max_chunk_bytes,
        );
        let put = timed(|| target.put(namespace, &format!("too-large-{index}"), oversize))?;
        recorder.record_status(put.value, put.elapsed);

        let copy = timed(|| {
            target.copy(
                namespace,
                &format!("copy-missing-{index}"),
                &format!("copy-dst-{index}"),
            )
        })?;
        recorder.record_status(copy.value, copy.elapsed);
    }
    Ok(())
}

impl GatewayTarget {
    fn put(&self, namespace: &str, object: &str, chunks: Vec<Bytes>) -> Result<ObjectStatusCode> {
        let response = match self {
            Self::Local(gateway) => gateway.put(namespace, object, chunks)?,
            Self::Tokio(gateway) => gateway.put(namespace, object, chunks)?,
            Self::Tcp(gateway) => gateway.put(namespace, object, chunks)?,
        };
        Ok(status_code(response.status.as_ref()))
    }

    fn get(
        &self,
        namespace: &str,
        object: &str,
        max_chunk_bytes: u32,
    ) -> Result<Vec<proto::GetChunk>> {
        match self {
            Self::Local(gateway) => gateway.get(namespace, object, max_chunk_bytes),
            Self::Tokio(gateway) => gateway.get(namespace, object, max_chunk_bytes),
            Self::Tcp(gateway) => gateway.get(namespace, object, max_chunk_bytes),
        }
    }

    fn copy(&self, namespace: &str, source: &str, destination: &str) -> Result<ObjectStatusCode> {
        let response = match self {
            Self::Local(gateway) => gateway.copy(namespace, source, destination)?,
            Self::Tokio(gateway) => gateway.copy(namespace, source, destination)?,
            Self::Tcp(gateway) => gateway.copy(namespace, source, destination)?,
        };
        Ok(status_code(response.status.as_ref()))
    }

    fn cancel_get_after_first_chunk(&self, namespace: &str, object: &str) -> Result<()> {
        match self {
            Self::Local(gateway) => gateway.cancel_get_after_first_chunk(namespace, object),
            Self::Tokio(gateway) => gateway.cancel_get_after_first_chunk(namespace, object),
            Self::Tcp(gateway) => gateway.cancel_get_after_first_chunk(namespace, object),
        }
    }

    fn put_pair_concurrently(
        &self,
        namespace: &str,
        first: (&str, Bytes),
        second: (&str, Bytes),
    ) -> Result<(proto::PutResponse, proto::PutResponse)> {
        match self {
            Self::Local(gateway) => gateway.put_pair_concurrently(namespace, first, second),
            Self::Tokio(gateway) => gateway.put_pair_concurrently(namespace, first, second),
            Self::Tcp(gateway) => gateway.put_pair_concurrently(namespace, first, second),
        }
    }

    fn put_many_concurrently(
        &self,
        puts: Vec<TenantSkewPut>,
    ) -> Result<Vec<Timed<ObjectStatusCode>>> {
        match self {
            Self::Local(gateway) => gateway
                .put_many_concurrently(
                    puts.into_iter()
                        .map(|put| (put.namespace, put.object, put.chunks, put.tenant_key))
                        .collect(),
                )?
                .into_iter()
                .map(|(response, elapsed)| {
                    Ok(Timed {
                        value: status_code(response.status.as_ref()),
                        elapsed,
                    })
                })
                .collect(),
            Self::Tokio(_) | Self::Tcp(_) => self.put_many_with_threads(puts),
        }
    }

    fn put_many_with_threads(
        &self,
        puts: Vec<TenantSkewPut>,
    ) -> Result<Vec<Timed<ObjectStatusCode>>> {
        std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(puts.len());
            for (index, put) in puts.into_iter().enumerate() {
                let target = self.clone();
                handles.push(scope.spawn(move || {
                    let chunks = put.chunks;
                    let put = timed(|| target.put(&put.namespace, &put.object, chunks))?;
                    Ok::<_, anyhow::Error>((index, put))
                }));
            }
            let mut results = Vec::with_capacity(handles.len());
            results.resize_with(handles.len(), || None);
            for handle in handles {
                let (index, put) = handle
                    .join()
                    .map_err(|_| anyhow::anyhow!("concurrent tenant-skew PUT panicked"))??;
                results[index] = Some(put);
            }
            results
                .into_iter()
                .map(|put| put.ok_or_else(|| anyhow::anyhow!("missing tenant-skew PUT result")))
                .collect()
        })
    }
}

#[derive(Default)]
struct WorkloadRecorder {
    request_count: usize,
    success_count: usize,
    error_count: usize,
    canceled_count: usize,
    latencies: Vec<Duration>,
}

impl WorkloadRecorder {
    fn record_status(&mut self, status: ObjectStatusCode, latency: Duration) {
        self.request_count += 1;
        self.latencies.push(latency);
        if status == ObjectStatusCode::Ok {
            self.success_count += 1;
        } else {
            self.error_count += 1;
        }
    }

    fn record_canceled(&mut self, latency: Duration) {
        self.request_count += 1;
        self.canceled_count += 1;
        self.latencies.push(latency);
    }

    fn finish(
        mut self,
        info: WorkloadTargetMetadata,
        profile: WorkloadProfile,
        elapsed: Duration,
        fairness_ratio: Option<f64>,
    ) -> WorkloadSummary {
        self.latencies.sort_unstable();
        let throughput_per_sec = if elapsed.is_zero() {
            0.0
        } else {
            self.request_count as f64 / elapsed.as_secs_f64()
        };
        WorkloadSummary {
            implementation: info.implementation,
            runtime_mode: info.runtime_mode,
            storage_client: info.storage_client,
            backend_mode: info.backend_mode,
            telemetry_sink_mode: info.telemetry_sink_mode,
            measurement_mode: MEASUREMENT_MODE.to_owned(),
            workload: profile.label.to_owned(),
            request_count: self.request_count,
            success_count: self.success_count,
            error_count: self.error_count,
            canceled_count: self.canceled_count,
            throughput_per_sec,
            tail_latency_samples: self.latencies.len(),
            tail_latency_confidence: tail_latency_confidence(self.latencies.len()).to_owned(),
            p50_us: percentile_us(&self.latencies, 50),
            p95_us: percentile_us(&self.latencies, 95),
            p99_us: percentile_us(&self.latencies, 99),
            fairness_ratio,
            max_chunk_bytes: profile.max_chunk_bytes,
            memory_budget_bytes: profile.memory_budget_bytes,
        }
    }
}

struct Timed<T> {
    value: T,
    elapsed: Duration,
}

fn timed<T>(f: impl FnOnce() -> Result<T>) -> Result<Timed<T>> {
    let started = Instant::now();
    let value = f()?;
    Ok(Timed {
        value,
        elapsed: started.elapsed(),
    })
}

fn status_code(status: Option<&OperationStatus>) -> ObjectStatusCode {
    status
        .and_then(|status| ObjectStatusCode::try_from(status.code).ok())
        .unwrap_or(ObjectStatusCode::Internal)
}

fn get_status(chunks: &[proto::GetChunk]) -> ObjectStatusCode {
    chunks
        .first()
        .and_then(|chunk| chunk.status.as_ref())
        .map_or(ObjectStatusCode::Internal, |status| {
            status_code(Some(status))
        })
}

fn collect_body(chunks: &[proto::GetChunk]) -> Vec<u8> {
    chunks
        .iter()
        .flat_map(|chunk| chunk.data.iter().copied())
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BodyDigest {
    bytes: usize,
    checksum: u64,
}

fn chunk_digest(chunks: &[Bytes]) -> BodyDigest {
    chunks.iter().fold(
        BodyDigest {
            bytes: 0,
            checksum: 0,
        },
        |digest, chunk| digest.extend(chunk),
    )
}

fn get_chunk_digest(chunks: &[proto::GetChunk]) -> BodyDigest {
    chunks.iter().fold(
        BodyDigest {
            bytes: 0,
            checksum: 0,
        },
        |digest, chunk| digest.extend(&chunk.data),
    )
}

impl BodyDigest {
    fn extend(self, bytes: &[u8]) -> Self {
        let checksum = bytes.iter().fold(self.checksum, |checksum, byte| {
            checksum.wrapping_mul(16_777_619) ^ u64::from(*byte)
        });
        Self {
            bytes: self.bytes + bytes.len(),
            checksum,
        }
    }
}

fn fixed_chunks(total_bytes: usize, max_chunk_bytes: usize) -> Vec<Bytes> {
    let mut remaining = total_bytes;
    let mut seed = 0u8;
    let mut chunks = Vec::new();
    while remaining > 0 {
        let len = remaining.min(max_chunk_bytes);
        let chunk = (0..len)
            .map(|_| {
                seed = seed.wrapping_add(1);
                seed
            })
            .collect::<Vec<_>>();
        chunks.push(Bytes::from(chunk));
        remaining -= len;
    }
    chunks
}

const fn tail_latency_confidence(samples: usize) -> &'static str {
    if samples >= TAIL_LATENCY_RELIABLE_SAMPLES {
        "standard"
    } else {
        "low-sample-diagnostic"
    }
}

fn percentile_us(latencies: &[Duration], percentile: usize) -> u128 {
    if latencies.is_empty() {
        return 0;
    }
    let index = ((latencies.len() - 1) * percentile) / 100;
    latencies[index].as_micros()
}

fn stackful_config_for(profile: WorkloadProfile, version: &str) -> StackfulGatewayConfig {
    StackfulGatewayConfig {
        size_limits: ObjectSizeLimits::new(profile.max_object_bytes, profile.max_chunk_bytes),
        namespace_mapping: NamespaceMapping::HermeticContainerPerNamespace {
            account: "devstore".to_owned(),
        },
        version: version.to_owned(),
        request_timeout: Some(Duration::from_secs(1)),
    }
}

fn tokio_config_for(profile: WorkloadProfile, version: &str) -> TokioGatewayConfig {
    let config = stackful_config_for(profile, version);
    TokioGatewayConfig {
        size_limits: config.size_limits,
        namespace_mapping: config.namespace_mapping,
        version: config.version,
        request_timeout: config.request_timeout,
        ..TokioGatewayConfig::default()
    }
}

pub fn go_workload_host_config(profile: WorkloadProfile) -> super::launch::GoHostConfig {
    super::launch::GoHostConfig {
        max_object_bytes: profile.max_object_bytes,
        max_chunk_bytes: profile.max_chunk_bytes,
        ..super::launch::GoHostConfig::default()
    }
}

pub fn go_workload_target_info() -> WorkloadTargetInfo<'static> {
    WorkloadTargetInfo {
        implementation: "go",
        runtime_mode: "goroutine",
        storage_client: "hermetic-storage-fixture",
        backend_mode: "hermetic",
        telemetry_sink_mode: "hermetic-otlp-fixture",
    }
}

pub fn format_summaries(summaries: &[WorkloadSummary]) -> String {
    summaries
        .iter()
        .map(WorkloadSummary::key_value_line)
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn parse_workload(label: &str) -> Result<WorkloadProfile> {
    find_workload(label).with_context(|| format!("unknown workload profile: {label}"))
}
