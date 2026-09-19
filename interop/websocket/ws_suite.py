"""Command-driven raw WebSocket broadcast checks, independent of the Rust FSM."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys
import uuid

from ws_wire import (
    Messages, Wire, check_close, check_server_upgrade, client_frame,
    close_payload, encode_frame, read_frame, upgrade_request,
)
from harness import PeerProcess, ProtocolError


WORKSPACE = Path(__file__).resolve().parents[2]
ARTIFACTS = WORKSPACE / "target" / "interop"


def require(condition, message):
    if not condition:
        raise ProtocolError(message)


def connect(address, *, path="/chat"):
    wire = Wire.connect(address, timeout=5)
    try:
        wire.send(upgrade_request(path))
        check_server_upgrade(wire)
        return wire
    except BaseException:
        wire.sock.close()
        raise


def event(wire, messages=None):
    if messages is None:
        messages = getattr(wire, "_ws_messages", None)
        if messages is None:
            messages = Messages()
            wire._ws_messages = messages
    for _ in range(32):
        result = messages.consume(read_frame(wire, expect_mask=False))
        if result is not None:
            return result
    raise ProtocolError("too many fragments before an event")


def close(wire):
    payload = close_payload(1000, "done")
    wire.send(client_frame(8, payload))
    kind, received = event(wire)
    require(kind == "close" and check_close(received)[0] in (None, 1000), "wrong close response")
    wire.expect_eof()


def broadcast_case(address, *, include_sender: bool, path="/chat"):
    with (
        connect(address, path=path) as first,
        connect(address, path=path) as second,
        connect(address, path=path) as third,
    ):
        # Pong confirms that both peers completed application registration.
        for recipient in (second, third):
            recipient.send(client_frame(9, b"ready"))
            require(event(recipient) == ("pong", b"ready"), "missing readiness pong")
        for message in (b"first-message", b"second-message"):
            first.send(client_frame(1, message))
            for recipient in (second, third):
                require(event(recipient) == ("text", message), "broadcast recipient mismatch")
            if include_sender:
                require(event(first) == ("text", message), "broadcast sender mismatch")
        second.send(client_frame(1, b"reverse-message"))
        require(event(first) == ("text", b"reverse-message"), "reverse broadcast mismatch")
        require(event(third) == ("text", b"reverse-message"), "reverse broadcast recipient mismatch")
        if include_sender:
            require(event(second) == ("text", b"reverse-message"), "reverse sender mismatch")
        close(first)
        close(second)
        close(third)


def fragmented_case(address, *, include_sender: bool, path="/chat"):
    with connect(address, path=path) as first, connect(address, path=path) as second:
        second.send(client_frame(9, b"ready"))
        require(event(second) == ("pong", b"ready"), "missing readiness pong")
        first.send(
            client_frame(1, b"\xe2", fin=False)
            + client_frame(9, b"between-fragments")
            + client_frame(0, b"\x82\xac-text")
        )
        require(event(first) == ("pong", b"between-fragments"), "interleaved ping failed")
        require(event(second) == ("text", "\u20ac-text".encode()), "fragmented UTF-8 mismatch")
        if include_sender:
            require(event(first) == ("text", "\u20ac-text".encode()), "fragmented sender mismatch")
        close(first)
        close(second)


def coalesced_case(address, *, path="/chat"):
    with connect(address, path=path) as recipient, Wire.connect(address, timeout=5) as sender:
        recipient.send(client_frame(9, b"ready"))
        require(event(recipient) == ("pong", b"ready"), "missing readiness pong")
        sender.send(upgrade_request(path) + client_frame(1, b"upgrade-leftover"))
        check_server_upgrade(sender)
        require(event(recipient) == ("text", b"upgrade-leftover"), "upgrade bytes were lost")
        # Close without requiring sender-echo policy; drain that possible event first.
        sender.send(client_frame(8, close_payload()))
        first = event(sender)
        if first[0] == "text":
            require(first[1] == b"upgrade-leftover", "unexpected sender message")
            first = event(sender)
        require(
            first[0] == "close" and check_close(first[1])[0] in (None, 1000),
            "missing normal close after coalesced upgrade",
        )
        sender.expect_eof()
        close(recipient)


BAD_FRAMES = {
    "unmasked-client": (encode_frame(1, b"x"), 1002),
    "reserved-opcode": (client_frame(3, b"x"), 1002),
    "fragmented-ping": (client_frame(9, b"x", fin=False), 1002),
    "oversized-ping": (client_frame(9, b"x" * 126), 1002),
    "orphan-continuation": (client_frame(0, b"x"), 1002),
    "invalid-text-utf8": (client_frame(1, b"\xff"), 1007),
    "partial-close-code": (client_frame(8, b"\x03"), 1002),
    "forbidden-close-code": (client_frame(8, close_payload(1005)), 1002),
    "invalid-close-utf8": (client_frame(8, b"\x03\xe8\xff"), 1007),
    "reserved-rsv": (b"\xc1\x80abcd", 1002),
    "nonminimal-length": (b"\x81\xfe\x00\x01abcd\x19", 1002),
    "new-data-mid-fragment": (
        client_frame(1, b"first", fin=False) + client_frame(1, b"second"), 1002,
    ),
}
BAD_HANDSHAKES = {
    "handshake-wrong-version": upgrade_request().replace(b"Version: 13", b"Version: 12"),
    "handshake-short-key": upgrade_request().replace(b"dGhlIHNhbXBsZSBub25jZQ==", b"YQ=="),
    "handshake-missing-upgrade": upgrade_request().replace(b"Upgrade: websocket\r\n", b""),
    "handshake-wrong-method": upgrade_request().replace(b"GET /chat", b"POST /chat"),
}


def invalid_case(address, name, *, path="/chat"):
    raw, code = BAD_FRAMES[name]
    with connect(address, path=path) as wire:
        wire.send(raw)
        kind, payload = event(wire)
        require(kind == "close", "invalid frame did not produce close")
        require(check_close(payload)[0] == code, "wrong protocol-error close code")
        try:
            wire.send(client_frame(8, payload))
        except (BrokenPipeError, ConnectionResetError):
            pass
        wire.expect_eof(allow_reset=True)


def invalid_handshake_case(address, name, *, path="/chat"):
    raw = BAD_HANDSHAKES[name].replace(b"/chat", path.encode("ascii"))
    raw = raw.replace(b"keep-alive, Upgrade", b"Upgrade, close")
    with Wire.connect(address) as wire:
        wire.send(raw)
        response = wire.response()
        require(response.status in (400, 405, 426), "invalid handshake was not rejected")
        wire.expect_eof(allow_reset=True)


def run_suite(address, *, include_sender: bool, path="/chat"):
    cases = {
        "broadcast": lambda: broadcast_case(address, include_sender=include_sender, path=path),
        "fragment-control": lambda: fragmented_case(address, include_sender=include_sender, path=path),
        "coalesced-upgrade": lambda: coalesced_case(address, path=path),
    }
    cases.update({name: lambda name=name: invalid_case(address, name, path=path) for name in BAD_FRAMES})
    cases.update({
        name: lambda name=name: invalid_handshake_case(address, name, path=path)
        for name in BAD_HANDSHAKES
    })
    results = []
    for name, function in cases.items():
        try:
            function()
            results.append({"case": name, "passed": True})
        except (AssertionError, OSError, ValueError) as error:
            results.append({"case": name, "passed": False, "error": str(error)})
    return results


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--server-command", required=True, help="JSON argument array")
    parser.add_argument("--path", default="/chat")
    parser.add_argument("--exclude-sender", action="store_true")
    args = parser.parse_args()
    command = json.loads(args.server_command)
    require(
        isinstance(command, list) and command and all(isinstance(item, str) for item in command),
        "server command must be a nonempty string array",
    )
    command = [item.replace("{bind}", "127.0.0.1:0") for item in command]
    report = {"command": command, "include_sender": not args.exclude_sender, "results": []}
    try:
        with PeerProcess(command, cwd=WORKSPACE) as server:
            report["results"] = run_suite(
                server.address, include_sender=not args.exclude_sender, path=args.path
            )
            report["peer_log"] = server.diagnostics()
    except (OSError, RuntimeError) as error:
        report["startup_error"] = str(error)
    ARTIFACTS.mkdir(parents=True, exist_ok=True)
    output = ARTIFACTS / f"websocket-{uuid.uuid4().hex}.json"
    output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    print(f"REPORT {output}", file=sys.stderr)
    return 0 if report["results"] and all(row["passed"] for row in report["results"]) else 1


if __name__ == "__main__":
    sys.exit(main())
