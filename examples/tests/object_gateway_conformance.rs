// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use examples::object_gateway::conformance::{
    DEFAULT_SCENARIOS, format_reports, run_default_implementations,
};
use examples::object_gateway::launch::{HostProcessConfig, HostRuntime, run_host_ready_check};
use std::path::PathBuf;

#[test]
fn object_gateway_conformance_stack_and_steal_pass_shared_matrix() {
    let reports = run_default_implementations();
    assert_eq!(reports.len(), 3);
    for report in &reports {
        assert!(report.passed(), "{report:#?}");
        let labels = report
            .results
            .iter()
            .map(|result| result.scenario.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            DEFAULT_SCENARIOS
                .iter()
                .map(|scenario| scenario.label)
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn object_gateway_conformance_output_is_implementation_and_scenario_labeled() {
    let reports = run_default_implementations();
    let output = format_reports(&reports);
    assert!(output.contains("implementation=stack scenario=empty-object status=pass"));
    assert!(output.contains("implementation=stack-steal scenario=deadline status=pass"));
    assert!(output.contains("implementation=tokio scenario=grpc-health status=pass"));
    assert!(output.contains("scenario=telemetry-export-failure-diagnostic status=pass"));
}

#[test]
fn object_gateway_conformance_process_launch_ready_check_reports_addresses() {
    let ready = run_host_ready_check(&HostProcessConfig::new(host_binary(), HostRuntime::Stack))
        .expect("host readiness launch should pass");

    assert_eq!(ready.runtime, HostRuntime::Stack);
    assert!(ready.stdout.contains("ready=true"));
    assert_eq!(ready.grpc_addr.ip(), ready.admin_addr.ip());
}

fn host_binary() -> PathBuf {
    option_env!("CARGO_BIN_EXE_object-gateway-stack-host")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../target/debug/object-gateway-stack-host")
        })
}
