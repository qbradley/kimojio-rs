#!/usr/bin/env python3
"""Run serialized, byte-checked wrapper comparisons against frozen binaries."""

import argparse
import hashlib
import json
import math
from pathlib import Path
import random
import statistics
import subprocess
import sys


CASES = [
    {"name": "empty", "request_bytes": 0, "response_bytes": 0, "iterations": 20_000},
    {"name": "small", "request_bytes": 0, "response_bytes": 128, "iterations": 20_000},
    {"name": "post-small", "request_bytes": 128, "response_bytes": 128, "iterations": 20_000},
    {"name": "large", "request_bytes": 65_536, "response_bytes": 65_536, "iterations": 2_000},
    {
        "name": "chunked",
        "request_bytes": 1_048_576,
        "response_bytes": 1_048_576,
        "chunked": True,
        "iterations": 200,
    },
    {
        "name": "fragmented",
        "request_bytes": 8_192,
        "response_bytes": 8_192,
        "chunk_bytes": 512,
        "chunked": True,
        "iterations": 2_000,
    },
]


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def validate_result(result, case, warmup):
    expected = {
        "valid": True,
        "connections_created": 1,
        "reconnects": 0,
        "warmup_exchanges": warmup,
        "measured_exchanges": case["iterations"],
        "server_exchanges": case["iterations"] + warmup,
        "request_bytes": case["request_bytes"],
        "response_bytes": case["response_bytes"],
        "chunk_bytes": case.get("chunk_bytes", 16_384),
        "chunked": case.get("chunked", False),
        "validated_payload_bytes": case["iterations"]
        * (case["request_bytes"] + case["response_bytes"]),
    }
    for key, value in expected.items():
        if result.get(key) != value:
            raise ValueError(f"{key}: expected {value!r}, got {result.get(key)!r}")
    elapsed = result["elapsed_seconds"]
    latency = result["nanoseconds_per_exchange"]
    if not math.isfinite(elapsed) or elapsed <= 0 or not math.isfinite(latency) or latency <= 0:
        raise ValueError("nonpositive or nonfinite timing")
    if not math.isclose(latency, elapsed * 1e9 / case["iterations"], rel_tol=1e-9):
        raise ValueError("timing denominator does not match measured exchanges")


def summarize(rows):
    groups = {}
    for row in rows:
        groups.setdefault((row["candidate"], row["case"]), []).append(
            row["result"]["nanoseconds_per_exchange"]
        )
    return [
        {
            "candidate": candidate,
            "case": case,
            "trials": len(values),
            "median_ns": statistics.median(values),
            "min_ns": min(values),
            "max_ns": max(values),
            "population_stdev_ns": statistics.pstdev(values),
        }
        for (candidate, case), values in groups.items()
    ]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--trials", type=int, default=5)
    parser.add_argument("--cpu", type=int, default=2)
    parser.add_argument("--seed", type=int, default=20260920)
    parser.add_argument("--case", action="append", choices=[case["name"] for case in CASES])
    args = parser.parse_args()
    if not 1 <= args.trials <= 100 or args.cpu < 0:
        parser.error("trials must be 1..100 and cpu must be nonnegative")
    manifest = json.loads(args.manifest.read_text())
    candidates = manifest["candidates"]
    if not candidates or len({item["name"] for item in candidates}) != len(candidates):
        parser.error("candidate names must be nonempty and unique")
    for candidate in candidates:
        if not candidate["revision"] or not candidate["build_command"]:
            parser.error("every candidate needs its revision and build command")
        candidate["binary"] = str(Path(candidate["binary"]).resolve())
        candidate["binary_sha256"] = digest(Path(candidate["binary"]))
    cases = [case for case in CASES if not args.case or case["name"] in args.case]
    artifacts = args.output.with_suffix("")
    artifacts.mkdir(parents=True, exist_ok=True)
    report = {
        "schema": 1,
        "complete": False,
        "valid": False,
        "cpu": args.cpu,
        "seed": args.seed,
        "manifest": manifest,
        "cases": cases,
        "rows": [],
        "statistics": "medians and trial ranges; no confidence or causal claim",
    }
    rng = random.Random(args.seed)
    for trial in range(args.trials):
        order = [(candidate, case) for candidate in candidates for case in cases]
        rng.shuffle(order)
        for candidate, case in order:
            warmup = min(1_000, max(10, case["iterations"] // 10))
            stem = f"{candidate['name']}-{case['name']}-{trial}"
            output = artifacts / f"{stem}.json"
            command = [
                "taskset", "-c", str(args.cpu), candidate["binary"],
                "--iterations", str(case["iterations"]), "--warmup", str(warmup),
                "--request-bytes", str(case["request_bytes"]),
                "--response-bytes", str(case["response_bytes"]),
                "--chunk-bytes", str(case.get("chunk_bytes", 16_384)),
                "--timeout-seconds", "90", "--json", str(output),
            ]
            if case.get("chunked"):
                command.append("--chunked")
            row = {
                "candidate": candidate["name"],
                "case": case["name"],
                "trial": trial,
                "command": command,
                "valid": False,
            }
            try:
                with (artifacts / f"{stem}.stdout").open("w") as stdout:
                    with (artifacts / f"{stem}.stderr").open("w") as stderr:
                        process = subprocess.run(
                            command, stdout=stdout, stderr=stderr, timeout=120, check=False
                        )
                row["exit_code"] = process.returncode
                if process.returncode != 0:
                    raise ValueError(f"benchmark exited {process.returncode}")
                result = json.loads(output.read_text())
                validate_result(result, case, warmup)
                if digest(Path(candidate["binary"])) != candidate["binary_sha256"]:
                    raise ValueError("binary changed during comparison")
                row["result"] = result
                row["valid"] = True
            except (OSError, ValueError, KeyError, subprocess.TimeoutExpired) as error:
                row["error"] = str(error)
                print(f"{stem}: {error}", file=sys.stderr)
            report["rows"].append(row)
            args.output.write_text(json.dumps(report, indent=2) + "\n")
    report["complete"] = True
    report["valid"] = all(row["valid"] for row in report["rows"])
    report["summaries"] = summarize(report["rows"]) if report["valid"] else []
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    for summary in report["summaries"]:
        print(
            f"{summary['candidate']:20} {summary['case']:12} "
            f"{summary['median_ns'] / 1000:9.3f} us/exchange "
            f"[{summary['min_ns'] / 1000:.3f}, {summary['max_ns'] / 1000:.3f}]"
        )
    return 0 if report["valid"] else 1


if __name__ == "__main__":
    sys.exit(main())
