"""Intentional reset/slow-consumer resource challenges, never throughput benchmarks."""

import argparse
import json
import os
from pathlib import Path
import socket
import struct
import sys
import time
import uuid

from run import (
    ROOT, ARTIFACTS, Samples, digest, load_manifest, snapshot, validate_cpus,
)
from harness import PeerProcess, Wire, request
sys.path.insert(0, str(ROOT / "interop" / "websocket"))
from chat_suite import reset_recovery, slow_recipient


def require(condition, detail):
    if not condition:
        raise AssertionError(detail)


def http_recovery(address, process, kind):
    small = "/128.bin" if kind == "static" else "/bytes/128"
    large = "/1048576.bin" if kind == "static" else "/bytes/1048576"

    def healthy():
        with Wire.connect(address, timeout=3) as wire:
            wire.send(request(small, close=True))
            result = wire.response()
            require(result.status == 200 and result.body == b"x" * 128, "recovery payload/status mismatch")
            wire.expect_eof()

    healthy()
    baseline = snapshot(process.pid)
    results = []
    for iteration in range(3):
        for name in ("partial-header-reset", "partial-body-reset", "stalled-response-reset"):
            with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as conn:
                conn.settimeout(3)
                conn.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
                conn.connect(address)
                wire = Wire(conn, timeout=3)
                if name == "partial-header-reset":
                    wire.send(b"GET / HTTP/1.1\r\nHost: local")
                elif name == "partial-body-reset":
                    wire.send(b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100000\r\n\r\nprefix")
                else:
                    wire.send(request(large, close=True))
                    head = wire.until(b"\r\n\r\n", 65536)
                    require(head.startswith(b"HTTP/1.1 200 "), "stalled response did not start")
                    time.sleep(.05)
                conn.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
            started = time.monotonic()
            healthy()
            recovery_seconds = time.monotonic() - started
            sample = snapshot(process.pid)
            settle_started = time.monotonic()
            settle_samples = [sample]
            while sample["fds"] > baseline["fds"] + 2 and time.monotonic() - settle_started < 3:
                time.sleep(.02)
                sample = snapshot(process.pid)
                settle_samples.append(sample)
            require(sample["fds"] <= baseline["fds"] + 2,
                    f"descriptor growth after3s: baseline{baseline['fds']} current{sample['fds']}")
            results.append({"case": name, "iteration": iteration, "intentional_abort": True,
                            "healthy_recovery_seconds": recovery_seconds, "sample": sample,
                            "descriptor_settle_seconds": time.monotonic() - settle_started,
                            "descriptor_settle_samples": settle_samples})
    return {"baseline": baseline, "intentional_resets": 9, "results": results}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path)
    parser.add_argument("--server-cpu", type=int, default=0)
    parser.add_argument("--driver-cpu", type=int, default=6)
    args = parser.parse_args()
    validate_cpus(args.server_cpu, [args.driver_cpu])
    os.sched_setaffinity(0, {args.driver_cpu})
    identity = json.loads((ARTIFACTS / "tool-publication.json").read_text())
    manifest = load_manifest(args.manifest, identity)
    directory = ARTIFACTS / f"challenge-{uuid.uuid4().hex}"
    (directory / "root").mkdir(parents=True)
    for size in (128, 1048576):
        (directory / "root" / f"{size}.bin").write_bytes(b"x" * size)
    report = {"purpose": "intentional_aborts_and_bounded_recovery_not_throughput",
              "manifest": manifest, "results": [], "throughput": None}
    for target in manifest["targets"]:
        row = {"target": target, "unexpected_errors": []}
        command = target.get("challenge_command") if target["kind"] == "ws" else target["command"]
        try:
            require(bool(command), "explicit WS challenge_command is required")
            actual = [item.replace("{bind}", "127.0.0.1:0").replace("{root}", str(directory / "root")) for item in command]
            row["command"] = ["taskset", "-c", str(args.server_cpu), "env", "GOMAXPROCS=1", *actual]
            with PeerProcess(row["command"], cwd=ROOT) as server:
                with Samples(server.process.pid) as sampled:
                    if target["kind"] == "ws":
                        row["resets"] = reset_recovery(server.address, server.process)
                        row["intentional_resets"] = 6
                        row["slow_recipient"] = slow_recipient(server.address)
                    else:
                        row["resets"] = http_recovery(server.address, server.process, target["kind"])
                        row["intentional_resets"] = 9
                row["samples"] = sampled.rows
                row["unexpected_errors"] += sampled.errors
                row["retention_samples"] = []
                for pause in (.2, .3):
                    time.sleep(pause)
                    row["retention_samples"].append(snapshot(server.process.pid))
                row["server_log"] = server.diagnostics()
                require(digest(target["binary"]) == target["sha256"], "target hash changed")
            row["valid"] = not row["unexpected_errors"]
        except (AssertionError, OSError, RuntimeError, ValueError) as error:
            row["unexpected_errors"].append(repr(error))
            row["valid"] = False
        report["results"].append(row)
        (directory / "results.json").write_text(json.dumps(report, indent=2) + "\n")
        print(target["name"], row["valid"], row["unexpected_errors"])
    print(f"REPORT {directory / 'results.json'}")
    return 0 if all(row["valid"] for row in report["results"]) else 1


if __name__ == "__main__":
    sys.exit(main())
