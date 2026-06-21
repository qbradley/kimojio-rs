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

fn value<'a>(line: &'a str, key: &str) -> &'a str {
    line.split_whitespace()
        .find_map(|part| part.strip_prefix(&format!("{key}=")))
        .unwrap_or_else(|| panic!("missing {key}: {line}"))
}

fn usize_value(line: &str, key: &str) -> usize {
    value(line, key).parse().unwrap()
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
    for runtime in ["stack", "stack-steal"] {
        for workload in ["routed", "middleware-heavy"] {
            let output = run(&["--runtime", runtime, "--workload", workload]);
            assert!(output.contains(&format!("runtime={runtime}")));
            assert_eq!(usize_value(&output, "requests"), 64);
            assert_eq!(usize_value(&output, "success"), 64);
            assert_eq!(usize_value(&output, "failure"), 0);
            assert!(usize_value(&output, "p50_us") <= usize_value(&output, "p95_us"));
            assert!(usize_value(&output, "p95_us") <= usize_value(&output, "p99_us"));
        }

        let failure = run(&["--runtime", runtime, "--workload", "failure"]);
        assert!(failure.contains(&format!("runtime={runtime}")));
        assert_eq!(usize_value(&failure, "requests"), 64);
        assert_eq!(usize_value(&failure, "success"), 16);
        assert_eq!(usize_value(&failure, "failure"), 48);
        assert_eq!(
            usize_value(&failure, "success") + usize_value(&failure, "failure"),
            usize_value(&failure, "requests")
        );
        assert_eq!(usize_value(&failure, "timeout_count"), 16);
        assert_eq!(usize_value(&failure, "retry_count"), 16);
        assert_eq!(usize_value(&failure, "load_shed_count"), 16);
        assert_eq!(usize_value(&failure, "buffer_reject_count"), 16);
        assert_eq!(usize_value(&failure, "cancellation_cleanup_count"), 16);
        assert!(usize_value(&failure, "p50_us") <= usize_value(&failure, "p95_us"));
        assert!(usize_value(&failure, "p95_us") <= usize_value(&failure, "p99_us"));
    }
}
