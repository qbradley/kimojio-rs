// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::process::Command;

fn run(args: &[&str]) -> String {
    let binary = env!("CARGO_BIN_EXE_stack_router_workload");
    let output = Command::new(binary).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn stack_router_workload_output_shape_includes_required_fields() {
    let output = run(&["--runtime", "stack", "--workload", "routed"]);
    for field in [
        "stack-router-workload",
        "runtime=stack",
        "workload=Routed",
        "requests=",
        "success=",
        "failure=",
        "throughput_per_sec=",
        "p50_us=",
        "p95_us=",
        "p99_us=",
        "timeout_count=",
        "retry_count=",
        "load_shed_count=",
        "buffer_reject_count=",
        "cancellation_cleanup_count=",
    ] {
        assert!(output.contains(field), "missing field {field}: {output}");
    }
}

#[test]
fn stack_router_workload_runs_middleware_and_failure_profiles() {
    let heavy = run(&["--runtime", "stack-steal", "--workload", "middleware-heavy"]);
    assert!(heavy.contains("runtime=stack-steal"));
    assert!(heavy.contains("success=64"));

    for runtime in ["stack", "stack-steal"] {
        let failure = run(&["--runtime", runtime, "--workload", "failure"]);
        assert!(failure.contains(&format!("runtime={runtime}")));
        assert!(failure.contains("success=16"));
        assert!(failure.contains("failure=48"));
        assert!(failure.contains("timeout_count=16"));
        assert!(failure.contains("retry_count=16"));
        assert!(failure.contains("load_shed_count=16"));
        assert!(failure.contains("buffer_reject_count=16"));
        assert!(failure.contains("cancellation_cleanup_count=16"));
    }
}
