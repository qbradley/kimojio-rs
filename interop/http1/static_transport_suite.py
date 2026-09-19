"""Bounded static-server reset, timeout, recovery, and descriptor checks."""

from __future__ import annotations

import argparse
import base64
import json
import os
from pathlib import Path
import socket
import struct
import sys
import time
import uuid

from harness import PeerProcess, Wire, fields, request
from server_suite import ARTIFACTS, FIXTURE, WORKSPACE, create_fixtures, require


LARGE_BYTES = 8 * 1024 * 1024


def prepare_large_file(root: Path) -> None:
    block = bytes(range(256)) * 4096
    with (root / "large.bin").open("wb") as output:
        for _ in range(8):
            output.write(block)


def healthy(address) -> float:
    started = time.monotonic()
    with Wire.connect(address, timeout=3) as wire:
        wire.send(request(close=True))
        response = wire.response()
        require(response.status == 200 and response.body == FIXTURE, "recovery GET failed")
        wire.expect_eof()
    return time.monotonic() - started


def run_probes(address, process, *, include_timeout=True, iterations=4) -> dict:
    require(1 <= iterations <= 8, "probe iteration count must be between 1 and 8")
    healthy(address)
    baseline = len(os.listdir(f"/proc/{process.pid}/fd"))
    report = {
        "matrix": "static-transport-v1",
        "baseline_fds": baseline,
        "descriptor_allowance": 2,
        "timeout_case_selected": include_timeout,
        "results": [],
    }
    for iteration in range(iterations):
        for kind in ("partial-header-reset", "partial-body-reset", "stalled-response-reset"):
            row = {"case": kind, "iteration": iteration}
            try:
                with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as client:
                    client.settimeout(3)
                    client.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
                    client.connect(address)
                    if kind == "partial-header-reset":
                        client.sendall(b"GET /fixture.bin HTTP/1.1\r\nHost: loca")
                    elif kind == "partial-body-reset":
                        client.sendall(
                            b"GET /fixture.bin HTTP/1.1\r\nHost: localhost\r\n"
                            b"Content-Length: 100000\r\n\r\nbody-prefix"
                        )
                    else:
                        client.sendall(request("/large.bin", close=True))
                        wire = Wire(client, timeout=3)
                        head = wire.until(b"\r\n\r\n", 65536)
                        row["response_head_base64"] = base64.b64encode(head).decode()
                        require(head.startswith(b"HTTP/1.1 200 "), "large response did not start 200")
                        headers = fields(head[:-4].split(b"\r\n")[1:])
                        require(
                            [value for key, value in headers if key == b"content-length"]
                            == [str(LARGE_BYTES).encode()],
                            "wrong large response length",
                        )
                        prefix = bytes(wire.buffer)
                        require(
                            prefix == (bytes(range(256)) * ((len(prefix) + 255) // 256))[: len(prefix)],
                            "large response prefix is corrupt",
                        )
                        row["already_read_body_bytes"] = len(prefix)
                        row["receive_buffer_bytes"] = client.getsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF)
                        time.sleep(0.1)
                    client.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
                row["recovery_seconds"] = round(healthy(address), 6)
                row["fds_after_recovery"] = len(os.listdir(f"/proc/{process.pid}/fd"))
                require(
                    row["fds_after_recovery"] <= baseline + report["descriptor_allowance"],
                    "descriptor growth exceeded settle allowance",
                )
                require(process.poll() is None, "server exited")
                row["passed"] = True
            except (AssertionError, OSError, ValueError) as error:
                row.update(passed=False, error=repr(error))
            report["results"].append(row)
    if include_timeout:
        row = {"case": "partial-head-deadline-response", "wire_deadline_seconds": 3}
        try:
            with Wire.connect(address, timeout=3) as wire:
                wire.send(b"GET /fixture.bin HTTP/1.1\r\nHost: loca")
                response = wire.response()
                row["status"] = response.status
                row["wire_base64"] = base64.b64encode(response.wire).decode()
                require(response.status == 408, "missing 408 partial-request timeout response")
                wire.expect_eof()
            row["recovery_seconds"] = round(healthy(address), 6)
            row["passed"] = True
        except (AssertionError, OSError, ValueError) as error:
            row.update(passed=False, error=repr(error))
        report["results"].append(row)
    report["final_fds"] = len(os.listdir(f"/proc/{process.pid}/fd"))
    report["scope"] = (
        "Bounded reset/recovery checks with sampled child descriptor counts; "
        "not exhaustive leak proof or deterministic kernel-receipt injection."
    )
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--server-command", required=True, help="JSON argument array")
    parser.add_argument("--without-timeout", action="store_true", help="select only the reset/recovery profile")
    args = parser.parse_args()
    command = json.loads(args.server_command)
    require(
        isinstance(command, list) and command and all(isinstance(item, str) for item in command),
        "server command must be a nonempty string array",
    )
    root = create_fixtures()
    prepare_large_file(root)
    command = [
        item.replace("{bind}", "127.0.0.1:0").replace("{root}", str(root)) for item in command
    ]
    report = {"command": command, "results": []}
    server = PeerProcess(command, cwd=WORKSPACE)
    try:
        with server:
            report.update(run_probes(
                server.address, server.process, include_timeout=not args.without_timeout
            ))
    except (OSError, RuntimeError, AssertionError) as error:
        report["setup_error"] = repr(error)
    finally:
        report["peer_log"] = server.diagnostics()
    output = ARTIFACTS / f"static-transport-{uuid.uuid4().hex}.json"
    output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    print(f"REPORT {output}", file=sys.stderr)
    return 0 if report["results"] and all(row["passed"] for row in report["results"]) and not report.get("setup_error") else 1


if __name__ == "__main__":
    sys.exit(main())
