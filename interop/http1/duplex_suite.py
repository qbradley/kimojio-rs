"""Independent HTTP/1 duplex progress probes for command-supplied targets."""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
from pathlib import Path
import re
import socket
import sys
import uuid

from client_suite import load_adapter, request_head
from fixture_server_suite import TraceWire
from harness import CHUNK_EXTENSIONS, STATUS, PeerProcess, fields, run_command
from scripted import ScriptedPeer
from server_suite import ARTIFACTS, WORKSPACE, require


class ResponsePrefix:
    """Read body prefixes without waiting for the response to finish."""

    def __init__(self, wire):
        self.wire = wire
        self.head = wire.until(b"\r\n\r\n", 65536)
        lines = self.head[:-4].split(b"\r\n")
        status = STATUS.fullmatch(lines[0])
        require(status is not None and int(status[2]) == 200, "duplex echo needs an early 200 head")
        headers = fields(lines[1:])
        self.closing = any(
            token.strip().lower() == b"close"
            for key, value in headers if key == b"connection"
            for token in value.split(b",")
        )
        transfer = [value.lower() for key, value in headers if key == b"transfer-encoding"]
        lengths = [value for key, value in headers if key == b"content-length"]
        require(not (transfer and lengths), "ambiguous response framing")
        require(not transfer or transfer == [b"chunked"], "unsupported response transfer coding")
        require(
            not lengths or len(lengths) == 1 and re.fullmatch(rb"[0-9]{1,20}", lengths[0]),
            "invalid response Content-Length",
        )
        self.chunked = bool(transfer)
        self.remaining = int(lengths[0]) if lengths else None
        self.chunk_remaining = 0

    def chunk_size(self):
        line = self.wire.until(b"\r\n", 4096)[:-2]
        size, separator, extension = line.partition(b";")
        if separator:
            size = size.rstrip(b" \t")
            require(CHUNK_EXTENSIONS.fullmatch(separator + extension), "invalid chunk extension")
        require(re.fullmatch(rb"[0-9a-fA-F]{1,16}", size), "invalid chunk size")
        count = int(size, 16)
        require(count <= 65536, "duplex fixture chunk exceeds limit")
        return count

    def take(self, count):
        if not self.chunked:
            require(self.remaining is None or count <= self.remaining, "premature body end")
            result = self.wire.take(count)
            if self.remaining is not None:
                self.remaining -= count
            return result
        result = bytearray()
        while len(result) < count:
            if self.chunk_remaining == 0:
                self.chunk_remaining = self.chunk_size()
                require(self.chunk_remaining != 0, "premature chunked body end")
            amount = min(self.chunk_remaining, count - len(result))
            result.extend(self.wire.take(amount))
            self.chunk_remaining -= amount
            if self.chunk_remaining == 0:
                require(self.wire.take(2) == b"\r\n", "invalid chunk terminator")
        return bytes(result)

    def finish(self, expected_trailers=(), *, close=True):
        if self.chunked:
            require(self.chunk_remaining == 0 and self.chunk_size() == 0, "extra response body")
            lines = []
            total = 0
            while True:
                line = self.wire.until(b"\r\n", 4096)
                total += len(line)
                require(total <= 4096, "trailers exceed fixture limit")
                if line == b"\r\n":
                    break
                lines.append(line[:-2])
            require(fields(lines) == expected_trailers, "wrong duplex trailers")
        else:
            require(self.remaining in (None, 0), "incomplete response body")
            require(not expected_trailers, "trailers require chunked response framing")
        if close:
            self.wire.expect_eof()
        else:
            require(not self.closing, "response forbids connection reuse")
            require(self.chunked or self.remaining == 0, "reuse requires complete bounded framing")


def server_case(address, *, chunked, path="/echo", expect_continue=False, reuse=False):
    name = "server-duplex-chunked" if chunked else "server-duplex-length"
    row = {
        "case": name + ("-expect" if expect_continue else "") + ("-reuse" if reuse else ""),
        "exchanges": 0,
        "connections_created": 0,
    }
    wire = None
    try:
        with TraceWire.connect(address, timeout=4) as wire:
            row["connections_created"] = 1
            payloads = [(b"one", b"two"), (b"thr", b"eee"), (b"fin", b"ish")] if reuse else [(b"one", b"two")]
            for index, parts in enumerate(payloads):
                final = index == len(payloads) - 1
                framing = b"Transfer-Encoding: chunked\r\nTrailer: X-Duplex\r\n" if chunked else b"Content-Length: 6\r\n"
                wire.send(
                    b"POST " + path.encode("ascii") + b" HTTP/1.1\r\nHost: localhost\r\n"
                    + (b"Connection: close\r\n" if final else b"") + framing
                    + (b"Expect: 100-continue\r\n" if expect_continue else b"") + b"\r\n"
                )
                if expect_continue:
                    require(wire.response("POST").status == 100, "duplex Expect needs 100 before final head")
                response = ResponsePrefix(wire)
                row["response_head_before_request_body"] = True
                for part in parts:
                    wire.send(b"3\r\n" + part + b"\r\n" if chunked else part)
                    require(response.take(3) == part, "echo did not progress before the next request fragment")
                if chunked:
                    wire.send(b"0\r\nX-Duplex: done\r\n\r\n")
                response.finish(((b"x-duplex", b"done"),) if chunked else (), close=final)
                row["exchanges"] += 1
            row["passed"] = True
    except (AssertionError, OSError, ValueError) as error:
        row.update(passed=False, error=repr(error))
    row["trace"] = wire.trace if wire else []
    return row


def client_case(adapter, directory: Path, *, chunked, expect_continue=False):
    directory.mkdir(parents=True)
    payload = bytes(range(256)) * 32768
    digest = hashlib.sha256(payload).hexdigest().encode()
    prefix = b"duplex-response-start:"
    row = {
        "case": ("client-duplex-chunked" if chunked else "client-duplex-length")
        + ("-expect" if expect_continue else ""),
        "request_body_bytes": len(payload),
        "peer_verified_body_bytes": 0,
        "phase": "waiting-for-request-head",
    }

    def serve(wire, _index):
        headers = request_head(wire, "POST", "/fixture")
        row["request_head_base64"] = base64.b64encode(wire._wire).decode()
        expectations = [value.lower() for key, value in headers if key == b"expect"]
        require(
            expectations == ([b"100-continue"] if expect_continue else []),
            "wrong duplex Expect policy",
        )
        if expect_continue:
            require(not wire.buffer, "client sent buffered upload bytes before 100 Continue")
        transfer = [value.lower() for key, value in headers if key == b"transfer-encoding"]
        lengths = [value for key, value in headers if key == b"content-length"]
        require(
            transfer == [b"chunked"] and not lengths if chunked
            else not transfer and lengths == [str(len(payload)).encode()],
            "wrong upload framing",
        )
        row["request_wire_bytes_buffered_before_response_head"] = len(wire.buffer)
        row["receive_buffer_bytes"] = wire.sock.getsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF)
        early_response = (
            (b"HTTP/1.1 100 Continue\r\n\r\n" if expect_continue else b"")
            + b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n"
            b"Trailer: X-Duplex\r\nConnection: close\r\n\r\n"
            + f"{len(prefix):x}\r\n".encode() + prefix + b"\r\n"
        )
        row["early_response_base64"] = base64.b64encode(early_response).decode()
        row["phase"] = "sending-early-response"
        wire.send(early_response, chunk_size=7)
        row["phase"] = "receiving-request-body-after-early-200"
        offset = 0
        while offset < len(payload):
            if chunked:
                line = wire.until(b"\r\n", 128)[:-2]
                require(re.fullmatch(rb"[0-9a-fA-F]{1,16}", line), "invalid request chunk size")
                count = int(line, 16)
                require(0 < count <= len(payload) - offset, "premature or oversized request chunk")
            else:
                count = min(16384, len(payload) - offset)
            remaining = count
            while remaining:
                part = wire.take(min(16384, remaining))
                require(part == payload[offset : offset + len(part)], "request body corruption")
                offset += len(part)
                remaining -= len(part)
                row["peer_verified_body_bytes"] = offset
            if chunked:
                require(wire.take(2) == b"\r\n", "invalid request chunk ending")
        if chunked:
            require(wire.until(b"\r\n", 128) == b"0\r\n", "missing request terminal chunk")
            require(wire.take(2) == b"\r\n", "unexpected request trailers")
        wire.send(
            f"{len(digest):x}\r\n".encode() + digest
            + b"\r\n0\r\nX-Duplex: done\r\n\r\n"
        )
        row["phase"] = "final-response-sent"
        wire.sock.shutdown(socket.SHUT_WR)
        wire.expect_eof()

    try:
        with ScriptedPeer(serve, timeout=10) as peer:
            peer.listener.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
            request_file = directory / "request.json"
            result_file = directory / "result.json"
            request_file.write_text(json.dumps({
                "schema": 1,
                "url": f"http://{peer.address[0]}:{peer.address[1]}/fixture",
                "method": "POST",
                "headers": [["expect", "100-continue"]] if expect_continue else [],
                "body_base64": base64.b64encode(payload).decode(),
                "body_mode": "chunked" if chunked else "known-length",
                "count": 1,
                "connection": "single",
                "timeout_ms": 6000,
            }) + "\n")
            command = [
                item.replace("{request_file}", str(request_file)).replace("{result_file}", str(result_file))
                for item in adapter["command"]
            ]
            completed = run_command(command, cwd=WORKSPACE, timeout=8)
            require(completed.returncode == 0, f"adapter failed: {completed.stderr!r}")
            require(result_file.is_file() and result_file.stat().st_size <= 128 * 1024, "invalid adapter result")
            report = json.loads(result_file.read_text())
            require(isinstance(report, dict) and report.get("schema") == 1, "invalid result schema")
            require(report.get("failure") is None, f"client failed after early 200: {report.get('failure')}")
            exchanges = report.get("exchanges")
            require(isinstance(exchanges, list) and len(exchanges) == 1, "wrong exchange count")
            exchange = exchanges[0]
            require(isinstance(exchange, dict) and exchange.get("index") == 0, "invalid exchange record")
            require(exchange.get("status") == 200, "wrong response status")
            require(base64.b64decode(exchange["body_base64"], validate=True) == prefix + digest, "response body mismatch")
            trailers = exchange.get("trailers") or {}
            require(isinstance(trailers, dict), "invalid response trailer record")
            require(
                {key.lower(): value for key, value in trailers.items()} == {"x-duplex": ["done"]},
                "response trailers mismatch",
            )
        require(row["peer_verified_body_bytes"] == len(payload), "client abandoned successful duplex upload")
        row["passed"] = True
    except (AssertionError, OSError, ValueError, KeyError, TypeError) as error:
        row.update(passed=False, error=repr(error))
    return row


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--client-adapter", type=Path)
    parser.add_argument("--server-command", help="JSON argument array")
    parser.add_argument("--path", default="/echo")
    parser.add_argument("--reuse", action="store_true", help="require three gated server exchanges on one socket")
    args = parser.parse_args()
    if not args.client_adapter and not args.server_command:
        parser.error("supply a client adapter, server command, or both")
    directory = ARTIFACTS / f"duplex-{uuid.uuid4().hex}"
    directory.mkdir(parents=True)
    report = {"results": [], "scope": "Independent duplex progress, not deterministic kernel completion scheduling."}
    if args.server_command:
        command = json.loads(args.server_command)
        require(
            isinstance(command, list) and command and all(isinstance(item, str) for item in command),
            "server command must be a nonempty string array",
        )
        command = [item.replace("{bind}", "127.0.0.1:0") for item in command]
        report["server_command"] = command
        server = PeerProcess(command, cwd=WORKSPACE)
        try:
            with server:
                for expect_continue in (False, True):
                    for mode in (False, True):
                        report["results"].append(server_case(
                            server.address, chunked=mode, path=args.path,
                            expect_continue=expect_continue,
                            reuse=args.reuse,
                        ))
        except (OSError, RuntimeError) as error:
            report["startup_error"] = repr(error)
        finally:
            report["server_log"] = server.diagnostics()
    if args.client_adapter:
        adapter = load_adapter(args.client_adapter)
        require(
            {"upload", "chunked-upload", "expect-continue"} <= set(adapter["capabilities"]),
            "duplex client adapter requires upload, chunked-upload, and expect-continue capabilities",
        )
        report["adapter"] = adapter
        for expect_continue in (False, True):
            for mode in (False, True):
                name = ("chunked" if mode else "length") + ("-expect" if expect_continue else "")
                report["results"].append(client_case(
                    adapter, directory / name, chunked=mode, expect_continue=expect_continue
                ))
    output = directory / "results.json"
    output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    print(f"REPORT {output}", file=sys.stderr)
    return 0 if report["results"] and not report.get("startup_error") and all(row["passed"] for row in report["results"]) else 1


if __name__ == "__main__":
    sys.exit(main())
