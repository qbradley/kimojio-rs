"""Repeat the same capped retention controls with both immutable allocation binaries."""
import json
from pathlib import Path
import subprocess

HERE = Path(__file__).resolve().parent
freeze = json.loads((HERE / "freeze.json").read_text())
for version, binaries in (("baseline", freeze["baseline_binaries"]), ("candidate", freeze["binaries"])):
    folder = HERE / "retention" / version
    for backend in ("native", "generic"):
        for payload, cohorts in (("static", 32), ("owned", 8)):
            subprocess.run([
                "python3", str(HERE.parent / "retention/run.py"), "--binary", binaries["allocations"]["path"],
                "--backend", backend, "--payload", payload, "--cohorts", str(cohorts),
                "--output-directory", str(folder),
            ], check=True)
        subprocess.run([
            "python3", str(HERE.parent / "retention-candidate-a4af94fd/empty.py"),
            "--binary", binaries["allocations"]["path"], "--backend", backend,
            "--cohorts", "4096", "--output-directory", str(folder),
        ], check=True)
