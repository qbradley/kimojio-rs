"""Normalize the frozen native benchmark CLI and capture per-process wait4 resources."""

import argparse
import json
import os
from pathlib import Path
import sys
import time
import uuid

from run import ROOT, ARTIFACTS, digest, snapshot
from harness import PeerProcess


def execute(command, timeout):
    samples = []
    terminal_sample_errors = []
    started = time.monotonic()
    with PeerProcess(command, cwd=ROOT, wait_for_readiness=False) as child:
        pid = child.process.pid
        sample_at = started
        while True:
            reaped, status, usage = os.wait4(pid, os.WNOHANG)
            if reaped:
                child.process.returncode = os.waitstatus_to_exitcode(status)
                break
            now = time.monotonic()
            if now - started > timeout:
                raise TimeoutError("native client exceeded outer deadline")
            if now >= sample_at:
                try:
                    samples.append(snapshot(pid))
                except OSError as error:
                    terminal_sample_errors.append(repr(error))
                sample_at = now + .05
            time.sleep(.002)
        for thread in child._threads:
            thread.join(timeout=1)
        result = {
            "exit": child.process.returncode, "command": command,
            "outer_elapsed_seconds": time.monotonic() - started,
            "diagnostics": child.diagnostics(), "samples": samples,
            "terminal_sample_errors": terminal_sample_errors,
            "rusage": {
                "cpu_user_seconds": usage.ru_utime, "cpu_system_seconds": usage.ru_stime,
                "max_rss_kib": usage.ru_maxrss, "voluntary_context_switches": usage.ru_nvcsw,
                "involuntary_context_switches": usage.ru_nivcsw,
                "scope": "whole native child process, not the request measurement interval",
            },
        }
    return result


def normalize(native, process, publication):
    errors = list(native["error_details"])
    if native["errors"] and not errors:
        errors.append(f"native reported {native['errors']} errors without details")
    if native["errors"] and native["requests_per_second"] is not None:
        errors.append("invalid native run exposed non-null throughput")
    if native["histogram"]["kind"] != "log2_32_subbuckets" or native["histogram"]["overflow_count"]:
        errors.append("native histogram specification mismatch or overflow")
    if native["warmup_count"] or native["warmup_errors"] or native["config"]["warmup_ms"]:
        errors.append("native comparison invocation must use separate warmup process")
    valid = bool(native["valid"] and native["errors"] == 0 and process["exit"] == 0 and not errors)
    usage = process["rusage"]
    return {
        "schema": 1, "mode": "http-native", "valid": valid, "errors": errors,
        "native_error_count": native["errors"], "attempts": native["attempts"],
        "successes": native["count"], "validated_payload_bytes": native["validated_payload_bytes"],
        "elapsed_seconds": native["elapsed_seconds"],
        "invocation_seconds": process["outer_elapsed_seconds"],
        "successes_per_second": native["requests_per_second"] if valid else None,
        **{name + "_ms": native["latency_us"][name] / 1000 if native["latency_us"][name] is not None else None
           for name in ("p50", "p95", "p99")},
        "latency_histogram": None, "histogram": native["histogram"],
        "histogram_scope": "native exports quantiles/specification, not raw bins; do not pool quantiles",
        "measurement_cpu_total_seconds": native["process_cpu_seconds"],
        "cpu_user_seconds": None, "cpu_system_seconds": None,
        "process_cpu_user_seconds": usage["cpu_user_seconds"],
        "process_cpu_system_seconds": usage["cpu_system_seconds"],
        "max_rss_kib": usage["max_rss_kib"],
        "voluntary_context_switches": usage["voluntary_context_switches"],
        "involuntary_context_switches": usage["involuntary_context_switches"],
        "max_sampled_fds": max((row["fds"] for row in process["samples"]), default=None),
        "final_sampled_fds": process["samples"][-1]["fds"] if process["samples"] else None,
        "new_connections": native["connections"], "limit_retirements": native["limit_retirements"],
        "fresh_closes": native["fresh_closes"], "reused_connections": None,
        "configuration": native["config"], "publication": publication,
        "measurement_scope": native["measurement_scope"],
        "resource_scope": usage["scope"], "native": native, "process": process,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--publication", required=True, type=Path)
    parser.add_argument("--url", required=True)
    parser.add_argument("--connections", required=True, type=int)
    parser.add_argument("--size", required=True, type=int)
    parser.add_argument("--duration-ms", required=True, type=int)
    parser.add_argument("--mode", choices=("keepalive", "fresh"), required=True)
    parser.add_argument("--client-cpus", default="2,4,6")
    args = parser.parse_args()
    if not 1 <= args.duration_ms <= 30000 or args.connections not in (1, 16):
        parser.error("invalid bounded duration/concurrency")
    publication = json.loads(args.publication.read_text())
    binary = publication["binary"]
    if digest(binary["path"]) != binary["sha256"]:
        raise ValueError("native publication hash mismatch")
    directory = ARTIFACTS / f"native-invocation-{uuid.uuid4().hex}"
    directory.mkdir(parents=True)
    output = directory / "native.json"
    command = [
        "taskset", "-c", args.client_cpus, binary["path"],
        "--url", args.url, "--connections", str(args.connections),
        "--warmup-ms", "0", "--duration-ms", str(args.duration_ms),
        "--size", str(args.size), "--mode", args.mode, "--timeout-ms", "5000",
        "--max-requests-per-connection", "1000000000", "--json", str(output),
    ]
    process = execute(command, args.duration_ms / 1000 + 20)
    if not output.is_file() or output.stat().st_size > 128 * 1024:
        raise RuntimeError(f"missing/oversized native result: {process['diagnostics']}")
    native = json.loads(output.read_text())
    result = normalize(native, process, publication)
    if native["limit_retirements"]:
        result["errors"].append("high native client request cap was reached; comparison policy invalid")
        result["valid"] = False
        result["successes_per_second"] = None
    if digest(binary["path"]) != binary["sha256"]:
        raise RuntimeError("native executable changed during invocation")
    (directory / "normalized.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result))
    return 0 if result["valid"] else 1


if __name__ == "__main__":
    sys.exit(main())
