"""Command-JSON adapter protocol for independent HTTP/1 client tests."""

from __future__ import annotations

import argparse
import base64
import contextlib
import json
from pathlib import Path
import sys
import socket
import time
import uuid

from harness import fields, run_command
from scripted import ScriptedPeer
from server_suite import ARTIFACTS, WORKSPACE, require


RESPONSES = {
    "length": (b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\nonetwo", 200, b"onetwo"),
    "chunked": (
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n"
        b"3;part=first\r\none\r\n3\r\ntwo\r\n0\r\nX-End: done\r\n\r\n",
        200, b"onetwo",
    ),
    "interim": (
        b"HTTP/1.1 103 Early Hints\r\nLink: </a>\r\n\r\n"
        b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nend",
        200, b"end",
    ),
    "head": (b"HTTP/1.1 200 OK\r\nContent-Length: 999\r\n\r\n", 200, b""),
    "no-content": (b"HTTP/1.1 204 No Content\r\n\r\n", 204, b""),
    "not-modified": (b"HTTP/1.1 304 Not Modified\r\nContent-Length: 999\r\n\r\n", 304, b""),
    "close-delimited": (b"HTTP/1.0 200 OK\r\n\r\nclosed-body", 200, b"closed-body"),
    "truncated-length": (b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nshort", None, None),
    "truncated-chunk": (
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nabc",
        None, None,
    ),
    "conflicting-length": (
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Length: 3\r\n\r\nabc",
        None, None,
    ),
    "invalid-status": (b"THIS IS NOT HTTP\r\n\r\n", None, None),
}
STRICT_RESPONSES = {
    "transfer-encoding-with-length": (
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: 0\r\n\r\n0\r\n\r\n",
        None, None,
    ),
}
EXPECT_RESPONSES = {
    "expect-continue": RESPONSES["length"],
    "expect-reject": (
        b"HTTP/1.1 417 Expectation Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        417, b"",
    ),
}


def request_head(wire, method: str, path: str) -> tuple[tuple[bytes, bytes], ...]:
    raw = wire.until(b"\r\n\r\n", 65536)
    lines = raw[:-4].split(b"\r\n")
    require(
        lines[0] == f"{method} {path} HTTP/1.1".encode(),
        f"unexpected request line {lines[0]!r}",
    )
    headers = fields(lines[1:])
    require(sum(key == b"host" for key, _value in headers) == 1, "request needs one Host")
    return headers


def read_upload(wire, headers) -> bytes:
    length = [value for key, value in headers if key == b"content-length"]
    transfer = [value.lower() for key, value in headers if key == b"transfer-encoding"]
    require(not (length and transfer), "ambiguous request framing")
    if transfer:
        require(transfer == [b"chunked"], "expected plain chunked upload")
        body = bytearray()
        while True:
            size_line = wire.until(b"\r\n", 128)[:-2]
            require(size_line and all(c in b"0123456789abcdefABCDEF" for c in size_line), "bad chunk size")
            size = int(size_line, 16)
            require(len(body) + size <= 8192, "upload exceeds fixture limit")
            if size == 0:
                require(wire.take(2) == b"\r\n", "unexpected upload trailers")
                break
            body.extend(wire.take(size))
            require(wire.take(2) == b"\r\n", "bad upload chunk terminator")
        return bytes(body)
    require(len(length) == 1 and length[0].isdigit(), "missing request body framing")
    size = int(length[0])
    require(size <= 8192, "upload exceeds fixture limit")
    return wire.take(size)


def load_adapter(path: Path) -> dict:
    adapter = json.loads(path.read_text())
    require(isinstance(adapter, dict), "adapter must be a JSON object")
    require(adapter.get("schema") == 1, "adapter schema must be 1")
    command = adapter.get("command")
    require(
        isinstance(command, list) and bool(command) and all(isinstance(item, str) for item in command),
        "adapter command must be a nonempty string array",
    )
    require(
        any("{request_file}" in item for item in command)
        and any("{result_file}" in item for item in command),
        "adapter command must contain request_file and result_file placeholders",
    )
    capabilities = adapter.get("capabilities")
    require(
        isinstance(capabilities, list) and all(isinstance(item, str) for item in capabilities),
        "adapter capabilities must be an array of names",
    )
    require("basic" in capabilities, "basic capability is required")
    require(
        set(capabilities) <= {
            "basic", "reuse", "upload", "chunked-upload", "interim", "trailers", "strict-framing",
            "expect-continue",
        },
        "unknown adapter capability",
    )
    return adapter


def selected_cases(adapter: dict) -> list[str]:
    names = list(RESPONSES)
    if "reuse" in adapter["capabilities"]:
        names.append("reuse")
    if "upload" in adapter["capabilities"]:
        names.append("upload")
    if "chunked-upload" in adapter["capabilities"]:
        names.append("chunked-upload")
    if "strict-framing" in adapter["capabilities"]:
        names.extend(STRICT_RESPONSES)
    if "expect-continue" in adapter["capabilities"]:
        names.extend(EXPECT_RESPONSES)
    return names


def run_case(adapter: dict, name: str, directory: Path) -> dict:
    directory.mkdir(parents=True)
    expect = name in EXPECT_RESPONSES
    method = "HEAD" if name == "head" else "POST" if "upload" in name or expect else "GET"
    upload = b"fixture-upload-123" if "upload" in name or expect else b""
    count = 2 if name == "reuse" else 1
    expected_wire, status, body = {**RESPONSES, **STRICT_RESPONSES, **EXPECT_RESPONSES}.get(name, RESPONSES["length"])
    observations = []

    def serve(wire, _connection_index):
        for index in range(count):
            before = len(wire._wire)
            headers = request_head(wire, method, "/fixture")
            if expect:
                require(
                    [value.lower() for key, value in headers if key == b"expect"] == [b"100-continue"],
                    "missing Expect request field",
                )
            if name == "expect-continue":
                require(not wire.buffer, "client sent body before 100 Continue")
                wire.send(b"HTTP/1.1 100 Continue\r\n\r\n")
            if upload:
                transfer = [value.lower() for key, value in headers if key == b"transfer-encoding"]
                if name == "chunked-upload":
                    require(transfer == [b"chunked"], "client did not select chunked upload framing")
                else:
                    require(not transfer, "client did not select known-length upload framing")
            received = read_upload(wire, headers) if upload and name != "expect-reject" else b""
            require(
                received == (b"" if name == "expect-reject" else upload),
                "independent peer received wrong upload body",
            )
            observations.append({"index": index, "request_bytes": len(wire._wire) - before})
            wire.send(expected_wire, chunk_size=1 if status is not None else None)
        with contextlib.suppress(OSError):
            wire.sock.shutdown(socket.SHUT_WR)
        wire.expect_eof(allow_reset=status is None)

    with ScriptedPeer(serve, timeout=8) as peer:
        request_file = directory / "request.json"
        result_file = directory / "result.json"
        request_file.write_text(json.dumps({
            "schema": 1,
            "url": f"http://{peer.address[0]}:{peer.address[1]}/fixture",
            "method": method,
            "headers": [["expect", "100-continue"]] if expect else [],
            "body_base64": base64.b64encode(upload).decode(),
            "body_mode": "chunked" if name == "chunked-upload" else "known-length",
            "count": count,
            "connection": "reuse" if count > 1 else "single",
            "timeout_ms": 6000,
        }, indent=2) + "\n")
        command = [
            item.replace("{request_file}", str(request_file)).replace("{result_file}", str(result_file))
            for item in adapter["command"]
        ]
        completed = run_command(command, cwd=WORKSPACE, timeout=8)
        require(completed.returncode == 0, f"adapter failed: {completed.stderr!r}")
        require(result_file.is_file(), "adapter did not create result file")
        require(result_file.stat().st_size <= 128 * 1024, "adapter result exceeds limit")
        report = json.loads(result_file.read_text())
        require(isinstance(report, dict), "result must be a JSON object")
        require(report.get("schema") == 1, "result schema must be 1")
        failure = report.get("failure")
        if status is None:
            require(
                isinstance(failure, dict)
                and failure.get("kind") in ("protocol", "io", "rejected"),
                "malformed response must fail, not complete or time out",
            )
        else:
            require(failure is None, f"unexpected client failure: {failure}")
            exchanges = report.get("exchanges")
            require(isinstance(exchanges, list) and len(exchanges) == count, "wrong exchange count")
            for index, exchange in enumerate(exchanges):
                require(isinstance(exchange, dict), "exchange must be a JSON object")
                require(exchange.get("index") == index, "wrong exchange order")
                require(exchange.get("status") == status, "wrong final status")
                require(
                    base64.b64decode(exchange["body_base64"], validate=True) == body,
                    "wrong response body",
                )
                if "interim" in adapter["capabilities"]:
                    require(
                        exchange.get("interim_statuses") == (
                            [103, 100] if name == "interim" else [100] if name == "expect-continue" else []
                        ),
                        "wrong informational response sequence",
                    )
                if "trailers" in adapter["capabilities"]:
                    trailers = exchange.get("trailers") or {}
                    require(isinstance(trailers, dict), "trailers must be a JSON object")
                    require(
                        {key.lower(): value for key, value in trailers.items()}
                        == ({"x-end": ["done"]} if name == "chunked" else {}),
                        "wrong response trailers",
                    )
    require(len(observations) == count, "wrong request count at independent peer")
    return {"case": name, "passed": True, "observations": observations}


def run_suite(adapter: dict, directory: Path) -> dict:
    results = []
    for name in selected_cases(adapter):
        started = time.monotonic()
        try:
            result = run_case(adapter, name, directory / name)
        except (AssertionError, OSError, ValueError, KeyError, TypeError) as error:
            result = {"case": name, "passed": False, "error": str(error)}
        result["seconds"] = round(time.monotonic() - started, 6)
        results.append(result)
    report = {
        "schema": 1,
        "adapter": adapter,
        "capabilities": adapter["capabilities"],
        "results": results,
        "not_covered": ["cancel", "stalled-upload", "upgrade", "deterministic-short-write"],
    }
    for capability in (
        "reuse", "upload", "chunked-upload", "interim", "trailers", "strict-framing", "expect-continue",
    ):
        if capability not in adapter["capabilities"]:
            report["not_covered"].append(capability)
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "results.json").write_text(json.dumps(report, indent=2) + "\n")
    return report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--adapter", type=Path, required=True)
    args = parser.parse_args()
    adapter = load_adapter(args.adapter)
    directory = ARTIFACTS / f"client-{uuid.uuid4().hex}"
    report = run_suite(adapter, directory)
    print(json.dumps(report, indent=2))
    print(f"REPORT {directory / 'results.json'}", file=sys.stderr)
    return 0 if all(result["passed"] for result in report["results"]) else 1


if __name__ == "__main__":
    sys.exit(main())
