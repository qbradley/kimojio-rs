import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess

parser = argparse.ArgumentParser()
parser.add_argument("--timers", action="store_true")
parser.add_argument("--root", type=Path, default=Path("target/http1-core-study"))
args = parser.parse_args()
ROOT = args.root.resolve()
EVIDENCE = ROOT / "evidence"
prefix = "timer-" if args.timers else ""
if (EVIDENCE / f"{prefix}timings.jsonl").exists():
    parser.error("timing evidence already exists; use a fresh study directory")
VERSIONS = [prefix + version for version in ("base", "role", "metadata")]
manifest = {}
for version in VERSIONS:
    records = [
        json.loads(line)
        for line in (EVIDENCE / f"{version}-build.jsonl").read_text().splitlines()
    ]
    binaries = [
        record["executable"]
        for record in records
        if record.get("reason") == "compiler-artifact"
        and record["target"]["name"] == "roundtrip"
        and record.get("executable")
    ]
    assert len(binaries) == 1, binaries
    frozen = ROOT / f"frozen-{version}" / "roundtrip"
    frozen.parent.mkdir(exist_ok=True)
    if not frozen.exists():
        shutil.copy2(binaries[0], frozen)
    assert frozen.read_bytes() == Path(binaries[0]).read_bytes()
    manifest[version] = {
        "binary": str(frozen),
        "sha256": hashlib.sha256(frozen.read_bytes()).hexdigest(),
        "source": subprocess.check_output(
            ["git", "-C", str(ROOT / version), "rev-parse", "HEAD"], text=True
        ).strip(),
        "build": "CARGO_PROFILE_BENCH_DEBUG=1 cargo bench -p kimojio-fsm-http1 "
        "--bench roundtrip --no-run",
        "size": subprocess.check_output(["size", str(frozen)], text=True),
    }
(EVIDENCE / f"{prefix}manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")

pattern = re.compile(
    r"(http1/\S+)\s+time:\s+\[([\d.]+) (ns|µs|us|ms) "
    r"([\d.]+) (ns|µs|us|ms) ([\d.]+) (ns|µs|us|ms)\]"
)
scale = {"ns": 1, "µs": 1000, "us": 1000, "ms": 1_000_000}
with (EVIDENCE / f"{prefix}timings.jsonl").open("w") as results:
    for group in range(7):
        order = VERSIONS[group % 3:] + VERSIONS[:group % 3]
        if group % 2:
            order = list(reversed(order))
        for version in order:
            command = [
                "taskset", "-c", "2", manifest[version]["binary"], "--bench",
                "--warm-up-time", "0.1", "--measurement-time", "0.5",
                "--sample-size", "30", "--nresamples", "10000", "--noplot",
                "--save-baseline", f"{version}-{group}",
            ]
            run = subprocess.run(command, text=True, capture_output=True, timeout=180)
            (EVIDENCE / f"timing-{group}-{version}.txt").write_text(
                run.stdout + run.stderr
            )
            if run.returncode:
                raise RuntimeError((version, group, run.returncode, run.stderr))
            rows = []
            for match in pattern.finditer(run.stdout):
                cell, low, low_unit, point, unit, high, high_unit = match.groups()
                rows.append({
                    "group": group, "version": version, "cell": cell,
                    "low_ns": float(low) * scale[low_unit],
                    "ns": float(point) * scale[unit],
                    "high_ns": float(high) * scale[high_unit],
                })
            assert len(rows) == 8, (version, group, run.stdout)
            for row in rows:
                results.write(json.dumps(row) + "\n")
            results.flush()
            print(f"group {group + 1}/7: {version} passed", flush=True)
for version, entry in manifest.items():
    assert hashlib.sha256(Path(entry["binary"]).read_bytes()).hexdigest() == entry["sha256"]
