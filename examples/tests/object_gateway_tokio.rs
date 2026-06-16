// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::{path::PathBuf, process::Command};

use examples::object_gateway::conformance::{format_reports, run_tokio_implementation};

#[test]
fn object_gateway_tokio_conformance_passes_shared_matrix() {
    let report = run_tokio_implementation();

    assert!(report.passed(), "{report:#?}");
    let output = format_reports(&[report]);
    assert!(output.contains("implementation=tokio scenario=streaming-large-object status=pass"));
    assert!(output.contains("implementation=tokio scenario=telemetry-success-records status=pass"));
}

#[test]
fn object_gateway_tokio_host_reports_multithread_readiness_and_shutdown() {
    let output = Command::new(tokio_host_binary())
        .arg("--grpc-addr")
        .arg("127.0.0.1:0")
        .arg("--admin-addr")
        .arg("127.0.0.1:0")
        .arg("--workers")
        .arg("2")
        .arg("--shutdown-after-ready")
        .output()
        .expect("Tokio host should launch");

    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("object-gateway-tokio-host runtime=tokio"));
    assert!(stdout.contains("runtime_mode=multi-thread"));
    assert!(stdout.contains("ready=true"));
}

fn tokio_host_binary() -> PathBuf {
    option_env!("CARGO_BIN_EXE_object-gateway-tokio-host")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../target/debug/object-gateway-tokio-host")
        })
}
