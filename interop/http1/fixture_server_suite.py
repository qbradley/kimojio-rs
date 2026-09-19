"""Independent checks for the documented wrapper fixture routes, not static files."""

from __future__ import annotations

import argparse
import base64
import json
import sys
import time
import uuid

from harness import PeerProcess, Wire, request, run_command
from server_suite import ARTIFACTS, EAGER_NEGATIVES, WORKSPACE, require


CASES = (
    "get", "head", "keepalive", "pipeline", "trailers", "echo-length",
    "echo-chunked", "echo-chunked-trailers", "echo-continue",
    "early-expect", "early-no-expect", "missing-host", "negative-length", "te-cl",
    "bare-lf-prefix", "bare-cr-prefix", "http10-close", "unsupported-expect",
)


class TraceWire(Wire):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.trace = []

    def send(self, data, **kwargs):
        event = {
            "event": "send-intent",
            "base64": base64.b64encode(data).decode(),
            "chunk_size": kwargs.get("chunk_size"),
        }
        self.trace.append(event)
        try:
            super().send(data, **kwargs)
            event["completed"] = True
        except BaseException as error:
            event["error"] = repr(error)
            raise

    def _receive(self):
        before = len(self.buffer)
        try:
            super()._receive()
            self.trace.append({
                "event": "receive",
                "base64": base64.b64encode(self.buffer[before:]).decode(),
                "eof": self.eof,
            })
        except BaseException as error:
            self.trace.append({"event": "receive-error", "error": repr(error)})
            raise


def expected(wire, status, body, *, method="GET", trailers=(), version="1.1"):
    response = wire.response(method)
    require(response.status == status, f"expected {status}, received {response.status}")
    require(response.version == version, f"expected HTTP/{version}, received HTTP/{response.version}")
    require(response.body == body, f"wrong body: received {len(response.body)} bytes")
    require(response.trailers == trailers, f"wrong trailers: {response.trailers!r}")
    return response


def probe(wire, name):
    if name == "get":
        wire.send(request("/", close=True))
        expected(wire, 200, b"hello from kimojio-http1\n")
    elif name == "head":
        wire.send(request("/", method="HEAD", close=True))
        expected(wire, 200, b"", method="HEAD")
    elif name == "http10-close":
        wire.send(request("/", version="1.0"))
        response = expected(wire, 200, b"hello from kimojio-http1\n", version="1.0")
        require(not response.values(b"transfer-encoding"), "HTTP/1.0 response must not use Transfer-Encoding")
    elif name == "unsupported-expect":
        wire.send(
            b"POST /echo HTTP/1.1\r\nHost: localhost\r\n"
            b"Expect: unsupported-token\r\nContent-Length: 6\r\nConnection: close\r\n\r\n"
        )
        require(wire.response("POST").status == 417, "unsupported Expect needs immediate 417 without upload")
    elif name in ("keepalive", "pipeline"):
        first = request("/bytes/17")
        second = request("/bytes/0", close=True)
        wire.send(first + second if name == "pipeline" else first)
        expected(wire, 200, b"x" * 17)
        if name != "pipeline":
            wire.send(second)
        expected(wire, 200, b"")
    elif name == "trailers":
        wire.send(request("/trailers", close=True))
        expected(wire, 200, b"first\nsecond\n", trailers=((b"x-finished", b"yes"),))
    elif name in ("echo-length", "echo-chunked", "echo-chunked-trailers", "echo-continue"):
        headers = b"POST /echo HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n"
        trailers = ()
        if name in ("echo-length", "echo-continue"):
            headers += b"Content-Length: 6\r\n"
            body = b"onetwo"
        else:
            headers += b"Transfer-Encoding: chunked\r\n"
            body = b"3;part=one\r\none\r\n3\r\ntwo\r\n0\r\n"
            if name == "echo-chunked-trailers":
                headers += b"Trailer: X-Probe\r\n"
                body += b"X-Probe: done\r\n"
                trailers = ((b"x-probe", b"done"),)
            body += b"\r\n"
        if name == "echo-continue":
            wire.send(headers + b"Expect: 100-continue\r\n\r\n", chunk_size=1)
            expected(wire, 100, b"")
            wire.send(body, chunk_size=1)
        else:
            wire.send(headers + b"\r\n" + body, chunk_size=1)
        expected(wire, 200, b"onetwo", method="POST", trailers=trailers)
    elif name in ("early-expect", "early-no-expect"):
        wire.send(
            b"POST /early HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n"
            b"Content-Length: 1048576\r\n"
            + (b"Expect: 100-continue\r\n" if name == "early-expect" else b"")
            + b"\r\n"
        )
        expected(wire, 413, b"", method="POST")
    elif name in EAGER_NEGATIVES:
        wire.send(EAGER_NEGATIVES[name])
        response = wire.response()
        require(response.status == 400, f"invalid prefix needs immediate 400, received {response.status}")
    else:
        bad = {
            "missing-host": b"GET / HTTP/1.1\r\n\r\n",
            "negative-length": b"POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: -1\r\n\r\n",
            "te-cl": b"POST /echo HTTP/1.1\r\nHost: localhost\r\n"
            b"Transfer-Encoding: chunked\r\nContent-Length: 5\r\n\r\n0\r\n\r\n",
        }[name]
        wire.send(bad + request("/bytes/0", close=True))
        response = wire.response()
        require(response.status == 400, f"invalid request needs 400, received {response.status}")
    wire.expect_eof(allow_reset=name in ("missing-host", "negative-length", "te-cl") or name in EAGER_NEGATIVES)


def run_suite(address, *, go_peer=None):
    results = []
    for name in CASES:
        row = {"case": name}
        started = time.monotonic()
        wire = None
        try:
            with TraceWire.connect(address, timeout=1 if name in EAGER_NEGATIVES else 3) as wire:
                probe(wire, name)
            row["passed"] = True
        except (AssertionError, OSError, ValueError) as error:
            row.update(passed=False, error=repr(error))
        row["trace"] = wire.trace if wire else []
        row["seconds"] = round(time.monotonic() - started, 6)
        results.append(row)
    if go_peer:
        for chunked in (False, True):
            name = "go-upload-chunked" if chunked else "go-upload-length"
            command = [
                go_peer, "--mode", "client",
                "--url", f"http://{address[0]}:{address[1]}/echo",
                "--method", "POST", "--body", "independent-go-body", "--count", "2",
            ] + (["--chunked"] if chunked else [])
            row = {"case": name}
            try:
                completed = run_command(command, cwd=WORKSPACE, timeout=12)
                row.update(
                    stdout=completed.stdout.decode(errors="replace"),
                    stderr=completed.stderr.decode(errors="replace"),
                )
                require(completed.returncode == 0, f"Go peer exited {completed.returncode}")
                exchanges = [json.loads(line) for line in completed.stdout.splitlines()]
                require(len(exchanges) == 2, "Go peer completed the wrong exchange count")
                for index, exchange in enumerate(exchanges):
                    require(exchange["index"] == index, "Go peer response order mismatch")
                    require(exchange["status"] == 200, "Go peer received wrong status")
                    require(
                        base64.b64decode(exchange["body_base64"], validate=True) == b"independent-go-body",
                        "Go peer received wrong body",
                    )
                row["passed"] = True
            except (AssertionError, OSError, ValueError, KeyError) as error:
                row.update(passed=False, error=repr(error))
            results.append(row)
    return results


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--server-command", required=True, help="JSON argument array")
    parser.add_argument("--go-peer")
    args = parser.parse_args()
    command = json.loads(args.server_command)
    require(
        isinstance(command, list) and command and all(isinstance(item, str) for item in command),
        "server command must be a nonempty string array",
    )
    command = [item.replace("{bind}", "127.0.0.1:0") for item in command]
    report = {"command": command, "results": [], "matrix": "fixture-application-boundary-v3"}
    server = PeerProcess(command, cwd=WORKSPACE)
    try:
        with server:
            report["results"] = run_suite(server.address, go_peer=args.go_peer)
    except (OSError, RuntimeError) as error:
        report["startup_error"] = repr(error)
    finally:
        report["peer_log"] = server.diagnostics()
    ARTIFACTS.mkdir(parents=True, exist_ok=True)
    output = ARTIFACTS / f"fixture-server-{uuid.uuid4().hex}.json"
    output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    print(f"REPORT {output}", file=sys.stderr)
    return 0 if report["results"] and all(row["passed"] for row in report["results"]) else 1


if __name__ == "__main__":
    sys.exit(main())
