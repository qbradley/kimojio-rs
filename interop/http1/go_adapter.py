"""Reference adapter for the independent Go peer, not the new Rust client."""

import argparse
import base64
import json
from pathlib import Path

from harness import run_command
from server_suite import WORKSPACE


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--peer", required=True)
    parser.add_argument("--request", type=Path, required=True)
    parser.add_argument("--result", type=Path, required=True)
    args = parser.parse_args()
    request = json.loads(args.request.read_text())
    body = base64.b64decode(request["body_base64"], validate=True)
    body_file = args.request.with_suffix(".body")
    body_file.write_bytes(body)
    command = [
        args.peer,
        "--mode", "client",
        "--url", request["url"],
        "--method", request["method"],
        "--body-file", str(body_file),
        "--count", str(request["count"]),
    ]
    if request["body_mode"] == "chunked":
        command.append("--chunked")
    if request["headers"] == [["expect", "100-continue"]]:
        command.append("--expect-continue")
    elif request["headers"]:
        raise ValueError("Go adapter supports only the Expect request field")
    result = run_command(command, cwd=WORKSPACE, timeout=request["timeout_ms"] / 1000)
    exchanges = [json.loads(line) for line in result.stdout.splitlines()]
    failure = None
    if result.returncode:
        detail = result.stderr.decode(errors="replace")
        kind = "timeout" if "timeout" in detail.lower() or "deadline" in detail.lower() else "rejected"
        failure = {"kind": kind, "detail": detail}
    args.result.write_text(json.dumps({
        "schema": 1, "exchanges": exchanges, "failure": failure,
    }, indent=2) + "\n")


if __name__ == "__main__":
    main()
