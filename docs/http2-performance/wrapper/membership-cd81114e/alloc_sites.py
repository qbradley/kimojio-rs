"""Resolve allocation stacks against each frozen allocation executable."""
import json
import gzip
from pathlib import Path
import subprocess

HERE = Path(__file__).resolve().parent
freeze = json.loads((HERE / "freeze.json").read_text())
summary = []


def read_trace(path):
    if path.suffix == ".gz":
        with gzip.open(path, "rt") as stream:
            return stream.read()
    return path.read_text()


for version, binaries in (("baseline", freeze["baseline_binaries"]), ("candidate", freeze["binaries"])):
    folder = HERE / "retention" / version
    traces = {path.name.removesuffix(".gz").removesuffix(".jsonl"):
              [json.loads(line) for line in read_trace(path).splitlines()]
              for path in folder.glob("*.jsonl*")}
    offsets = sorted({pc - r["executable_base"] - 1 for rows in traces.values() for r in rows
                      for site in r["sites"] for pc in site["stack"]
                      if 0 < pc - r["executable_base"] < 64 * 1024**2})
    symbols = {}
    lines = subprocess.check_output(
        ["addr2line", "-a", "-f", "-C", "-i", "-e", binaries["allocations"]["path"]]
        + [hex(offset) for offset in offsets], text=True).splitlines()
    for line in lines:
        if line.startswith("0x"):
            current = int(line, 16)
            symbols[current] = []
        else:
            symbols[current].append(line)
    site_rows = []
    for case, rows in sorted(traces.items()):
        for snapshot in rows:
            groups = {}
            for index, site in enumerate(snapshot["sites"]):
                frames = []
                for pc in site["stack"]:
                    offset = pc - snapshot["executable_base"] - 1
                    values = symbols.get(offset, [])
                    frames.extend({"offset": hex(offset), "name": values[i], "source": values[i + 1]}
                                  for i in range(0, len(values) - 1, 2))
                names = [frame["name"] for frame in frames]
                if any(("Vec<alloc::rc::Weak<kimojio::task::io_scope::IoScopeRegistry" in name
                        or "WaitScopes>::push" in name) for name in names):
                    category = "reverse_membership_vector"
                elif any(name.lstrip("<").startswith("alloc::rc::Rc<kimojio::async_event::WaitData>")
                         and "::new" in name for name in names):
                    category = "wait_data"
                else:
                    category = "other"
                group = groups.setdefault(category, {key: 0 for key in
                    ("allocations", "reallocations", "deallocations", "live_allocations", "live_bytes")})
                for key in group:
                    group[key] += site[key]
                if snapshot["label"] == "connection_live" and snapshot["cohort"] in (32, 4096):
                    site_rows.append({"case": case, "cohort": snapshot["cohort"], "site": index,
                                      "category": category, **site, "frames": frames})
            totals = {key: sum(c[key] for c in snapshot["counts"])
                      for key in ("allocations", "reallocations", "deallocations", "live")}
            tracked = sum(s["live_bytes"] for s in snapshot["sites"])
            assert tracked <= totals["live"]
            summary.append({"version": version, "case": case, "label": snapshot["label"],
                            "cohort": snapshot["cohort"], **totals, "tracked_live": tracked,
                            "unattributed_live": totals["live"] - tracked, "groups": groups})
    (HERE / (version + "-allocation-sites.json")).write_text(json.dumps(site_rows, indent=2) + "\n")
(HERE / "allocation-site-summary.json").write_text(json.dumps(summary, indent=2) + "\n")
for row in summary:
    if row["label"] == "connection_live" and row["cohort"] in (32, 4096):
        print(row["version"], row["case"], row["cohort"], row["allocations"], row["live"],
              {key: value for key, value in row["groups"].items() if key != "other"})
