"""Reconcile continuous allocation ledgers without adding non-simultaneous peaks."""
import csv
import json
from pathlib import Path

from summarize import read_text

HERE = Path(__file__).resolve().parent


def main():
    rows, comparisons = [], []
    names = sorted({f.name.removesuffix(".gz")
                    for f in (HERE / "retention/baseline").glob("*.jsonl*")})
    for name in names:
        left = [json.loads(l) for l in read_text(HERE / "retention/baseline" / name).splitlines()]
        right = [json.loads(l) for l in read_text(HERE / "retention/candidate" / name).splitlines()]
        assert len(left) == len(right)
        case = name.removesuffix(".jsonl")
        for baseline, candidate in zip(left, right):
            assert (baseline["label"], baseline["cohort"]) == (candidate["label"], candidate["cohort"])
            comparisons.append({
                "case": case, "label": baseline["label"], "cohort": baseline["cohort"],
                "all_origin_counters_equal": baseline["counts"] == candidate["counts"],
            })
            for version, snapshot in (("baseline", baseline), ("candidate", candidate)):
                row = {"version": version, "case": case, "label": snapshot["label"],
                       "cohort": snapshot["cohort"]}
                row.update({k: sum(c[k] for c in snapshot["counts"])
                            for k in ("allocations", "reallocations", "deallocations", "live")})
                row["tracked_site_live"] = sum(s["live_bytes"] for s in snapshot["sites"])
                row["unattributed_live"] = row["live"] - row["tracked_site_live"]
                assert row["unattributed_live"] >= 0
                for index, origin in enumerate(snapshot["counts"]):
                    row[f"origin_{index}_live"] = origin["live"]
                rows.append(row)
    with (HERE / "retention-summary.csv").open("w") as output:
        writer = csv.DictWriter(output, fieldnames=list(rows[0]), lineterminator="\n")
        writer.writeheader()
        writer.writerows(rows)
    (HERE / "retention-comparison.json").write_text(json.dumps(comparisons, indent=2) + "\n")


if __name__ == "__main__":
    main()
