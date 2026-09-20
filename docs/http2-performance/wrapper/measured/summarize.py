"""Trial-cluster summaries. Cold groups contain independent fresh processes."""
import collections
import csv
import itertools
import json
from pathlib import Path
import statistics
from evidence_io import read_text

HERE = Path(__file__).resolve().parent


def percentile(values, p):
    values = sorted(values)
    position = (len(values) - 1) * p
    lo = int(position)
    hi = min(lo + 1, len(values) - 1)
    return values[lo] + (values[hi] - values[lo]) * (position - lo)


def interval(values):
    samples = [statistics.median(sample) for sample in itertools.product(values, repeat=len(values))]
    return percentile(samples, 0.025), percentile(samples, 0.975)


def main():
    records = [json.loads(line) for line in read_text(HERE / "matrix.jsonl").splitlines()]
    assert not any("failure" in r for r in records)
    cells = collections.defaultdict(lambda: collections.defaultdict(list))
    for record in records:
        r = record["report"]
        cells[(record["label"], r["concurrency"], r["phase"], r["backend"])][record["trial"]].append(r)
    assert len(cells) == 80
    summaries = []
    values_by_cell = {}
    for key, trials in sorted(cells.items()):
        label, concurrency, phase, backend = key
        assert sorted(trials) == list(range(5))
        wall = []
        cpu = []
        latency = []
        payload = None
        for _, reports in sorted(trials.items()):
            exchanges = sum(r["measured_exchanges"] for r in reports)
            wall.append(sum(r["elapsed_ns"] for r in reports) / exchanges)
            cpu.append(sum(r["process_cpu_ns"] for r in reports) / exchanges)
            latency.append(sum(r["elapsed_ns"] for r in reports) / sum(r["cohorts"] for r in reports))
            payload = reports[0]["request_bytes"] + reports[0]["response_bytes"]
        values_by_cell[key] = {"wall": wall, "cpu": cpu}
        low, high = interval(wall)
        median = statistics.median(wall)
        row = {
            "case": label, "concurrency": concurrency, "phase": phase, "backend": backend,
            "trial_groups": 5, "process_runs": sum(map(len, trials.values())),
            "cohorts_per_process": trials[0][0]["cohorts"],
            "warmup_cohorts": trials[0][0]["warmup_cohorts"],
            "wall_ns_per_exchange_median": median,
            "wall_ns_per_exchange_min": min(wall), "wall_ns_per_exchange_max": max(wall),
            "wall_ns_per_exchange_p25": percentile(wall, 0.25),
            "wall_ns_per_exchange_p75": percentile(wall, 0.75),
            "wall_median_bootstrap95_low": low, "wall_median_bootstrap95_high": high,
            "cpu_ns_per_exchange_median": statistics.median(cpu),
            "cpu_ns_per_exchange_min": min(cpu), "cpu_ns_per_exchange_max": max(cpu),
            "cohort_completion_ns_median": statistics.median(latency),
            "exchanges_per_second": 1e9 / median,
            "bidirectional_payload_mib_per_second": payload * 1e9 / median / 1024**2,
            "cold_connection_p50_ns": statistics.median([r["elapsed_ns"] for rs in trials.values() for r in rs]) if phase == "cold" else "",
            "cold_connection_p95_ns": percentile([r["elapsed_ns"] for rs in trials.values() for r in rs], 0.95) if phase == "cold" else "",
        }
        summaries.append(row)
    with (HERE / "matrix-summary.csv").open("w") as output:
        writer = csv.DictWriter(output, fieldnames=list(summaries[0]), lineterminator="\n")
        writer.writeheader()
        writer.writerows(summaries)
    ratios = []
    for key, values in sorted(values_by_cell.items()):
        label, concurrency, phase, backend = key
        if backend != "native":
            continue
        generic = values_by_cell[label, concurrency, phase, "generic"]
        pairs = [g / n for g, n in zip(generic["wall"], values["wall"])]
        cpu_pairs = [g / n for g, n in zip(generic["cpu"], values["cpu"])]
        low, high = interval(pairs)
        ratios.append({
            "case": label, "concurrency": concurrency, "phase": phase,
            "generic_over_native_wall_median": statistics.median(pairs),
            "bootstrap95_low": low, "bootstrap95_high": high,
            "generic_over_native_cpu_median": statistics.median(cpu_pairs),
            "paired_wall_ratios": pairs,
        })
    (HERE / "paired-ratios.json").write_text(json.dumps(ratios, indent=2) + "\n")
    report = {
        "cells": len(cells), "trial_groups": len(cells) * 5, "process_runs": len(records),
        "failures": 0,
        "measured_exchanges": sum(r["report"]["measured_exchanges"] for r in records),
        "retirements_per_endpoint_including_warmup": sum(r["report"]["client_retirements"] for r in records),
        "compared_payload_bytes_including_warmup": sum(r["report"]["checked_payload_bytes_including_warmup"] for r in records),
        "required_overlap_witnesses": sum(r["report"]["early_response_witnesses_including_warmup"] for r in records if r["report"]["overlap_required"]),
        "minimum_steady_window_ns": min(r["report"]["elapsed_ns"] for r in records if r["report"]["phase"] == "steady"),
        "maximum_steady_window_ns": max(r["report"]["elapsed_ns"] for r in records if r["report"]["phase"] == "steady"),
        "uncertainty": "exact 5^5 percentile bootstrap of trial-group medians; five groups only; no request-latency tail claim",
    }
    (HERE / "summary.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
