"""Cheap, capped same-connection retention run after the duplex repair gate."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import resource
import subprocess

parser = argparse.ArgumentParser()
parser.add_argument("--binary", type=Path, required=True)
parser.add_argument("--backend", choices=("native", "generic"), required=True)
parser.add_argument("--cohorts", type=int, choices=(256, 1024, 4096), required=True)
parser.add_argument("--output-directory", type=Path, default=Path(__file__).resolve().parent)
args = parser.parse_args()
folder = args.output_directory.resolve()
folder.mkdir(parents=True, exist_ok=True)
stem = f"{args.backend}-empty-{args.cohorts}"
trace = folder / (stem + ".jsonl")
record_path = folder / (stem + "-run.json")
assert not trace.exists() and not Path(str(trace) + ".gz").exists() and not record_path.exists()


def limits():
    resource.setrlimit(resource.RLIMIT_AS, (512 * 1024**2,) * 2)
    resource.setrlimit(resource.RLIMIT_CPU, (100, 105))
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))


command = [
    "taskset", "-c", "8-31", str(args.binary.resolve()), "--backend", args.backend,
    "--case", "empty", "--concurrency", "8", "--phase", "steady",
    "--warmup", "1", "--cohorts", str(args.cohorts - 1), "--payload", "static",
    "--timeout-seconds", "90",
]
record = {
    "command": command, "binary_sha256": hashlib.sha256(args.binary.read_bytes()).hexdigest(),
    "address_space_limit_bytes": 512 * 1024**2, "cpu_limit_seconds": 100,
    "wall_limit_seconds": 110, "stack_sites_cap": 2048, "stack_depth": 24,
}
try:
    result = subprocess.run(
        command, env=dict(os.environ, KIMOJIO_RETENTION_OUTPUT=str(trace)),
        capture_output=True, text=True, timeout=110, preexec_fn=limits,
    )
    record.update(returncode=result.returncode, stdout=result.stdout, stderr=result.stderr)
    assert result.returncode == 0, record
    report = json.loads(result.stdout)
    assert report["valid"] and report["case"] == "empty"
    assert report["client_retirements"] == report["server_retirements"] == args.cohorts * 8
    assert report["checked_payload_bytes_including_warmup"] == args.cohorts * 8 * 128
    assert report["drivers_closed_successfully"] == 2
    for line in trace.read_text().splitlines():
        snapshot = json.loads(line)
        print(args.backend, snapshot["label"], snapshot["cohort"],
              sum(c["live"] for c in snapshot["counts"]),
              sum(s["live_bytes"] for s in snapshot["sites"]), len(snapshot["sites"]))
except Exception as error:
    record["failure"] = str(error)
    raise
finally:
    record_path.write_text(json.dumps(record, indent=2) + "\n")
