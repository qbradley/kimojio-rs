"""Bounded debugger observations of actual strict socket benchmark retirements."""
import json
import hashlib
import os
from pathlib import Path
import resource
import subprocess

HERE = Path(__file__).resolve().parent
freeze = json.loads((HERE / "freeze.json").read_text())
OUT = HERE / "membership-distribution"
OUT.mkdir(exist_ok=True)


def limits():
    resource.setrlimit(resource.RLIMIT_AS, (1024 * 1024**2,) * 2)
    resource.setrlimit(resource.RLIMIT_CPU, (100, 105))
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))


for version, entry in (("baseline", freeze["baseline_binaries"]["runtime"]),
                       ("candidate", freeze["binaries"]["runtime"])):
    assert hashlib.sha256(Path(entry["path"]).read_bytes()).hexdigest() == entry["sha256"]
    symbols = subprocess.check_output(["nm", "--defined-only", entry["path"]], text=True)
    symbol = next(line.split()[-1] for line in symbols.splitlines() if "18retire_from_scopes" in line)
    for backend in ("native", "generic"):
        for case, cohorts in (("empty", 32), ("duplex", 1)):
            stem = version + "-" + backend + "-" + case
            result_path = OUT / (stem + ".json")
            assert not result_path.exists()
            env = dict(os.environ, MALLOC_ARENA_MAX="2", MEMBERSHIP_VERSION=version, MEMBERSHIP_SYMBOL=symbol,
                       MEMBERSHIP_OUTPUT=str(result_path))
            command = [
                "taskset", "-c", "8-31", "gdb", "-nx", "-batch", "--readnever",
                "-iex", "maint set worker-threads 1",
                "-iex", "set auto-load python-scripts off", "-iex", "set debuginfod enabled off",
                "-x", str(HERE / "membership_gdb.py"), "--args", entry["path"],
                "--backend", backend, "--case", case, "--bytes", "1048576",
                "--concurrency", "8", "--cohorts", str(cohorts), "--warmup", "0",
                "--timeout-seconds", "90",
            ]
            result = subprocess.run(command, env=env, text=True, capture_output=True, timeout=110, preexec_fn=limits)
            (OUT / (stem + "-log.txt")).write_text(result.stdout + result.stderr)
            (OUT / (stem + "-command.json")).write_text(json.dumps(
                {"command": command, "returncode": result.returncode, "binary": entry,
                 "address_space_limit": 1024**3, "wall_limit": 110, "cpu_limit": [100, 105]},
                indent=2) + "\n")
            assert result.returncode == 0, result.stderr
            outcomes = [json.loads(line) for line in result.stdout.splitlines()
                        if line.startswith('{"allocations"')]
            assert len(outcomes) == 1
            report = outcomes[0]
            assert report["valid"] and report["drivers_closed_successfully"] == 2
            assert report["client_retirements"] == report["server_retirements"] == cohorts * 8
            assert report["checked_payload_bytes_including_warmup"] == cohorts * 8 * (128 if case == "empty" else 2 * 1048576)
            if case == "duplex":
                assert report["early_response_witnesses_including_warmup"] == cohorts * 8
            counts = json.loads(result_path.read_text())
            assert counts["failure"] is None and counts["retirement_observations"] > 0
            print(stem, counts["counts_by_membership_length"], flush=True)
