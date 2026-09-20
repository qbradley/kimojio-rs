"""Record separate user CPU-clock profiles of the immutable normal-allocator binary."""
import hashlib
import argparse
import json
import os
from pathlib import Path
import subprocess

here = Path(__file__).resolve().parent
root = Path("/workspace/kimojio-rs/target/http2-program/build-http2-wrapper-performance/measured-6e003746")
parser = argparse.ArgumentParser()
parser.add_argument("--frame-pointer", action="store_true")
parser.add_argument("--stack", type=int, choices=(32768, 65528), default=32768)
parser.add_argument("--backend", choices=("native", "generic"))
parser.add_argument("--case", choices=("empty", "duplex1m"))
parser.add_argument("--mmap-pages", type=int)
parser.add_argument("--frequency", type=int, default=999)
args = parser.parse_args()
binary = root / ("runtime-frame-pointer" if args.frame_pointer else "runtime")
folder = root / "profiles"
folder.mkdir(exist_ok=True)
plan = json.loads((here / "plan.json").read_text())
records = []
for label in ("empty", "duplex1m"):
    if args.case and label != args.case:
        continue
    cell = next(c for c in plan if c["label"] == label and c["concurrency"] == 8)
    for backend in ("native", "generic"):
        if args.backend and backend != args.backend:
            continue
        suffix = ("-fp" if args.frame_pointer else "") + ("64" if args.stack == 65528 else "")
        if args.mmap_pages:
            suffix += "-buffered"
        if args.frequency != 999:
            suffix += "-f" + str(args.frequency)
        name = backend + "-" + label + suffix
        data = folder / (name + ".data")
        assert not data.exists()
        command = [
            "perf", "record", "-N", "-e", "cpu-clock:u", "-F", str(args.frequency),
            *(["-m", str(args.mmap_pages)] if args.mmap_pages else []),
            "--call-graph", f"dwarf,{args.stack}", "-o", str(data), "--",
            "taskset", "-c", "2", str(binary), "--backend", backend,
            "--case", cell["case"], "--bytes", str(cell["bytes"]),
            "--concurrency", "8", "--warmup", str(cell["warmup"]),
            "--cohorts", str(cell["cohorts"] * 20), "--timeout-seconds", "90",
        ]
        result = subprocess.run(command, capture_output=True, text=True, timeout=120,
                                env=dict(os.environ, DEBUGINFOD_URLS=""))
        record = {
            "name": name, "command": command, "returncode": result.returncode,
            "stdout": result.stdout, "stderr": result.stderr,
            "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        }
        if data.exists():
            record.update(data_sha256=hashlib.sha256(data.read_bytes()).hexdigest(),
                          data_bytes=data.stat().st_size)
        records.append(record)
        (here / ("profiles" + suffix + ".json")).write_text(json.dumps(records, indent=2) + "\n")
        assert result.returncode == 0, record
        report = json.loads(result.stdout)
        assert report["valid"] and report["drivers_closed_successfully"] == 2
        assert report["client_retirements"] == report["server_retirements"]
        if label == "duplex1m":
            assert report["early_response_witnesses_including_warmup"] == report["client_retirements"]
        print(name, record["data_bytes"], result.stderr.strip(), flush=True)
