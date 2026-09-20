#!/usr/bin/env python3
"""Run allocation-only controls; never collect elapsed-time measurements."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--summary", type=Path, required=True)
    parser.add_argument("--repetitions", type=int, default=3)
    args = parser.parse_args()
    assert args.repetitions > 0
    assert "BENCH_DIAGNOSTICS" not in os.environ
    binary = args.binary.resolve()
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    cases = [(case, concurrency, 65536)
             for case in ["empty", "duplex", "32k", "1m"]
             for concurrency in [1, 8, 64, 128]]
    cases += [("duplex", concurrency, 17) for concurrency in [1, 8]]
    groups = {}
    completed = 0
    with args.output.open("w") as output:
        for repetition in range(args.repetitions):
            modes = ["direct", "selected", "auto"]
            if repetition % 2:
                modes.reverse()
            for case, concurrency, fragment in cases:
                for phase in ["cold", "steady"]:
                    batches = 1 if phase == "cold" else 4
                    for mode in modes:
                        command = [str(binary), mode, case, str(concurrency),
                                   str(fragment), str(batches), phase]
                        process = subprocess.run(command, capture_output=True, text=True)
                        if process.returncode:
                            raise RuntimeError(
                                f"Strict allocation workload failed: {command}\n{process.stderr}"
                            )
                        result = json.loads(process.stdout)
                        assert result["mode"] == mode and result["case"] == case
                        assert result["phase"] == phase and result["batches"] == batches
                        assert result["fragment"] == fragment
                        assert result["concurrency"] == concurrency
                        assert result["exchanges"] == batches * concurrency
                        assert result["payload_bytes"] == result["exchanges"] * (
                            result["request_bytes"] + result["response_bytes"])
                        assert result["all_payload_bytes_compared"]
                        assert result["shutdown_settled"]
                        assert result["teardown_live_change"] == 0, result
                        counts = result["allocation_counts"]
                        assert counts["requested_peak_live"] >= max(
                            counts["requested_live_start"], counts["requested_live_end"])
                        assert "elapsed_ns" not in result and "ns_per_exchange" not in result
                        key = (mode, case, concurrency, fragment, phase)
                        groups.setdefault(key, []).append(result)
                        record = dict(result, repetition=repetition, binary_sha256=digest,
                                      command=command)
                        output.write(json.dumps(record, sort_keys=True) + "\n")
                        output.flush()
                        completed += 1
    cells = []
    for key, results in groups.items():
        assert all(result == results[0] for result in results), (
            "Allocation counts or workload totals changed between repeats", key, results
        )
        cells.append(dict(results[0], identical_repetitions=len(results)))
    summary = dict(schema=1, binary=str(binary), binary_sha256=digest,
                   cells=len(cells), successful_runs=completed, failed_runs=0,
                   repetitions=args.repetitions, all_repetitions_identical=True,
                   results=cells)
    args.summary.write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n")
    print(json.dumps({key: value for key, value in summary.items() if key != "results"}))


if __name__ == "__main__":
    main()
