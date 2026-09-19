"""Run independent raw HTTP/1 checks against a command-supplied static server."""

from __future__ import annotations

import argparse
import base64
import dataclasses
import hashlib
import http.client
import json
from pathlib import Path
import sys
import time
import uuid

from harness import PeerProcess, ProtocolError, Wire, request, run_command


WORKSPACE = Path(__file__).resolve().parents[2]
ARTIFACTS = WORKSPACE / "target" / "interop"
FIXTURE = bytes(range(256)) * 16 + b"\r\nHTTP/1.1 599 Not-A-Response\r\n\r\n"
SECRET = b"OUTSIDE-ROOT-DO-NOT-SERVE-927610\n"


@dataclasses.dataclass(frozen=True)
class Case:
    name: str
    strict: bool = False


PORTABLE_NEGATIVES = {
    "conflicting-length": b"POST /fixture.bin HTTP/1.1\r\nHost: localhost\r\n"
    b"Content-Length: 1\r\nContent-Length: 2\r\n\r\nxx",
    "negative-length": b"POST /fixture.bin HTTP/1.1\r\nHost: localhost\r\n"
    b"Content-Length: -1\r\n\r\n",
    "nondigit-length": b"POST /fixture.bin HTTP/1.1\r\nHost: localhost\r\n"
    b"Content-Length: 1x\r\n\r\n",
    "missing-host": b"GET /fixture.bin HTTP/1.1\r\n\r\n",
    "duplicate-host": b"GET /fixture.bin HTTP/1.1\r\nHost: a\r\nHost: b\r\n\r\n",
    "space-in-name": b"GET /fixture.bin HTTP/1.1\r\nHost: localhost\r\nBad Name: a\r\n\r\n",
    "space-before-colon": b"GET /fixture.bin HTTP/1.1\r\nHost : localhost\r\n\r\n",
    "nul-in-field": b"GET /fixture.bin HTTP/1.1\r\nHost: localhost\r\nX: a\x00b\r\n\r\n",
    "bad-method": b"G(ET /fixture.bin HTTP/1.1\r\nHost: localhost\r\n\r\n",
    "overflow-length": b"GET /fixture.bin HTTP/1.1\r\nHost: localhost\r\n"
    b"Content-Length: 999999999999999999999999999999999\r\n\r\n",
}

STRICT_NEGATIVES = {
    "transfer-encoding-with-length": b"GET /fixture.bin HTTP/1.1\r\nHost: localhost\r\n"
    b"Transfer-Encoding: chunked\r\nContent-Length: 5\r\n\r\n0\r\n\r\n",
    "folded-field": b"GET /fixture.bin HTTP/1.1\r\nHost: localhost\r\nX: a\r\n b\r\n\r\n",
    "bare-lf": b"GET /fixture.bin HTTP/1.1\nHost: localhost\n\n",
}
EAGER_NEGATIVES = {
    "bare-lf-prefix": b"GET /fixture.bin HTTP/1.1\n",
    "bare-cr-prefix": b"GET /fixture.bin HTTP/1.1\rX",
}


def create_fixtures() -> Path:
    directory = ARTIFACTS / f"run-{uuid.uuid4().hex}"
    root = directory / "root"
    root.mkdir(parents=True)
    (root / "fixture.bin").write_bytes(FIXTURE)
    (root / "empty.bin").write_bytes(b"")
    (root / "space name.txt").write_bytes(b"space name\n")
    (root / "nested").mkdir()
    (root / "nested" / "item.txt").write_bytes(b"nested\n")
    (directory / "outside-secret.txt").write_bytes(SECRET)
    (root / "outside-link.txt").symlink_to("../outside-secret.txt")
    return root


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ProtocolError(message)


def check_file(response, body: bytes, *, head: bool = False, version: str = "1.1") -> None:
    require(response.status == 200, f"expected 200, received {response.status}")
    require(response.version == version, f"expected HTTP/{version}, received HTTP/{response.version}")
    require(response.body == (b"" if head else body), "file body mismatch")
    require(
        response.values(b"content-length") == [str(len(body)).encode()],
        "static response must advertise the exact file Content-Length",
    )


def cases(profile: str) -> list[Case]:
    names = [
        "get",
        "head",
        "empty",
        "encoded-space",
        "nested",
        "missing",
        "keepalive",
        "pipeline",
        "fragmented-request",
        "http10-close",
        "head-then-get",
        "unsupported-expect",
        "content-length-request",
        "chunked-request",
        "request-trailers",
        "python-client",
        "traversal",
    ]
    result = [Case(name) for name in names]
    result += [Case(name) for name in PORTABLE_NEGATIVES]
    if profile == "strict":
        result += [Case(name, True) for name in STRICT_NEGATIVES]
        result += [Case(name, True) for name in EAGER_NEGATIVES]
        result.append(Case("symlink-escape", True))
    return result


def run_case(address: tuple[str, int], name: str) -> None:
    if name == "python-client":
        connection = http.client.HTTPConnection(*address, timeout=3)
        try:
            for method, expected in (("GET", FIXTURE), ("HEAD", b""), ("GET", FIXTURE)):
                connection.request(method, "/fixture.bin")
                response = connection.getresponse()
                require(response.status == 200, f"Python peer got {response.status}")
                require(response.read(len(FIXTURE) + 1) == expected, "Python peer body mismatch")
        finally:
            connection.close()
        return
    if name == "traversal":
        for path in (
            "/../outside-secret.txt",
            "/%2e%2e/outside-secret.txt",
            "/%2e%2e%2foutside-secret.txt",
            "/nested/../../outside-secret.txt",
            "/%252e%252e/outside-secret.txt",
        ):
            with Wire.connect(address) as wire:
                wire.send(request(path, close=True))
                response = wire.response()
                require(
                    response.status in (301, 302, 400, 403, 404),
                    f"unexpected traversal status for {path}: {response.status}",
                )
                require(SECRET not in response.wire, "outside-root content leaked")
                wire.expect_eof()
        return
    with Wire.connect(address, timeout=1 if name in EAGER_NEGATIVES else 3) as wire:
        if name in ("get", "head", "empty", "encoded-space", "nested"):
            path, body = {
                "empty": ("/empty.bin", b""),
                "encoded-space": ("/space%20name.txt", b"space name\n"),
                "nested": ("/nested/item.txt", b"nested\n"),
            }.get(name, ("/fixture.bin", FIXTURE))
            method = "HEAD" if name == "head" else "GET"
            wire.send(request(path, method=method, close=True))
            check_file(wire.response(method), body, head=method == "HEAD")
            wire.expect_eof()
        elif name == "missing":
            wire.send(request("/absent.bin", close=True))
            require(wire.response().status == 404, "missing file must return 404")
            wire.expect_eof()
        elif name in ("keepalive", "pipeline", "head-then-get"):
            first_method = "HEAD" if name == "head-then-get" else "GET"
            first = request(method=first_method)
            second = request("/empty.bin", close=True)
            wire.send(first + second if name == "pipeline" else first)
            check_file(wire.response(first_method), FIXTURE, head=first_method == "HEAD")
            if name != "pipeline":
                wire.send(second)
            check_file(wire.response(), b"")
            wire.expect_eof()
        elif name == "fragmented-request":
            wire.send(request(close=True), chunk_size=1)
            check_file(wire.response(), FIXTURE)
            wire.expect_eof()
        elif name in ("content-length-request", "chunked-request", "request-trailers"):
            if name == "content-length-request":
                framing = b"Content-Length: 6\r\n"
                body = b"onetwo"
            else:
                framing = b"Transfer-Encoding: chunked\r\n"
                body = b"3;part=first\r\none\r\n3\r\ntwo\r\n0\r\n"
                if name == "request-trailers":
                    framing += b"Trailer: X-End\r\n"
                    body += b"X-End: done\r\n"
                body += b"\r\n"
            wire.send(
                b"GET /fixture.bin HTTP/1.1\r\nHost: localhost\r\n"
                + framing
                + b"\r\n"
                + body
                + request("/empty.bin", close=True),
                chunk_size=1,
            )
            check_file(wire.response(), FIXTURE)
            check_file(wire.response(), b"")
            wire.expect_eof()
        elif name == "http10-close":
            wire.send(request(version="1.0"))
            response = wire.response()
            check_file(response, FIXTURE, version="1.0")
            require(not response.values(b"transfer-encoding"), "HTTP/1.0 response must not use Transfer-Encoding")
            wire.expect_eof()
        elif name == "unsupported-expect":
            wire.send(
                b"GET /fixture.bin HTTP/1.1\r\nHost: localhost\r\n"
                b"Expect: unsupported-token\r\nContent-Length: 6\r\nConnection: close\r\n\r\n"
            )
            require(wire.response().status == 417, "unsupported Expect needs immediate 417 without upload")
            wire.expect_eof()
        elif name == "symlink-escape":
            wire.send(request("/outside-link.txt", close=True))
            response = wire.response()
            require(response.status in (403, 404), "outside-root symlink must be denied")
            require(SECRET not in response.wire, "outside-root symlink content leaked")
            wire.expect_eof()
        else:
            raw = {**PORTABLE_NEGATIVES, **STRICT_NEGATIVES, **EAGER_NEGATIVES}[name]
            wire.send(raw + (b"" if name in EAGER_NEGATIVES else request("/empty.bin", close=True)))
            response = wire.response()
            require(response.status == 400, f"invalid request needs 400, received {response.status}")
            # A second response would show that the connection was reused.
            wire.expect_eof(allow_reset=True)


def run_suite(address: tuple[str, int], profile: str) -> list[dict]:
    results = []
    for case in cases(profile):
        start = time.monotonic()
        try:
            run_case(address, case.name)
            result = {"case": case.name, "passed": True}
        except (AssertionError, OSError, ValueError, http.client.HTTPException) as error:
            result = {"case": case.name, "passed": False, "error": str(error)}
        result["seconds"] = round(time.monotonic() - start, 6)
        result["policy"] = "strict" if case.strict else "portable"
        results.append(result)
    return results


def go_client_checks(binary: str, address: tuple[str, int]) -> list[dict]:
    results = []
    for method in ("GET", "HEAD"):
        try:
            completed = run_command(
                [
                    binary,
                    "--mode", "client",
                    "--method", method,
                    "--url", f"http://{address[0]}:{address[1]}/fixture.bin",
                    "--count", "2",
                ],
                cwd=WORKSPACE,
                timeout=12,
            )
            require(
                completed.returncode == 0,
                f"Go peer exited {completed.returncode}: {completed.stderr!r}",
            )
            rows = [json.loads(line) for line in completed.stdout.splitlines()]
            require(len(rows) == 2, "Go peer did not complete exactly two requests")
            for index, row in enumerate(rows):
                require(row["index"] == index, "Go peer response order mismatch")
                require(row["status"] == 200, f"Go peer got {row['status']}")
                require(
                    base64.b64decode(row["body_base64"], validate=True)
                    == (b"" if method == "HEAD" else FIXTURE),
                    "Go peer body mismatch",
                )
            result = {"case": f"go-client-{method.lower()}", "passed": True}
        except (AssertionError, OSError, ValueError, KeyError, TypeError) as error:
            result = {
                "case": f"go-client-{method.lower()}",
                "passed": False,
                "error": str(error),
            }
        results.append(result)
    return results


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", choices=("strict", "portable"), default="strict")
    parser.add_argument("--server-command", help="JSON array; alternative to arguments after --")
    parser.add_argument("--go-peer", help="path to the standard-library Go peer binary")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if args.server_command and args.command:
        parser.error("supply one command form")
    try:
        command = json.loads(args.server_command) if args.server_command else args.command
    except json.JSONDecodeError as error:
        parser.error(f"invalid command JSON: {error}")
    if isinstance(command, list) and command and command[0] == "--":
        command = command[1:]
    if not isinstance(command, list) or not command or not all(
        isinstance(arg, str) for arg in command
    ):
        parser.error("server command must be a nonempty string array")
    root = create_fixtures()
    command = [
        arg.replace("{root}", str(root)).replace("{bind}", "127.0.0.1:0")
        for arg in command
    ]
    report = {
        "profile": args.profile,
        "matrix": "application-boundary-v3",
        "command": command,
        "fixture_sha256": hashlib.sha256(FIXTURE).hexdigest(),
        "results": [],
        "peers": ["python-raw", "python-http.client"] + (["go-net/http"] if args.go_peer else []),
    }
    try:
        with PeerProcess(command, cwd=WORKSPACE) as server:
            report["results"] = run_suite(server.address, args.profile)
            if args.go_peer:
                report["results"].extend(go_client_checks(args.go_peer, server.address))
            report["peer_log"] = server.diagnostics()
    except (OSError, RuntimeError, TimeoutError) as error:
        report["startup_error"] = str(error)
    output = root.parent / "results.json"
    output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    print(f"REPORT {output}", file=sys.stderr)
    return 0 if report["results"] and all(row["passed"] for row in report["results"]) else 1


if __name__ == "__main__":
    sys.exit(main())
