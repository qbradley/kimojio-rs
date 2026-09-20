"""Paired trial-cluster distributions; no trimming or optional stopping."""
import collections
import csv
import json
import math
from pathlib import Path
import random
import statistics
import sys

HERE = Path(__file__).resolve().parent
sys.path.append(str(HERE.parent / "measured"))
from evidence_io import read_text


def interval(values):
    rng = random.Random(458)
    medians = sorted(statistics.median(rng.choices(values, k=len(values))) for _ in range(20000))
    return medians[499], medians[19499]


def summarize(name):
    rows = [json.loads(line) for line in read_text(HERE / (name + ".jsonl")).splitlines()]
    groups = collections.defaultdict(lambda: collections.defaultdict(list))
    for row in rows:
        assert "failure" not in row and row["returncode"] == 0 and row["report"]["valid"]
        r = row["report"]
        key = (row["label"], r["concurrency"], r["phase"], r["backend"], row["variant"])
        groups[key][row["version"], row["trial"]].append(r)
    result = []
    for key, cohorts in sorted(groups.items()):
        record = dict(zip(("case", "concurrency", "phase", "backend", "variant"), key))
        by_version = {}
        for version in ("baseline", "candidate"):
            wall, cpu = [], []
            for trial in range(9):
                runs = cohorts[version, trial]
                assert runs
                count = sum(r["measured_exchanges"] for r in runs)
                wall.append(sum(r["elapsed_ns"] for r in runs) / count)
                cpu.append(sum(r["process_cpu_ns"] for r in runs) / count)
            by_version[version] = {"wall": wall, "cpu": cpu}
            for metric, values in by_version[version].items():
                record[f"{version}_{metric}_ns_median"] = statistics.median(values)
                record[f"{version}_{metric}_ns_min"] = min(values)
                record[f"{version}_{metric}_ns_max"] = max(values)
        for metric in ("wall", "cpu"):
            ratios = [c / b for c, b in zip(by_version["candidate"][metric], by_version["baseline"][metric])]
            lo, hi = interval(ratios)
            record[metric + "_candidate_over_baseline"] = statistics.median(ratios)
            record[metric + "_bootstrap95_low"] = lo
            record[metric + "_bootstrap95_high"] = hi
            record[metric + "_paired_ratios"] = ratios
        record["trials"] = by_version
        result.append(record)
    (HERE / (name + "-summary.json")).write_text(json.dumps(result, indent=2) + "\n")
    fields = [k for k in result[0] if k not in ("trials", "wall_paired_ratios", "cpu_paired_ratios")]
    with (HERE / (name + "-summary.csv")).open("w") as output:
        writer = csv.DictWriter(output, fieldnames=fields, extrasaction="ignore", lineterminator="\n")
        writer.writeheader()
        writer.writerows(result)
    if name == "matrix":
        aggregate = []
        for category in ("small", "large", "all"):
            selected = [r for r in result if r["phase"] == "steady"
                        and (category == "all" or (r["case"].endswith("1m") == (category == "large")))]
            per_backend = {}
            for backend in ("native", "generic"):
                cells = [r for r in selected if r["backend"] == backend]
                ratios = [math.exp(statistics.mean(math.log(r["wall_paired_ratios"][t]) for r in cells))
                          for t in range(9)]
                per_backend[backend] = ratios
                lo, hi = interval(ratios)
                aggregate.append({"category": category, "backend": backend, "cells": len(cells),
                                  "paired_trial_geomeans": ratios, "median": statistics.median(ratios),
                                  "bootstrap95_low": lo, "bootstrap95_high": hi})
            adjusted = [g / n for g, n in zip(per_backend["generic"], per_backend["native"])]
            lo, hi = interval(adjusted)
            aggregate.append({"category": category, "backend": "generic_divided_by_native_control",
                              "paired_trial_geomeans": adjusted, "median": statistics.median(adjusted),
                              "bootstrap95_low": lo, "bootstrap95_high": hi})
        (HERE / "aggregate.json").write_text(json.dumps(aggregate, indent=2) + "\n")
    return {"file": name, "cells": len(result), "runs": len(rows),
            "measured_exchanges": sum(r["report"]["measured_exchanges"] for r in rows),
            "required_overlap_witnesses": sum(r["report"]["early_response_witnesses_including_warmup"]
                                               for r in rows if r["report"]["overlap_required"])}


if __name__ == "__main__":
    totals = [summarize(name) for name in ("matrix", "sensitivity")]
    (HERE / "totals.json").write_text(json.dumps(totals, indent=2) + "\n")
    print(json.dumps(totals, indent=2))
