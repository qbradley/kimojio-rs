"""Collect whole-run allocation counts, not instrumented throughput."""

from __future__ import annotations

import argparse
import contextlib
import hashlib
import http.client
import json
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "interop" / "http1"))
sys.path.insert(0, str(ROOT / "interop" / "websocket"))
from harness import PeerProcess, Wire
from ws_wire import Messages, check_close, check_server_upgrade, client_frame, close_payload, read_frame, upgrade_request


def finish(peer: PeerProcess) -> dict:
    code = peer.process.wait(timeout=35)
    peer.close()
    diagnostics = peer.diagnostics()
    if code != 0:
        raise RuntimeError(f"allocation target exited {code}:\n{diagnostics}")
    records = [
        json.loads(line.removeprefix("stderr: ").removeprefix("ALLOC_STATS "))
        for line in diagnostics.splitlines()
        if line.removeprefix("stderr: ").startswith("ALLOC_STATS ")
    ]
    if len(records) != 1:
        raise RuntimeError(f"expected one allocation report:\n{diagnostics}")
    counts = records[0]
    if counts["failed_calls"] or not 0 <= counts["live_bytes"] <= counts["peak_live_bytes"]:
        raise RuntimeError(f"invalid allocation result: {counts}")
    return counts


def http_case(binary: Path, count: int, payload: bytes, directory: Path, static: bool) -> tuple[list[str], dict]:
    args = (
        ["--root", str(directory), "--stop-after", str(count)]
        if static else ["--connections", "1"]
    )
    command = [str(binary), "--bind", "127.0.0.1:0", *args]
    with PeerProcess(command, cwd=ROOT) as peer:
        connection = http.client.HTTPConnection(*peer.address, timeout=10)
        try:
            for index in range(count):
                path = "/body.bin" if static else f"/bytes/{len(payload)}"
                headers = {"Connection": "close"} if index + 1 == count else {}
                connection.request("GET", path, headers=headers)
                response = connection.getresponse()
                if response.status != 200 or response.read() != payload:
                    raise RuntimeError("HTTP allocation workload returned incorrect data")
        finally:
            connection.close()
        return command, finish(peer)


def chat_case(binary: Path, count: int, payload: bytes, fanout: int, run_for_ms: int) -> tuple[list[str], dict]:
    command = [str(binary), "--bind", "127.0.0.1:0", "--run-for-ms", str(run_for_ms)]
    with PeerProcess(command, cwd=ROOT) as peer:
        with contextlib.ExitStack() as sockets:
            clients = []
            for _ in range(fanout):
                wire = sockets.enter_context(Wire.connect(peer.address, timeout=run_for_ms / 1000 + 5))
                wire.send(upgrade_request())
                check_server_upgrade(wire)
                clients.append((wire, Messages()))
            for index in range(count):
                clients[0][0].send(client_frame(2, payload))
                for wire, messages in clients:
                    event = None
                    while event is None:
                        event = messages.consume(read_frame(wire, expect_mask=False))
                    if event != ("binary", payload):
                        detail = check_close(event[1]) if event[0] == "close" else event[0]
                        raise RuntimeError(f"wrong broadcast at message {index}: {detail}")
            for wire, messages in clients:
                wire.send(client_frame(8, close_payload(1000)))
                event = messages.consume(read_frame(wire, expect_mask=False))
                if event is None or event[0] != "close" or check_close(event[1])[0] != 1000:
                    raise RuntimeError("allocation workload did not complete normal close")
                if not wire.eof and (wire.buffer or wire.sock.recv(1)):
                    raise RuntimeError("unexpected bytes after close")
        return command, finish(peer)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary-dir", type=Path, default=ROOT / "target" / "release")
    parser.add_argument("--counts", type=int, nargs="+", default=[32, 128])
    parser.add_argument("--sizes", type=int, nargs="+", default=[128, 65536])
    parser.add_argument("--fanout", type=int, default=4)
    parser.add_argument("--chat-run-for-ms", type=int, default=15000)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if not all(0 < count <= 4096 for count in args.counts):
        parser.error("counts must be between 1 and 4096")
    if not all(0 < size <= 1024 * 1024 for size in args.sizes) or not 1 <= args.fanout <= 16:
        parser.error("sizes must be 1..1048576 and fanout must be 1..16")
    if max(args.counts) * max(args.sizes) > 8 * 1024 * 1024:
        parser.error("each count/size combination must fit the 8 MiB transcript bound")
    if not 0 < args.chat_run_for_ms <= 30000:
        parser.error("chat runtime must be between 1 and 30000 ms")
    report = {
        "scope": "Whole-process Rust allocator calls; setup and shutdown included; not timing results",
        "complete": False,
        "requested": {
            "counts": args.counts,
            "sizes": args.sizes,
            "fanout": args.fanout,
            "chat_run_for_ms": args.chat_run_for_ms,
        },
        "revision": subprocess.check_output(
            ["jj", "log", "-r", "@", "--no-graph", "-T", "commit_id"], cwd=ROOT, text=True
        ).strip(),
        "rows": [],
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    with tempfile.TemporaryDirectory(prefix="allocation-fixture-", dir=ROOT / "target") as name:
        directory = Path(name)
        for size in args.sizes:
            payload = b"x" * size
            (directory / "body.bin").write_bytes(payload)
            for count in args.counts:
                for mode in ("static", "wrapper", "chat"):
                    executable = {
                        "static": "alloc-http1-static",
                        "wrapper": "alloc-http1-wrapper",
                        "chat": "alloc-websocket-chat",
                    }[mode]
                    binary = (args.binary_dir / executable).resolve(strict=True)
                    before = hashlib.sha256(binary.read_bytes()).hexdigest()
                    if mode == "chat":
                        command, counters = chat_case(binary, count, payload, args.fanout, args.chat_run_for_ms)
                    else:
                        command, counters = http_case(binary, count, payload, directory, mode == "static")
                    if hashlib.sha256(binary.read_bytes()).hexdigest() != before:
                        raise RuntimeError(f"binary changed during allocation run: {binary}")
                    report["rows"].append({
                        "target": mode,
                        "size": size,
                        "successful_operations": count,
                        "deliveries": count * (args.fanout if mode == "chat" else 1),
                        "connections": args.fanout if mode == "chat" else 1,
                        "command": command,
                        "sha256": before,
                        "counters": counters,
                    })
                    args.output.write_text(json.dumps(report, indent=2) + "\n")
                    print(f"{mode} {size} bytes x {count}: allocation report complete", flush=True)
    report["complete"] = True
    args.output.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
