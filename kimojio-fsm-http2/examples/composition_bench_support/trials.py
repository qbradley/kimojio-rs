#!/usr/bin/env python3
"""Run five alternating CPU2 trials; retain failures rather than timing them."""
import hashlib
import itertools
import json
import os
import pathlib
import statistics
import subprocess
import sys

binary = pathlib.Path(sys.argv[1]).resolve()
assert "BENCH_DIAGNOSTICS" not in os.environ, "diagnostic instrumentation is not a timing workload"
out = pathlib.Path(sys.argv[2])
stop_on_failure = "--stop-on-failure" in sys.argv[3:]
out.mkdir(parents=True, exist_ok=True)
cases = list(itertools.product(["empty", "duplex", "32k", "1m", "paused"],
                               [1, 8, 64, 128], [17, 1024, 65536]))
work = [(case, c, f, max(1, {"empty": 65536, "paused": 65536,
                            "duplex": 16384, "32k": 4096, "1m": 128}[case]
                              // c // (8 if f == 17 else 1)), "steady")
        for case, c, f in cases]
work += [("empty", 1, f, 4096, "construct") for f in [17, 1024, 65536]]
failures = set()
rows = []
sha = hashlib.sha256(binary.read_bytes()).hexdigest()
with (out / "trials.jsonl").open("w") as log:
    for trial in range(5):
        modes = ["direct", "selected", "auto"] if trial % 2 == 0 else ["auto", "selected", "direct"]
        for case, c, fragment, batches, phase in work:
            for mode in modes:
                key = (mode, case, c, fragment, phase)
                if key in failures:
                    continue
                command = ["taskset", "-c", "2", str(binary), mode, case,
                           str(c), str(fragment), str(batches), phase]
                result = subprocess.run(command, capture_output=True, text=True, timeout=90)
                if result.returncode:
                    row = dict(mode=mode, case=case, concurrency=c, fragment=fragment,
                               phase=phase, batches=batches, error=result.stderr,
                               exit=result.returncode)
                    failures.add(key)
                else:
                    row = json.loads(result.stdout)
                row.update(trial=trial, binary_sha256=sha, command=command)
                rows.append(row)
                log.write(json.dumps(row) + "\n")
                log.flush()
                if result.returncode and stop_on_failure:
                    print(f"STOP: strict cell failed: {command}\n{result.stderr}", file=sys.stderr)
                    sys.exit(1)
        print(f"trial {trial + 1}: {len(rows)} rows, {len(failures)} blocked cases", flush=True)
summary = []
for case, c, fragment, batches, phase in work:
    for mode in ["direct", "selected", "auto"]:
        selected = [r for r in rows if (r["mode"], r["case"], r["concurrency"],
                    r["fragment"], r["phase"]) == (mode, case, c, fragment, phase)]
        values = [r["ns_per_exchange"] for r in selected if "error" not in r]
        row = dict(mode=mode, case=case, concurrency=c, fragment=fragment, phase=phase,
                   trials=len(values), blocked=not values)
        if values:
            row.update(median_ns=statistics.median(values), min_ns=min(values), max_ns=max(values))
        summary.append(row)
(out / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
