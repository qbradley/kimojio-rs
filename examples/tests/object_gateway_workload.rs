// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::process::Command;

use examples::object_gateway::{
    live::{
        LIVE_STORAGE_CONTAINER_ENV, LIVE_STORAGE_ENDPOINT_ENV, LIVE_STORAGE_ENV,
        LIVE_TELEMETRY_ENDPOINT_ENV, LIVE_TELEMETRY_ENV, live_storage_skip_reason_from,
        live_telemetry_skip_reason_from,
    },
    workload::{
        DEFAULT_WORKLOADS, WorkloadImplementation, format_summaries, parse_workload,
        run_implementation_profile,
    },
};

#[test]
fn object_gateway_workload_output_shape_includes_required_fields() {
    let summary = run_implementation_profile(
        WorkloadImplementation::Stack,
        parse_workload("small-object").unwrap(),
    )
    .unwrap();
    let line = summary.key_value_line();
    for field in [
        "implementation=",
        "runtime_mode=",
        "storage_client=",
        "backend_mode=",
        "telemetry_sink_mode=",
        "measurement_mode=cold-start-per-operation",
        "workload=",
        "request_count=",
        "success_count=",
        "error_count=",
        "canceled_count=",
        "throughput_per_sec=",
        "tail_latency_samples=",
        "tail_latency_confidence=",
        "p50_us=",
        "p95_us=",
        "p99_us=",
    ] {
        assert!(line.contains(field), "missing field {field} in {line}");
    }
}

#[test]
fn object_gateway_workload_streaming_reports_limits() {
    let profile = parse_workload("streaming-large-object").unwrap();
    let summary = run_implementation_profile(WorkloadImplementation::Stack, profile).unwrap();
    assert_eq!(summary.workload, "streaming-large-object");
    assert_eq!(summary.max_chunk_bytes, profile.max_chunk_bytes);
    assert_eq!(summary.memory_budget_bytes, profile.memory_budget_bytes);
    assert_eq!(summary.request_count, profile.iterations * 2);
    assert_eq!(summary.success_count, profile.iterations * 2);
    assert_eq!(summary.error_count, 0);
    assert_eq!(summary.canceled_count, 0);
}

#[test]
fn object_gateway_workload_all_rust_profiles_keep_report_shape() {
    let mut summaries = Vec::new();
    for implementation in WorkloadImplementation::ALL {
        for profile in DEFAULT_WORKLOADS {
            let summary = run_implementation_profile(implementation, *profile).unwrap();
            assert_eq!(summary.implementation, implementation.label());
            assert_eq!(summary.workload, profile.label);
            assert!(summary.request_count > 0);
            summaries.push(summary);
        }
    }
    let output = format_summaries(&summaries);
    for implementation in ["stack", "stack-steal", "tokio"] {
        assert!(
            output.contains(&format!("implementation={implementation}")),
            "missing workload output for {implementation}: {output}"
        );
    }
    for profile in DEFAULT_WORKLOADS {
        assert!(
            output.contains(&format!("workload={}", profile.label)),
            "missing workload output for {}: {output}",
            profile.label
        );
    }
}

#[test]
fn object_gateway_workload_binary_emits_key_value_output() {
    let output = Command::new(env!("CARGO_BIN_EXE_object-gateway-workload"))
        .args(["--implementation", "stack", "--workload", "small-object"])
        .output()
        .expect("failed to run object-gateway-workload");
    assert!(
        output.status.success(),
        "workload binary failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("object-gateway-workload"));
    assert!(stdout.contains("implementation=stack"));
    assert!(stdout.contains("workload=small-object"));
}

#[test]
fn object_gateway_workload_live_storage_gate_skips_without_full_configuration() {
    let reason = live_storage_skip_reason_from(|_| None).unwrap();
    assert!(
        reason.contains("storage") || reason.contains("KIMOJIO_OBJECT_GATEWAY"),
        "unexpected live storage skip reason: {reason}"
    );
    let reason = live_storage_skip_reason_from(|name| {
        if name == LIVE_STORAGE_ENV {
            Some("1".to_owned())
        } else {
            None
        }
    })
    .unwrap();
    assert!(reason.contains(LIVE_STORAGE_ENDPOINT_ENV));
    assert!(reason.contains(LIVE_STORAGE_CONTAINER_ENV));
}

#[test]
fn object_gateway_workload_live_telemetry_gate_skips_without_full_configuration() {
    let reason = live_telemetry_skip_reason_from(|_| None).unwrap();
    assert!(
        reason.contains("telemetry") || reason.contains("OpenTelemetry"),
        "unexpected live telemetry skip reason: {reason}"
    );
    let reason = live_telemetry_skip_reason_from(|name| {
        if name == LIVE_TELEMETRY_ENV {
            Some("1".to_owned())
        } else {
            None
        }
    })
    .unwrap();
    assert!(reason.contains(LIVE_TELEMETRY_ENDPOINT_ENV));
}
