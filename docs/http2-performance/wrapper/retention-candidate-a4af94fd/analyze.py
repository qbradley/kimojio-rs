"""Attribute candidate allocations to their real, frozen allocation stacks."""
import argparse
import gzip
import json
from pathlib import Path
import subprocess

parser = argparse.ArgumentParser()
parser.add_argument("--binary", required=True)
parser.add_argument("--case", choices=("static-32", "empty-4096"), required=True)
args = parser.parse_args()
folder = Path(__file__).resolve().parent


def read_trace(path):
    if path.exists():
        return path.read_text()
    with gzip.open(str(path) + ".gz", "rt") as stream:
        return stream.read()


series = {
    backend: [json.loads(line) for line in read_trace(folder / f"{backend}-{args.case}.jsonl").splitlines()]
    for backend in ("native", "generic")
}
offsets = sorted({
    pc - snapshot["executable_base"] - 1
    for snapshots in series.values() for snapshot in snapshots
    for site in snapshot["sites"] for pc in site["stack"]
    if 0 < pc - snapshot["executable_base"] < 64 * 1024**2
})
symbols = {}
for line in subprocess.check_output(
    ["addr2line", "-a", "-f", "-C", "-i", "-e", args.binary] + [hex(pc) for pc in offsets],
    text=True,
).splitlines():
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
        frames.extend((hex(offset), values[i], values[i + 1]) for i in range(0, len(values) - 1, 2))
    # Nearest production source, not an arbitrary shared ancestor.
    owner = "other"
    for _, _, path in frames:
        if "/http2-wrapper-performance/" not in path:
            continue
        path = path.split("/http2-wrapper-performance/")[1]
        if path.startswith((
            "kimojio-http2/benches/support/allocation",
            "kimojio-http2/benches/allocations.rs",
            "kimojio-http2/benches/support/retention.rs",
        )):
            continue
        owner = (
            "core/protocol" if path.startswith("kimojio-fsm-http2/")
            else "wrapper" if path.startswith("kimojio-http2/src/")
            else "harness" if path.startswith("kimojio-http2/benches/")
            else "runtime" if path.startswith("kimojio/")
            else "other"
        )
        break
    return owner, frames


rows = []
site_rows = []
for backend, snapshots in series.items():
    for snapshot in snapshots:
        groups = {}
        for index, site in enumerate(snapshot["sites"]):
            owner, frames = describe(snapshot, site)
            group = groups.setdefault(owner, {"live_bytes": 0, "live_objects": 0, "allocations": 0,
                                               "reallocations": 0, "deallocations": 0})
            for key in ("live_bytes", "allocations", "reallocations", "deallocations"):
                group[key] += site[key]
            group["live_objects"] += site["live_allocations"]
            if snapshot["label"] == "connection_live" and snapshot["cohort"] == int(args.case.split("-")[-1]):
                site_rows.append({
                    "backend": backend, "site": index, "owner": owner, **site, "frames": frames
                })
        counts = snapshot["counts"]
        rows.append({
            "backend": backend, "label": snapshot["label"], "cohort_field": snapshot["cohort"],
            **{key: sum(c[key] for c in counts) for key in ("allocations", "reallocations", "deallocations", "live")},
            "groups": groups,
        })
(folder / (args.case + "-ownership.json")).write_text(json.dumps(rows, indent=2) + "\n")
(folder / (args.case + "-sites.json")).write_text(json.dumps(site_rows, indent=2) + "\n")
for row in rows:
    if row["label"] == "connection_live":
        print(row["backend"], row["cohort_field"],
              {owner: group["live_bytes"] for owner, group in row["groups"].items()})
