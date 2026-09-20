"""Separate allocation counts on the timed source. CPU values are not timing evidence."""
import json
import os
from pathlib import Path
import resource
import subprocess

here = Path(__file__).resolve().parent
binary = "/workspace/kimojio-rs/target/http2-program/build-http2-wrapper-performance/measured-6e003746/allocations"
plan = json.loads((here / "plan.json").read_text())


def limits():
    resource.setrlimit(resource.RLIMIT_AS, (512 * 1024**2,) * 2)
    resource.setrlimit(resource.RLIMIT_CPU, (100, 105))
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))


with (here / "allocations.jsonl").open("x") as output:
    for cell in plan:
        for phase in ("cold", "steady"):
            for backend in ("native", "generic"):
                cohorts = 1 if phase == "cold" else (2 if cell["label"].endswith("1m") else 4)
                warmup = 0 if phase == "cold" else cell["warmup"]
                command = [
                    "taskset", "-c", "8-31", binary, "--backend", backend, "--case", cell["case"],
                    "--bytes", str(cell["bytes"]), "--concurrency", str(cell["concurrency"]),
                    "--phase", phase, "--cohorts", str(cohorts), "--warmup", str(warmup),
                    "--timeout-seconds", "90",
                ]
                record = {"command": command, "label": cell["label"], "timing_evidence": False}
                try:
                    environment = dict(os.environ)
                    environment.pop("KIMOJIO_RETENTION_OUTPUT", None)
                    environment.pop("KIMOJIO_RETENTION_CONTROL", None)
                    result = subprocess.run(command, env=environment, text=True, capture_output=True,
                                            timeout=110, preexec_fn=limits)
                    record.update(returncode=result.returncode, stderr=result.stderr)
                    report = json.loads(result.stdout)
                    record["report"] = report
                    assert result.returncode == 0 and report["valid"]
                    count = (cohorts + warmup) * cell["concurrency"]
                    assert report["client_retirements"] == report["server_retirements"] == count
                    assert report["drivers_closed_successfully"] == 2
                    if cell["case"] == "duplex":
                        assert report["early_response_witnesses_including_warmup"] == count
                    a = report["allocations"]
                    for boundary in ("start", "end"):
                        field = "requested_live_" + boundary
                        assert a[field] == sum(b[field] for b in a["origin_buckets"])
                    assert a["requested_peak_live"] >= max(a["requested_live_start"], a["requested_live_end"])
                except Exception as error:
                    record["failure"] = str(error)
                    output.write(json.dumps(record) + "\n")
                    output.flush()
                    raise
                output.write(json.dumps(record) + "\n")
                output.flush()
        print(cell["label"], cell["concurrency"], "allocation cells passed", flush=True)
