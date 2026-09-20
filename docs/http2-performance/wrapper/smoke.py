#!/usr/bin/env python3
"""Bounded correctness smoke run, not statistical performance qualification."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess


def cells():
    for backend in ("native", "generic"):
        for concurrency in (1, 8, 32, 128):
            for case in ("empty", "fixed", "stream", "duplex"):
                yield "runtime", backend, case, concurrency, "steady", "static", 1048576
        for concurrency in (1, 128):
            yield "runtime", backend, "duplex", concurrency, "cold", "static", 1048576
        yield "runtime", backend, "duplex", 8, "steady", "owned", 1048576
        yield "runtime", backend, "stream", 8, "steady", "static", 32768
        for phase in ("cold", "steady"):
            for concurrency in (1, 128):
                for case in ("empty", "fixed"):
                    yield "allocations", backend, case, concurrency, phase, "static", 1048576
            for payload in ("static", "owned"):
                yield "allocations", backend, "duplex", 8, phase, payload, 1048576


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binaries", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    hashes = {
        name: hashlib.sha256((args.binaries / name).read_bytes()).hexdigest()
        for name in ("runtime", "allocations")
    }
    completed = 0
    with args.output.open("x") as output:
        for executable, backend, case, concurrency, phase, payload, size in cells():
            cohorts, warmup = (1, 0) if phase == "cold" else (2, 1)
            command = [
                "taskset", "-c", "8-31", str(args.binaries / executable),
                "--backend", backend, "--case", case, "--concurrency", str(concurrency),
                "--phase", phase, "--cohorts", str(cohorts), "--warmup", str(warmup),
                "--bytes", str(size), "--chunk", "16384", "--payload", payload,
                "--timeout-seconds", "60",
            ]
            record = {"command": command, "binary_sha256": hashes[executable]}
            try:
                result = subprocess.run(command, capture_output=True, text=True, timeout=75)
                record.update(returncode=result.returncode, stderr=result.stderr)
                try:
                    report = json.loads(result.stdout)
                    record["report"] = report
                except json.JSONDecodeError:
                    record["stdout"] = result.stdout
                    raise AssertionError("benchmark did not emit one JSON object") from None
                assert result.returncode == 0 and report["valid"], record
                assert (report["backend"], report["case"], report["phase"]) == (
                    backend, case, phase
                )
                assert report["concurrency"] == concurrency
                assert report["measured_exchanges"] == cohorts * concurrency
                assert report["warmup_cohorts"] == warmup
                sizes = (0, 128) if case == "empty" else (
                    (4096, 4096) if case == "fixed" else (size, size)
                )
                assert (report["request_bytes"], report["response_bytes"]) == sizes
                total = (cohorts + warmup) * concurrency
                assert report["client_retirements"] == total
                assert report["server_retirements"] == total
                assert report["drivers_closed_successfully"] == 2
                assert report["connections_created"] == 1 and report["reconnects"] == 0
                assert report["checked_payload_bytes_including_warmup"] == total * (
                    report["request_bytes"] + report["response_bytes"]
                )
                if case == "duplex":
                    assert report["early_response_witnesses_including_warmup"] == total
                if executable == "allocations":
                    allocation = report["allocations"]
                    origins = allocation["origin_buckets"]
                    for boundary in ("start", "end"):
                        field = "requested_live_" + boundary
                        assert allocation[field] == sum(origin[field] for origin in origins)
                    assert allocation["requested_peak_live"] >= max(
                        allocation["requested_live_start"], allocation["requested_live_end"]
                    )
            except Exception as error:
                record["failure"] = str(error)
                output.write(json.dumps(record) + "\n")
                output.flush()
                raise
            output.write(json.dumps(record) + "\n")
            output.flush()
            completed += 1
    print(json.dumps({"successful_runs": completed, "statistical_run": False, "cpus": "8-31"}))


if __name__ == "__main__":
    main()
