"""Bounded application-contract checks for the command-supplied native chat server."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import socket
import struct
import subprocess
import sys
import threading
import uuid

from ws_suite import (
    ARTIFACTS, BAD_HANDSHAKES, WORKSPACE, close, connect, event, require,
)
from ws_wire import (
    Wire, check_close, check_server_upgrade, client_frame, upgrade_request,
)
from harness import PeerProcess, run_command


def ready(wire):
    wire.send(client_frame(9, b"ready"))
    require(event(wire) == ("pong", b"ready"), "missing readiness pong")


def rejected(wire, code):
    kind, payload = event(wire)
    require(kind == "close" and check_close(payload)[0] == code, f"expected close {code}")
    try:
        wire.send(client_frame(8, payload))
    except (BrokenPipeError, ConnectionResetError):
        pass
    wire.expect_eof(allow_reset=True)


def strict_handshakes(address):
    observations = {}
    for name, raw in BAD_HANDSHAKES.items():
        with Wire.connect(address, timeout=3) as wire:
            wire.send(raw.replace(b"keep-alive, Upgrade", b"Upgrade, close"))
            response = wire.response()
            expected = 426 if name == "handshake-wrong-version" else 400
            require(response.status == expected, f"{name}: expected {expected}, got {response.status}")
            if expected == 426:
                require(response.values(b"sec-websocket-version") == [b"13"], "missing supported version")
            wire.expect_eof(allow_reset=True)
            observations[name] = response.status
    return observations


def binary_boundaries(address):
    with connect(address) as sender, connect(address) as receiver:
        ready(receiver)
        for size in (0, 1, 125, 126, 65535, 65536):
            payload = (bytes(range(256)) * ((size + 255) // 256))[:size]
            sender.send(client_frame(2, payload))
            for peer in (sender, receiver):
                require(event(peer) == ("binary", payload), f"binary broadcast mismatch at {size}")
        sender.send(client_frame(1, b""))
        for peer in (sender, receiver):
            require(event(peer) == ("text", b""), "empty text changed opcode or payload")
        close(sender)
        close(receiver)
    return {"binary_lengths": [0, 1, 125, 126, 65535, 65536], "empty_text": True}


def validated_only(address):
    with connect(address) as receiver, connect(address) as sender:
        ready(receiver)
        sender.send(client_frame(1, b"not-a-message-yet", fin=False))
        ready(sender)
        sender.send(client_frame(0, b"\xff"))
        rejected(sender, 1007)
        receiver.send(client_frame(1, b"after-invalid-message"))
        require(event(receiver) == ("text", b"after-invalid-message"), "invalid or partial text was broadcast")
        close(receiver)
    return {"invalid_fragment_close": 1007, "recipient_sentinel_exact": True}


def concurrent_publishers(address):
    with connect(address) as first, connect(address) as second, connect(address) as observer:
        for peer in (first, second, observer):
            ready(peer)
        barrier = threading.Barrier(3, timeout=2)
        failures = []

        def publish(wire, name):
            try:
                barrier.wait()
                for index in range(4):
                    wire.send(client_frame(1, f"{name}:{index}".encode()))
            except (OSError, AssertionError, threading.BrokenBarrierError) as error:
                failures.append(repr(error))

        workers = [
            threading.Thread(target=publish, args=(first, "first"), daemon=True),
            threading.Thread(target=publish, args=(second, "second"), daemon=True),
        ]
        try:
            for worker in workers:
                worker.start()
            barrier.wait()
        finally:
            for worker in workers:
                if worker.ident is not None:
                    worker.join(timeout=5)
            require(not any(worker.is_alive() for worker in workers), "publisher thread survived deadline")
        require(not failures, f"publisher failed: {failures}")
        observed = [[event(peer) for _ in range(8)] for peer in (first, second, observer)]
        require(observed[0] == observed[1] == observed[2], "recipients disagree on broadcast order")
        for name in ("first", "second"):
            selected = [payload for kind, payload in observed[0] if kind == "text" and payload.startswith(name.encode() + b":")]
            require(selected == [f"{name}:{index}".encode() for index in range(4)], "publisher order or count mismatch")
        for peer in (first, second, observer):
            close(peer)
    return {"publishers": 2, "messages_each": 4, "recipients": 3, "common_order": [payload.decode() for _, payload in observed[0]]}


def bytewise_upgrade_and_frames(address):
    with Wire.connect(address, timeout=5) as wire:
        wire.send(upgrade_request(), chunk_size=1)
        check_server_upgrade(wire)
        payload = "bytewise-\u20ac".encode()
        wire.send(client_frame(1, payload), chunk_size=1)
        require(event(wire) == ("text", payload), "bytewise exchange mismatch")
        close(wire)
    return {"write_chunk_bytes": 1, "scope": "TCP may coalesce writes; not forced kernel read boundaries."}


def healthy(address):
    with connect(address) as wire:
        wire.send(client_frame(1, b"healthy"))
        require(event(wire) == ("text", b"healthy"), "healthy connection failed")
        close(wire)


def message_limits(address):
    with connect(address) as wire:
        wire.send(client_frame(2, b"x" * 1024))
        require(event(wire) == ("binary", b"x" * 1024), "exact configured limit failed")
        close(wire)
    with connect(address) as wire:
        wire.send(client_frame(2, b"x" * 1025))
        rejected(wire, 1009)
    with connect(address) as wire:
        wire.send(client_frame(2, b"x" * 512, fin=False) + client_frame(0, b"x" * 513))
        rejected(wire, 1009)
    healthy(address)
    return {"limit_bytes": 1024, "exact_limit_accepted": True, "single_and_fragmented_overflow": 1009}


def reset_recovery(address, process):
    healthy(address)
    baseline = len(os.listdir(f"/proc/{process.pid}/fd"))
    samples = []
    for _ in range(3):
        for upgraded in (False, True):
            with Wire.connect(address, timeout=3) as wire:
                if upgraded:
                    wire.send(upgrade_request())
                    check_server_upgrade(wire)
                    wire.send(client_frame(2, b"x" * 1024)[:20])
                else:
                    wire.send(b"GET /chat HTTP/1.1\r\nHost: local")
                wire.sock.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
            healthy(address)
            count = len(os.listdir(f"/proc/{process.pid}/fd"))
            require(count <= baseline + 2, "descriptor growth exceeded settle allowance")
            require(process.poll() is None, "server exited after reset")
            samples.append(count)
    return {"baseline_fds": baseline, "recovery_fds": samples, "descriptor_allowance": 2}


def slow_recipient(address):
    with connect(address) as sender, connect(address) as fast, connect(address) as slow:
        ready(fast)
        ready(slow)
        slow.sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
        payloads = []
        for index in range(12):
            payload = index.to_bytes(4, "big") + b"x" * (48 * 1024 - 4)
            payloads.append(payload)
            sender.send(client_frame(2, payload))
            for peer in (sender, fast):
                require(event(peer) == ("binary", payload), "slow recipient disrupted a healthy recipient")
        delivered = 0
        for _ in range(13):
            kind, payload = event(slow)
            if kind == "close":
                require(check_close(payload)[0] == 1008, "slow-recipient policy did not close 1008")
                try:
                    slow.send(client_frame(8, payload))
                except (BrokenPipeError, ConnectionResetError):
                    pass
                slow.expect_eof(allow_reset=True)
                break
            require(
                kind == "binary" and delivered < len(payloads) and payload == payloads[delivered],
                "slow recipient saw corrupt or reordered data",
            )
            delivered += 1
        else:
            raise AssertionError("slow recipient was not evicted")
        close(sender)
        close(fast)
    healthy(address)
    return {"published_messages": 12, "healthy_recipients": 2, "slow_messages_before_close": delivered, "close": 1008}


def shutdown(address, process):
    with connect(address) as wire:
        wire.send(client_frame(1, b"before-shutdown"))
        require(event(wire) == ("text", b"before-shutdown"), "pre-shutdown exchange failed")
        kind, payload = event(wire)
        require(kind == "close" and check_close(payload)[0] in (1000, 1001), "missing normal shutdown close")
        wire.send(client_frame(8, payload))
        wire.expect_eof()
    require(process.wait(timeout=3) == 0, "shutdown did not exit successfully")
    return {"exit": 0, "close": check_close(payload)[0], "process_wait_bound_seconds": 3}


def node_case(address):
    node = shutil.which("node")
    if node is None:
        raise RuntimeError("Node with built-in WebSocket is required")
    completed = run_command(
        [node, "interop/websocket/node_peer.js", f"ws://{address[0]}:{address[1]}/chat", "native-node-\u20ac"],
        cwd=WORKSPACE, timeout=7,
    )
    require(completed.returncode == 0, f"Node peer failed: {completed.stderr.decode(errors='replace')}")
    result = json.loads(completed.stdout)
    require(result["received"] and result["clean"] and result["code"] == 1000, "Node close was not clean")
    return result


def go_case(address, executable):
    completed = run_command(
        [str(executable), "--address", f"{address[0]}:{address[1]}"],
        cwd=WORKSPACE, timeout=7,
    )
    require(completed.returncode == 0, f"Go peer failed: {completed.stderr.decode(errors='replace')}")
    result = json.loads(completed.stdout)
    require(result["binary_bytes"] == 65792 and result["pong"] and result["clean_eof"], "Go peer incomplete")
    return result


def run_suite(command, go_peer):
    cases = [
        ("strict-handshakes", strict_handshakes, [], False),
        ("binary-boundaries", binary_boundaries, [], False),
        ("validated-message-only", validated_only, [], False),
        ("concurrent-publishers", concurrent_publishers, [], False),
        ("bytewise-upgrade-and-frames", bytewise_upgrade_and_frames, [], False),
        ("node-built-in", node_case, [], False),
        ("go-standard-library", lambda address: go_case(address, go_peer), [], False),
        ("message-limits", message_limits, ["--max-message-bytes", "1024"], False),
        ("reset-recovery", reset_recovery, [], True),
        ("slow-recipient", slow_recipient, [
            "--max-message-bytes", "65536", "--max-queued-messages", "2",
            "--max-client-bytes", "262144", "--send-buffer-bytes", "4096",
            "--close-timeout-ms", "5000",
        ], False),
        ("bounded-shutdown", shutdown, ["--run-for-ms", "1000", "--shutdown-grace-ms", "1000"], True),
    ]
    results = []
    for name, function, options, with_process in cases:
        actual = [*command, "--bind", "127.0.0.1:0", *options]
        row = {"case": name, "command": actual}
        try:
            with PeerProcess(actual, cwd=WORKSPACE) as peer:
                if with_process:
                    row["observations"] = function(peer.address, peer.process)
                else:
                    row["observations"] = function(peer.address)
                row["peer_log"] = peer.diagnostics()
            row["passed"] = True
        except (AssertionError, OSError, RuntimeError, ValueError, subprocess.TimeoutExpired) as error:
            row.update(passed=False, error=repr(error))
        results.append(row)
    return results


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--server-command", required=True, help="JSON executable/argument prefix; suite appends bind and limit flags")
    parser.add_argument("--publication", type=Path, help="owner source/binary identity JSON")
    parser.add_argument("--go-peer", type=Path, default=ARTIFACTS / "websocket-peer", help="required standard-library Go peer executable")
    args = parser.parse_args()
    command = json.loads(args.server_command)
    require(isinstance(command, list) and command and all(isinstance(item, str) for item in command), "invalid command array")
    executable = Path(shutil.which(command[0]) or command[0])
    digest = hashlib.sha256(executable.read_bytes()).hexdigest()
    report = {"matrix": "native-chat-contract-v1", "binary_sha256": digest}
    if args.publication:
        report["publication"] = json.loads(args.publication.read_text())
        require(report["publication"]["binary_sha256"] == digest, "publication hash mismatch")
    report["results"] = run_suite(command, args.go_peer.resolve())
    require(hashlib.sha256(executable.read_bytes()).hexdigest() == digest, "executable changed during run")
    ARTIFACTS.mkdir(parents=True, exist_ok=True)
    output = ARTIFACTS / f"websocket-chat-{uuid.uuid4().hex}.json"
    output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    print(f"REPORT {output}", file=sys.stderr)
    return 0 if all(row["passed"] for row in report["results"]) else 1


if __name__ == "__main__":
    sys.exit(main())
