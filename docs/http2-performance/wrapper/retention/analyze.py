"""Resolve allocation return addresses against the retained diagnostic executable."""
import csv
import gzip
import json
from pathlib import Path
import subprocess

folder = Path(__file__).resolve().parent
binary = "/workspace/kimojio-rs/target/http2-program/build-http2-wrapper-performance/retention-frozen/allocations"


def read_trace(path):
    if path.exists():
        return path.read_text()
    with gzip.open(str(path) + ".gz", "rt") as stream:
        return stream.read()


snapshots = {
    backend: [json.loads(line) for line in read_trace(folder / f"{backend}-static-32.jsonl").splitlines()]
    for backend in ("native", "generic")
}
offsets = sorted({
    pc - snapshot["executable_base"] - 1
    for series in snapshots.values() for snapshot in series
    for site in snapshot["sites"] for pc in site["stack"]
    if 0 < pc - snapshot["executable_base"] < 64 * 1024**2
})
text = subprocess.check_output(
    ["addr2line", "-a", "-f", "-C", "-i", "-e", binary] + [hex(pc) for pc in offsets], text=True
)
symbols = {}
current = None
for line in text.splitlines():
    if line.startswith("0x"):
        current = int(line, 16)
        symbols[current] = []
    else:
        symbols[current].append(line)


def describe(snapshot, site):
    frames = []
    for pc in site["stack"]:
        offset = pc - snapshot["executable_base"] - 1
        values = symbols.get(offset, [])
        frames.extend(
            (hex(offset), values[i], values[i + 1])
            for i in range(0, len(values) - 1, 2)
        )
    names = "\n".join(frame[1] for frame in frames)
    if "TaskState>::new_completion" in names:
        group = "runtime Completion Rc"
    elif "Task>::register_wait" in names:
        group = "runtime scope waits Vec"
    elif "Task>::register_io" in names:
        group = "runtime scope completions Vec"
    elif "Rc<kimojio::async_event::WaitData" in names:
        group = "runtime WaitData Rc"
    elif "TaskState>::return_completion" in names:
        group = "runtime completion pool Vec"
    else:
        group = "other"
        for _, _, path in frames:
            if "/http2-wrapper-performance/" not in path:
                continue
            path = path.split("/http2-wrapper-performance/")[1]
            if path.startswith("kimojio-http2/benches/support/allocation") or path.startswith(
                ("kimojio-http2/benches/allocations.rs", "kimojio-http2/benches/support/retention.rs")
            ):
                continue
            group = (
                "core/protocol" if path.startswith("kimojio-fsm-http2/")
                else "wrapper" if path.startswith("kimojio-http2/src/")
                else "harness" if path.startswith("kimojio-http2/benches/")
                else "other runtime" if path.startswith("kimojio/")
                else "other"
            )
            break
    return group, frames


attribution = []
rows = []
for backend, series in snapshots.items():
    for snapshot in series:
        groups = {}
        for index, site in enumerate(snapshot["sites"]):
            if not site["live_bytes"]:
                continue
            group, frames = describe(snapshot, site)
            row = groups.setdefault(group, {"bytes": 0, "objects": 0})
            row["bytes"] += site["live_bytes"]
            row["objects"] += site["live_allocations"]
            if snapshot["label"] == "connection_live" and snapshot["cohort"] == 32:
                attribution.append({
                    "backend": backend, "site": index, "group": group,
                    "live_bytes": site["live_bytes"], "live_objects": site["live_allocations"],
                    "frames": frames,
                })
        counts = snapshot["counts"]
        rows.append({
            "backend": backend, "label": snapshot["label"], "cohort_field": snapshot["cohort"],
            "allocations": sum(c["allocations"] for c in counts),
            "reallocations": sum(c["reallocations"] for c in counts),
            "deallocations": sum(c["deallocations"] for c in counts),
            "requested_live_bytes": sum(c["live"] for c in counts),
            "attributed_live_bytes": sum(g["bytes"] for g in groups.values()),
            "groups": groups,
        })
(folder / "ownership.json").write_text(json.dumps(rows, indent=2) + "\n")
(folder / "site-symbols.json").write_text(json.dumps(attribution, indent=2) + "\n")
with (folder / "cohort-counts.csv").open("w") as output:
    fields = [key for key in rows[0] if key != "groups"]
    writer = csv.DictWriter(output, fieldnames=fields)
    writer.writeheader()
    writer.writerows({key: row[key] for key in fields} for row in rows)
for row in rows:
    if row["label"] == "connection_live" and row["cohort_field"] == 32:
        print(row["backend"], json.dumps(row["groups"], sort_keys=True))
