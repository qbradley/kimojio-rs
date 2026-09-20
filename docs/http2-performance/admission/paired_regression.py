#!/usr/bin/env python3
"""Alternate immutable revisions for common-path regression measurements."""
import hashlib
import json
import math
import os
from pathlib import Path
import random
import statistics
import subprocess
import sys

assert "BENCH_DIAGNOSTICS" not in os.environ
baseline, admission = [Path(path).resolve() for path in sys.argv[1:3]]
old_summary = json.loads(Path(sys.argv[3]).read_text())
output = Path(sys.argv[4])
output.mkdir(parents=True, exist_ok=True)
binaries = {"baseline": baseline, "admission": admission}
hashes = {name: hashlib.sha256(path.read_bytes()).hexdigest()
          for name, path in binaries.items()}
cases = [
    ("empty", 1, 65536), ("empty", 128, 65536), ("empty", 1, 17),
    ("duplex", 8, 65536), ("32k", 8, 65536), ("32k", 64, 17),
    ("1m", 1, 65536), ("1m", 8, 65536), ("1m", 128, 65536),
]
work = []
for mode in ["direct", "selected", "auto"]:
    for case, concurrency, fragment in cases:
        old = next(row for row in old_summary if (
            row["mode"], row["case"], row["concurrency"], row["fragment"], row["phase"]
        ) == (mode, case, concurrency, fragment, "steady"))
        assert old["trials"] == 5 and not old["blocked"]
        batches = max(1, math.ceil(400_000_000 / (old["median_ns"] * concurrency)))
        work.append((mode, case, concurrency, fragment, batches))

rows = []
with (output / "paired-trials.jsonl").open("w") as log:
    for trial in range(9):
        order = ["baseline", "admission"] if trial % 2 == 0 else ["admission", "baseline"]
        for mode, case, concurrency, fragment, batches in work:
            pair = {}
            for variant in order:
                command = ["taskset", "-c", "2", str(binaries[variant]), mode, case,
                           str(concurrency), str(fragment), str(batches), "steady"]
                result = subprocess.run(command, capture_output=True, text=True, timeout=120)
                if result.returncode:
                    log.write(json.dumps(dict(variant=variant, trial=trial, command=command,
                                              exit=result.returncode, error=result.stderr)) + "\n")
                    log.flush()
                    raise SystemExit(f"Strict regression cell failed: {command}\n{result.stderr}")
                data = json.loads(result.stdout)
                assert data["all_payload_bytes_compared"]
                assert data["exchanges"] == batches * concurrency
                pair[variant] = data
                row = dict(data, variant=variant, trial=trial, command=command,
                           binary_sha256=hashes[variant])
                rows.append(row)
                log.write(json.dumps(row) + "\n")
                log.flush()
            stable = [key for key in pair["baseline"]
                      if key not in ["elapsed_ns", "ns_per_exchange"]]
            assert all(pair["baseline"][key] == pair["admission"][key]
                       for key in stable), (mode, case, concurrency, fragment, pair)
        print(f"paired trial {trial + 1}/9 complete", flush=True)

rng = random.Random(20260920)
summary = []
for mode, case, concurrency, fragment, batches in work:
    selected = [row for row in rows if (
        row["mode"], row["case"], row["concurrency"], row["fragment"]
    ) == (mode, case, concurrency, fragment)]
    values = {variant: [next(row["ns_per_exchange"] for row in selected
                            if row["variant"] == variant and row["trial"] == trial)
                        for trial in range(9)] for variant in binaries}
    ratios = [new / old for new, old in zip(values["admission"], values["baseline"])]
    bootstrap = sorted(statistics.median(rng.choices(ratios, k=9)) for _ in range(10000))
    summary.append(dict(
        mode=mode, case=case, concurrency=concurrency, fragment=fragment,
        batches=batches, pairs=9,
        baseline_median_ns=statistics.median(values["baseline"]),
        admission_median_ns=statistics.median(values["admission"]),
        baseline_range_ns=[min(values["baseline"]), max(values["baseline"])],
        admission_range_ns=[min(values["admission"]), max(values["admission"])],
        paired_ratios=ratios, paired_median_ratio=statistics.median(ratios),
        paired_bootstrap_95=[bootstrap[249], bootstrap[9749]],
    ))
(output / "paired-summary.json").write_text(json.dumps(summary, indent=2) + "\n")
