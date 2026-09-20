"""Reuse immutable experiment drivers; only data paths and binary identities change."""
import argparse
import importlib.util
import json
import math
import os
from pathlib import Path
import resource
import statistics
import subprocess
import sys

HERE = Path(__file__).resolve().parent
OLD = Path("/workspace/kimojio-rs/target/http2-program/worktrees/http2-wrapper-performance/docs/http2-performance/wrapper/direct-read-458b2bcd")
REVISION = "2a65d8e4a690975564104f3a78c697448eaee8e0"
FREEZE = json.loads((HERE / "freeze.json").read_text())


def load(name):
    path = OLD / name
    recorded = subprocess.check_output(["git", "show", REVISION + ":docs/http2-performance/wrapper/direct-read-458b2bcd/" + name])
    assert path.read_bytes() == recorded
    spec = importlib.util.spec_from_file_location("prior_" + name.replace(".", "_"), path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    module.HERE = HERE
    return module


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("pilot", "matrix", "sensitivity", "stats", "retention", "controls"))
    args = parser.parse_args()
    if args.action in ("pilot", "matrix", "sensitivity"):
        runner = load("paired.py")
        runner.PREVIOUS = HERE.parent / "measured"
        runner.BINARIES = {
            "baseline": FREEZE["baseline_binaries"]["runtime"]["path"],
            "candidate": FREEZE["binaries"]["runtime"]["path"],
        }
        runner.main()
    elif args.action == "stats":
        summary = load("summarize.py")
        totals = [summary.summarize(name) for name in ("matrix", "sensitivity")]
        groups = json.loads((HERE / "aggregate.json").read_text())
        groups = [g for g in groups if g["backend"] in ("native", "generic")]
        for category in ("small", "large", "all"):
            rows = [g for g in groups if g["category"] == category]
            values = [math.sqrt(a * b) for a, b in zip(rows[0]["paired_trial_geomeans"], rows[1]["paired_trial_geomeans"])]
            low, high = summary.interval(values)
            groups.append({"category": category, "backend": "both_affected_backends",
                           "paired_trial_geomeans": values, "median": statistics.median(values),
                           "bootstrap95_low": low, "bootstrap95_high": high})
        (HERE / "aggregate.json").write_text(json.dumps(groups, indent=2) + "\n")
        (HERE / "totals.json").write_text(json.dumps(totals, indent=2) + "\n")
        print(json.dumps(groups, indent=2))
    elif args.action == "retention":
        for version, binaries in (("baseline", FREEZE["baseline_binaries"]), ("candidate", FREEZE["binaries"])):
            folder = HERE / "retention" / version
            for backend in ("native", "generic"):
                for payload, cohorts in (("static", 32), ("owned", 8)):
                    subprocess.run([
                        sys.executable, str(HERE.parent / "retention/run.py"),
                        "--binary", binaries["allocations"]["path"], "--backend", backend,
                        "--payload", payload, "--cohorts", str(cohorts), "--output-directory", str(folder),
                    ], check=True)
                subprocess.run([
                    sys.executable, str(HERE.parent / "retention-candidate-a4af94fd/empty.py"),
                    "--binary", binaries["allocations"]["path"], "--backend", backend,
                    "--cohorts", "4096", "--output-directory", str(folder),
                ], check=True)
    else:
        def limits():
            resource.setrlimit(resource.RLIMIT_AS, (512 * 1024**2,) * 2)
            resource.setrlimit(resource.RLIMIT_CPU, (100, 105))
            resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
        for version, binaries in (("baseline", FREEZE["baseline_binaries"]), ("candidate", FREEZE["binaries"])):
            folder = HERE / "retention" / version
            folder.mkdir(parents=True, exist_ok=True)
            for control in ("wait-scoped", "wait-unscoped", "nop-scoped", "nop-unscoped"):
                trace = folder / (control + ".jsonl")
                assert not trace.exists() and not Path(str(trace) + ".gz").exists()
                command = ["taskset", "-c", "8-31", binaries["allocations"]["path"]]
                env = dict(os.environ, KIMOJIO_RETENTION_CONTROL=control, KIMOJIO_RETENTION_OUTPUT=str(trace))
                result = subprocess.run(command, env=env, capture_output=True, text=True,
                                        timeout=110, preexec_fn=limits)
                record = {"command": command, "control": control, "returncode": result.returncode,
                          "stdout": result.stdout, "stderr": result.stderr, "timing_evidence": False,
                          "address_space_limit": 512 * 1024**2, "cpu_limit": [100, 105], "wall_limit": 110}
                (folder / (control + "-run.json")).write_text(json.dumps(record, indent=2) + "\n")
                assert result.returncode == 0, record


if __name__ == "__main__":
    main()
