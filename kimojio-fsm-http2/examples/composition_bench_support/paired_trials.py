#!/usr/bin/env python3
"""Alternate immutable baseline/PoC binaries and stop on any strict failure."""
import hashlib
import json
import math
import os
import pathlib
import random
import statistics
import subprocess
import sys

assert "BENCH_DIAGNOSTICS" not in os.environ
baseline, candidate = [pathlib.Path(p).resolve() for p in sys.argv[1:3]]
summary = json.loads(pathlib.Path(sys.argv[3]).read_text())
out = pathlib.Path(sys.argv[4])
out.mkdir(parents=True, exist_ok=True)
cases = [("empty", 1, 65536), ("empty", 128, 65536), ("empty", 1, 17),
         ("duplex", 8, 65536), ("32k", 8, 65536), ("1m", 8, 65536)]
binaries = {"baseline": baseline, "candidate": candidate}
hashes = {name: hashlib.sha256(path.read_bytes()).hexdigest()
          for name, path in binaries.items()}
batch_counts = {}
for case, concurrency, fragment in cases:
    row = next(r for r in summary if (r["mode"], r["case"], r["concurrency"],
               r["fragment"], r["phase"]) == ("direct", case, concurrency, fragment, "steady"))
    assert not row["blocked"]
    batch_counts[(case, concurrency, fragment)] = max(
        1, math.ceil(400_000_000 / (row["median_ns"] * concurrency)))
rows = []
with (out / "paired-trials.jsonl").open("w") as log:
    for trial in range(9):
        order = ["baseline", "candidate"] if trial % 2 == 0 else ["candidate", "baseline"]
        for case, concurrency, fragment in cases:
            batches = batch_counts[(case, concurrency, fragment)]
            for variant in order:
                command = ["taskset", "-c", "2", str(binaries[variant]), "direct", case,
                           str(concurrency), str(fragment), str(batches), "steady"]
                result = subprocess.run(command, capture_output=True, text=True, timeout=120)
                if result.returncode:
                    log.write(json.dumps(dict(variant=variant, trial=trial, command=command,
                                              exit=result.returncode, error=result.stderr)) + "\n")
                    log.flush()
                    raise SystemExit(f"strict failure: {command}\n{result.stderr}")
                row = json.loads(result.stdout)
                row.update(variant=variant, trial=trial, command=command,
                           binary_sha256=hashes[variant])
                rows.append(row)
                log.write(json.dumps(row) + "\n")
                log.flush()
        print(f"paired trial {trial + 1}/9 complete", flush=True)

rng = random.Random(20260920)
results = []
for case, concurrency, fragment in cases:
    selected = [r for r in rows if (r["case"], r["concurrency"], r["fragment"])
                == (case, concurrency, fragment)]
    values = {variant: [next(r["ns_per_exchange"] for r in selected
                            if r["variant"] == variant and r["trial"] == trial)
                        for trial in range(9)] for variant in binaries}
    ratios = [poc / base for poc, base in zip(values["candidate"], values["baseline"])]
    bootstrap = sorted(statistics.median(rng.choices(ratios, k=9)) for _ in range(10000))
    results.append(dict(case=case, concurrency=concurrency, fragment=fragment,
                        batches=batch_counts[(case, concurrency, fragment)], trials=9,
                        baseline_median_ns=statistics.median(values["baseline"]),
                        candidate_median_ns=statistics.median(values["candidate"]),
                        baseline_range_ns=[min(values["baseline"]), max(values["baseline"])],
                        candidate_range_ns=[min(values["candidate"]), max(values["candidate"])],
                        paired_ratios=ratios, paired_median_ratio=statistics.median(ratios),
                        paired_bootstrap_95=[bootstrap[249], bootstrap[9749]]))
(out / "paired-summary.json").write_text(json.dumps(results, indent=2) + "\n")
