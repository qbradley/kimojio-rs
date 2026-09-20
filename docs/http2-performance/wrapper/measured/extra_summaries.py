"""Summarize separate allocation probes and bounded timing sensitivities."""
import collections
import csv
import json
from pathlib import Path
import statistics

from summarize import interval
from evidence_io import read_text

HERE = Path(__file__).resolve().parent


def records(name):
    return [json.loads(line) for line in read_text(HERE / name).splitlines()]


def main():
    allocation_rows = []
    for record in records("allocations.jsonl"):
        report = record["report"]
        assert report["valid"] and record["returncode"] == 0
        counts = report["allocations"]
        buckets = counts["origin_buckets"]
        assert sum(b["requested_live_start"] for b in buckets) == counts["requested_live_start"]
        assert sum(b["requested_live_end"] for b in buckets) == counts["requested_live_end"]
        row = {
            "case": record["label"], "concurrency": report["concurrency"],
            "phase": report["phase"], "backend": report["backend"],
            "measured_exchanges": report["measured_exchanges"],
            "warmup_cohorts": report["warmup_cohorts"],
        }
        for event in ("allocations", "reallocations", "deallocations"):
            row[event] = sum(b[event] for b in buckets)
            row[event + "_per_exchange"] = row[event] / report["measured_exchanges"]
        for field in ("requested_live_start", "requested_live_end", "requested_peak_live"):
            row[field] = counts[field]
        row["requested_live_change"] = row["requested_live_end"] - row["requested_live_start"]
        for bucket in buckets:
            row[bucket["origin"] + "_allocations"] = bucket["allocations"]
        allocation_rows.append(row)
    assert len(allocation_rows) == 80
    with (HERE / "allocation-summary.csv").open("w") as output:
        writer = csv.DictWriter(output, fieldnames=list(allocation_rows[0]), lineterminator="\n")
        writer.writeheader()
        writer.writerows(allocation_rows)

    groups = collections.defaultdict(dict)
    for record in records("sensitivity.jsonl"):
        report = record["report"]
        assert report["valid"] and record["returncode"] == 0
        groups[report["backend"], record["variant"]][record["trial"]] = {
            "wall": report["elapsed_ns"] / report["measured_exchanges"],
            "cpu": report["process_cpu_ns"] / report["measured_exchanges"],
            "exchanges": report["measured_exchanges"],
        }
    sensitivity = []
    for (backend, variant), trials in sorted(groups.items()):
        assert sorted(trials) == list(range(5))
        wall = [trials[t]["wall"] for t in sorted(trials)]
        cpu = [trials[t]["cpu"] for t in sorted(trials)]
        baseline = groups[backend, "static-16k"]
        ratios = [trials[t]["wall"] / baseline[t]["wall"] for t in sorted(trials)]
        low, high = interval(ratios)
        sensitivity.append({
            "backend": backend, "variant": variant,
            "wall_ns_per_exchange_median": statistics.median(wall),
            "wall_ns_per_exchange_min": min(wall), "wall_ns_per_exchange_max": max(wall),
            "cpu_ns_per_exchange_median": statistics.median(cpu),
            "paired_ratio_to_static16k_median": statistics.median(ratios),
            "paired_ratio_bootstrap95_low": low, "paired_ratio_bootstrap95_high": high,
            "wall_trial_values": wall, "paired_ratios": ratios,
            "measured_exchanges_per_trial": [trials[t]["exchanges"] for t in sorted(trials)],
        })
    (HERE / "sensitivity-summary.json").write_text(json.dumps(sensitivity, indent=2) + "\n")
    for row in allocation_rows:
        if row["concurrency"] == 8:
            print("allocation", row)
    for row in sensitivity:
        print("sensitivity", row)


if __name__ == "__main__":
    main()
