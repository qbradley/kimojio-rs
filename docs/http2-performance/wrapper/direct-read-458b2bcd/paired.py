"""Bounded paired experiment. Existing workload and all outcome assertions stay intact."""
import argparse
import importlib.util
import json
import math
from pathlib import Path

HERE = Path(__file__).resolve().parent
PREVIOUS = HERE.parent / "measured"
spec = importlib.util.spec_from_file_location("baseline_runner", PREVIOUS / "run.py")
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)
freeze = json.loads((HERE / "freeze.json").read_text())
BINARIES = {"baseline": freeze["baseline_binaries"]["runtime"]["path"],
            "candidate": freeze["binaries"]["runtime"]["path"]}


def execute(version, backend, cell, output, **extra):
    runner.BINARY = Path(BINARIES[version])
    return runner.execute(
        backend, cell["case"], cell["bytes"], cell["concurrency"], cell["phase"],
        cell["cohorts"], cell["warmup"], output, version=version,
        label=cell["label"], variant=cell.get("variant", "static16k"),
        chunk=cell.get("chunk", 16384), payload=cell.get("payload", "static"), **extra,
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("pilot", "matrix", "sensitivity"))
    args = parser.parse_args()
    destination = HERE / (args.action + ".jsonl")
    assert not destination.exists() and not Path(str(destination) + ".gz").exists()
    if args.action == "pilot":
        assert not (HERE / "plan.json").exists()
        old = json.loads((PREVIOUS / "plan.json").read_text())
        plan = []
        with (HERE / "pilot.jsonl").open("x") as output:
            for previous in old:
                if previous["concurrency"] not in (1, 8, 128) or previous["label"] == "stream32k":
                    continue
                cell = dict(previous, phase="steady")
                times = []
                for backend in ("native", "generic"):
                    for version in ("baseline", "candidate"):
                        report = execute(version, backend, cell, output, pilot=True)
                        times.append(report["elapsed_ns"])
                cell["cohorts"] = max(cell["cohorts"], math.ceil(cell["cohorts"] * 1_000_000_000 / min(times)))
                plan.append(cell)
                if cell["case"] in ("empty", "fixed"):
                    plan.append(dict(cell, phase="cold", cohorts=1, warmup=0))
        (HERE / "plan.json").write_text(json.dumps(plan, indent=2) + "\n")
    else:
        plan = json.loads((HERE / "plan.json").read_text())
        if args.action == "sensitivity":
            cell = next(c for c in plan if c["label"] == "duplex1m" and c["concurrency"] == 8)
            plan = [dict(cell, variant="static16k"),
                    dict(cell, variant="owned16k", payload="owned"),
                    dict(cell, variant="static1k", chunk=1024, cohorts=max(2, math.ceil(cell["cohorts"] / 12)))]
        with (HERE / (args.action + ".jsonl")).open("x") as output:
            for trial in range(9):
                for cell in plan if trial % 2 == 0 else list(reversed(plan)):
                    for backend in (("native", "generic") if trial % 2 == 0 else ("generic", "native")):
                        versions = ("baseline", "candidate") if trial % 2 == 0 else ("candidate", "baseline")
                        # Interleave cold processes instead of placing an entire version group first.
                        for repeat in range(cell["cold_repeats"] if cell["phase"] == "cold" else 1):
                            for version in versions:
                                execute(version, backend, cell, output, trial=trial, repeat=repeat)
                print(args.action, trial + 1, "/ 9", flush=True)


if __name__ == "__main__":
    main()
