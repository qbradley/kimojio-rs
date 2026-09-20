"""Exclusive-CPU2 wrapper qualification. Normal allocator, strict workload oracles."""
import argparse
import json
import math
import os
from pathlib import Path
import resource
import subprocess

HERE = Path(__file__).resolve().parent
BINARY = Path("/workspace/kimojio-rs/target/http2-program/build-http2-wrapper-performance/measured-6e003746/runtime")
CASES = [("empty", "empty", 1048576), ("fixed4k", "fixed", 1048576),
         ("stream32k", "stream", 32768), ("stream1m", "stream", 1048576),
         ("duplex1m", "duplex", 1048576)]


def limits():
    resource.setrlimit(resource.RLIMIT_AS, (1024 * 1024**2,) * 2)
    # The recorded host control shows quantized process CPU clocks with RLIMIT_CPU.
    # The independent wall watchdog still bounds each single-threaded process.
    assert resource.getrlimit(resource.RLIMIT_CPU)[0] == resource.RLIM_INFINITY
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))


def execute(backend, case, size, concurrency, phase, cohorts, warmup, output, **extra):
    command = [
        "taskset", "-c", "2", str(BINARY), "--backend", backend, "--case", case,
        "--bytes", str(size), "--concurrency", str(concurrency), "--phase", phase,
        "--cohorts", str(cohorts), "--warmup", str(warmup), "--timeout-seconds", "90",
        "--payload", extra.get("payload", "static"), "--chunk", str(extra.get("chunk", 16384)),
    ]
    record = {"command": command, **extra}
    try:
        environment = dict(os.environ)
        environment.pop("KIMOJIO_RETENTION_OUTPUT", None)
        environment.pop("KIMOJIO_RETENTION_CONTROL", None)
        result = subprocess.run(command, env=environment, capture_output=True, text=True,
                                timeout=110, preexec_fn=limits)
        record.update(returncode=result.returncode, stderr=result.stderr)
        report = json.loads(result.stdout)
        record["report"] = report
        assert result.returncode == 0 and report["valid"], record
        count = (cohorts + (warmup if phase == "steady" else 0)) * concurrency
        assert report["client_retirements"] == report["server_retirements"] == count
        assert report["drivers_closed_successfully"] == 2
        assert report["connections_created"] == 1 and report["reconnects"] == 0
        expected = (0, 128) if case == "empty" else ((4096, 4096) if case == "fixed" else (size, size))
        assert (report["request_bytes"], report["response_bytes"]) == expected
        assert report["checked_payload_bytes_including_warmup"] == sum(expected) * count
        assert report["measured_exchanges"] == cohorts * concurrency
        assert report["allocations"] is None
        if case == "duplex":
            assert report["early_response_witnesses_including_warmup"] == count
    except Exception as error:
        record["failure"] = str(error)
        output.write(json.dumps(record) + "\n")
        output.flush()
        raise
    output.write(json.dumps(record) + "\n")
    output.flush()
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("pilot", "matrix", "sensitivity"))
    args = parser.parse_args()
    if args.action == "pilot":
        plan = []
        with (HERE / "pilot.jsonl").open("x") as output:
            for label, case, size in CASES:
                for concurrency in (1, 8, 32, 128):
                    warmup = max(32, math.ceil(2048 / concurrency)) if size == 32768 or case in ("empty", "fixed") else 8
                    measurements = {}
                    for backend in ("native", "generic"):
                        for phase in ("cold", "steady"):
                            report = execute(backend, case, size, concurrency, phase,
                                             1 if phase == "cold" else 4,
                                             0 if phase == "cold" else warmup, output,
                                             label=label, pilot=True)
                            measurements[backend, phase] = report["elapsed_ns"] / (1 if phase == "cold" else 4)
                    cohorts = max(2, math.ceil(250_000_000 / min(measurements[b, "steady"] for b in ("native", "generic"))))
                    repeats = min(31, max(1, math.ceil(10_000_000 / min(measurements[b, "cold"] for b in ("native", "generic")))))
                    plan.append({"label": label, "case": case, "bytes": size, "concurrency": concurrency,
                                 "cohorts": cohorts, "warmup": warmup, "cold_repeats": repeats})
                    print(json.dumps(plan[-1]), flush=True)
        (HERE / "plan.json").write_text(json.dumps(plan, indent=2) + "\n")
    elif args.action == "matrix":
        plan = json.loads((HERE / "plan.json").read_text())
        with (HERE / "matrix.jsonl").open("x") as output:
            for trial in range(5):
                for cell in plan if trial % 2 == 0 else list(reversed(plan)):
                    for phase in ("cold", "steady"):
                        order = ("native", "generic") if trial % 2 == 0 else ("generic", "native")
                        for backend in order:
                            for repeat in range(cell["cold_repeats"] if phase == "cold" else 1):
                                execute(backend, cell["case"], cell["bytes"], cell["concurrency"], phase,
                                        1 if phase == "cold" else cell["cohorts"],
                                        0 if phase == "cold" else cell["warmup"], output,
                                        label=cell["label"], trial=trial, repeat=repeat)
                print(f"completed trial {trial + 1}/5", flush=True)
    else:
        plan = json.loads((HERE / "plan.json").read_text())
        cells = [c for c in plan if c["label"] == "duplex1m" and c["concurrency"] == 8]
        with (HERE / "sensitivity.jsonl").open("x") as output:
            for trial in range(5):
                for cell in cells:
                    for variant, chunk, payload in (("static-16k", 16384, "static"),
                                                     ("owned-16k", 16384, "owned"),
                                                     ("static-1k", 1024, "static")):
                        for backend in (("native", "generic") if trial % 2 == 0 else ("generic", "native")):
                            execute(backend, cell["case"], cell["bytes"], cell["concurrency"],
                                    "steady", max(2, math.ceil(cell["cohorts"] / 8)) if chunk == 1024 else cell["cohorts"],
                                    cell["warmup"], output,
                                    label=cell["label"], trial=trial, variant=variant,
                                    chunk=chunk, payload=payload)
                print(f"completed sensitivity trial {trial + 1}/5", flush=True)


if __name__ == "__main__":
    main()
