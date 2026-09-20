"""Capped retained-allocation diagnostics. Never a timing benchmark."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import resource
import subprocess


def limits():
    resource.setrlimit(resource.RLIMIT_AS, (512 * 1024**2,) * 2)
    resource.setrlimit(resource.RLIMIT_CPU, (100, 105))
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))


parser = argparse.ArgumentParser()
parser.add_argument("--binary", type=Path, required=True)
parser.add_argument("--cohorts", type=int, choices=(1, 2, 8, 32), required=True)
parser.add_argument("--backend", choices=("native", "generic"), required=True)
parser.add_argument("--payload", choices=("static", "owned"), default="static")
parser.add_argument("--output-directory", type=Path, default=Path(__file__).resolve().parent)
args = parser.parse_args()
folder = args.output_directory.resolve()
folder.mkdir(parents=True, exist_ok=True)
stem = f"{args.backend}-{args.payload}-{args.cohorts}"
output = folder / (stem + ".jsonl")
assert not output.exists(), output
assert not output.with_suffix(".jsonl.gz").exists(), output
assert not (folder / (stem + "-run.json")).exists(), stem
warmup = int(args.cohorts > 1)
command = [
    "taskset", "-c", "8-31", str(args.binary.resolve()), "--backend", args.backend,
    "--case", "duplex", "--concurrency", "8", "--bytes", "1048576",
    "--chunk", "16384", "--payload", args.payload, "--phase", "steady",
    "--warmup", str(warmup), "--cohorts", str(args.cohorts - warmup),
    "--timeout-seconds", "90",
]
record = {
    "command": command, "binary_sha256": hashlib.sha256(args.binary.read_bytes()).hexdigest(),
    "address_space_limit_bytes": 512 * 1024**2, "cpu_limit_seconds": 100,
    "wall_limit_seconds": 110, "stack_sites_cap": 2048, "stack_depth": 24,
}
try:
    result = subprocess.run(
        command, env=dict(os.environ, KIMOJIO_RETENTION_OUTPUT=str(output)),
        capture_output=True, text=True, timeout=110, preexec_fn=limits,
    )
    record.update(returncode=result.returncode, stderr=result.stderr, stdout=result.stdout)
    assert result.returncode == 0, record
    report = json.loads(result.stdout)
    assert report["valid"]
    assert report["client_retirements"] == report["server_retirements"] == args.cohorts * 8
    assert report["early_response_witnesses_including_warmup"] == args.cohorts * 8
    assert report["drivers_closed_successfully"] == 2
    snapshots = [json.loads(line) for line in output.read_text().splitlines()]
    assert snapshots[-1]["label"] == "post_runtime_cleanup"
    for snapshot in snapshots:
        total = sum(c["live"] for c in snapshot["counts"])
        tracked = sum(s["live_bytes"] for s in snapshot["sites"])
        assert tracked <= total
        print(stem, snapshot["label"], snapshot["cohort"], total, tracked, len(snapshot["sites"]))
except Exception as error:
    record["failure"] = str(error)
    raise
finally:
    (folder / (stem + "-run.json")).write_text(json.dumps(record, indent=2) + "\n")
