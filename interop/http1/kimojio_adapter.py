"""Translate the example client's machine NDJSON into the independent adapter protocol."""

from __future__ import annotations

import argparse
import base64
import json
from pathlib import Path
from urllib.parse import urlsplit

from harness import run_command
from server_suite import ARTIFACTS, WORKSPACE, require


def decode_fields(items) -> dict[str, list[str]]:
    require(isinstance(items, list), "native fields must be name/base64 pairs")
    result = {}
    for item in items:
        require(
            isinstance(item, list) and len(item) == 2 and all(isinstance(value, str) for value in item),
            "invalid native field pair",
        )
        name, value = item
        decoded = base64.b64decode(value, validate=True).decode("latin-1")
        result.setdefault(name.lower(), []).append(decoded)
    return result


def normalize(raw: bytes, returncode: int) -> dict:
    require(returncode >= 0 and returncode != 101, "native client terminated abnormally")
    exchanges = []
    failure = None
    for line in raw.splitlines():
        if not line:
            continue
        require(failure is None, "native result contains records after a terminal error")
        row = json.loads(line)
        require(isinstance(row, dict), "native result must contain JSON objects")
        error = row.get("error")
        if error is not None:
            require(isinstance(error, str), "native error must be a string")
            kind = "timeout" if "timeout" in error.lower() or "deadline" in error.lower() else "rejected"
            failure = {"kind": kind, "detail": error}
            continue
        require(type(row.get("status")) is int, "native final status must be an integer")
        body = row.get("body_base64")
        require(isinstance(body, str), "native body must be base64")
        base64.b64decode(body, validate=True)
        exchanges.append({
            "index": len(exchanges),
            "status": row["status"],
            "body_base64": body,
            "headers": decode_fields(row.get("headers")),
            "trailers": decode_fields(row.get("trailers")),
        })
    require(bool(exchanges) or failure is not None, "native result contains no outcome")
    require(
        (returncode == 0) == (failure is None),
        "native exit code disagrees with its result; an unreported crash is not a protocol rejection",
    )
    return {"schema": 1, "exchanges": exchanges, "failure": failure}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--client", required=True)
    parser.add_argument("--native", action="store_true")
    parser.add_argument("--request", type=Path, required=True)
    parser.add_argument("--result", type=Path, required=True)
    args = parser.parse_args()
    for path in (args.request, args.result):
        require(
            path.resolve().is_relative_to(ARTIFACTS.resolve()),
            "adapter files must stay under target/interop",
        )
    job = json.loads(args.request.read_text())
    require(job.get("schema") == 1, "request schema must be 1")
    url = urlsplit(job["url"])
    require(
        url.scheme == "http" and url.hostname == "127.0.0.1" and url.port is not None
        and url.username is None and url.password is None,
        "native adapter requires an explicit loopback HTTP port",
    )
    headers = job.get("headers", [])
    expect = False
    for name, value in headers:
        require(
            name.lower() == "expect" and value.lower() == "100-continue" and not expect,
            "native adapter supports only one Expect request field",
        )
        expect = True
    body_file = args.request.with_suffix(".body")
    native_file = args.request.with_suffix(".native.ndjson")
    native_file.unlink(missing_ok=True)
    body_file.write_bytes(base64.b64decode(job["body_base64"], validate=True))
    path = url.path or "/"
    if url.query:
        path += "?" + url.query
    command = [
        args.client,
        "--connect", f"{url.hostname}:{url.port}",
        "--method", job["method"],
        "--path", path,
        "--body-file", str(body_file),
        "--result-file", str(native_file),
        "--count", str(job["count"]),
    ]
    require(job["body_mode"] in ("known-length", "chunked"), "unknown request body mode")
    if job["body_mode"] == "chunked":
        command.append("--chunked")
    if expect:
        command.append("--expect-continue")
    if args.native:
        command.append("--native")
    completed = run_command(command, cwd=WORKSPACE, timeout=job["timeout_ms"] / 1000)
    require(
        native_file.is_file(),
        f"native client did not write a result: exit {completed.returncode}, stderr {completed.stderr!r}",
    )
    require(native_file.stat().st_size <= 128 * 1024, "native result exceeds limit")
    report = normalize(native_file.read_bytes(), completed.returncode)
    args.result.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
