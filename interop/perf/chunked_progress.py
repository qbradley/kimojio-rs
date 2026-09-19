"""Untimed raw-wire progress barriers for the large chunked echo fixtures."""

import argparse
import base64
import hashlib
import json
from pathlib import Path
import uuid

from run import ARTIFACTS, ROOT, digest, load_manifest
from duplex_suite import ResponsePrefix
from harness import PeerProcess, Wire


def probe(address, size):
    payload = bytes((index * 31 + index // 4096) % 256 for index in range(size))
    head = (
        b"POST /echo HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n"
        b"Transfer-Encoding: chunked\r\n\r\n"
    )
    with Wire.connect(address, timeout=5) as wire:
        wire.send(head)
        response = ResponsePrefix(wire)
        if not response.chunked:
            raise AssertionError("early echo response is not chunked")
        offset = 0
        barriers = []
        while offset < size:
            amount = min(1024 if offset == 0 else 16384, size - offset)
            part = payload[offset:offset + amount]
            wire.send(f"{amount:x}\r\n".encode() + part + b"\r\n")
            if response.take(amount) != part:
                raise AssertionError(f"echo payload mismatch at {offset}")
            offset += amount
            barriers.append(offset)
        wire.send(b"0\r\n\r\n")
        response.finish()
    return {
        "bytes": size, "payload_formula": "(index*31+index//4096)%256",
        "payload_sha256": hashlib.sha256(payload).hexdigest(),
        "request_head_base64": base64.b64encode(head).decode(),
        "response_head_base64": base64.b64encode(response.head).decode(),
        "response_head_before_any_upload": True,
        "exact_echo_prefix_before_next_upload": barriers,
        "measurement_payload_note": "This untimed correctness corpus varies; timed uploads remain ASCII x only.",
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", required=True, type=Path)
    args = parser.parse_args()
    identity = json.loads((ARTIFACTS / "tool-publication.json").read_text())
    manifest = load_manifest(args.manifest, identity)
    directory = ARTIFACTS / f"chunked-progress-{uuid.uuid4().hex}"
    directory.mkdir()
    report = {"purpose": "UNTIMED_STREAMING_CORRECTNESS_BARRIERS", "manifest": manifest, "results": []}
    for target in manifest["targets"]:
        command = [part.replace("{bind}", "127.0.0.1:0").replace("{root}", str(directory))
                   for part in target["command"]]
        for size in (65536, 1048576):
            row = {"target": target["name"], "command": command, "bytes": size}
            try:
                with PeerProcess(command, cwd=ROOT) as server:
                    row["observations"] = probe(server.address, size)
                if digest(target["binary"]) != target["sha256"]:
                    raise AssertionError("target binary changed")
                row["passed"] = True
            except (AssertionError, OSError, RuntimeError, ValueError) as error:
                row.update(passed=False, error=repr(error))
            report["results"].append(row)
            print(target["name"], size, row["passed"], row.get("error", ""))
    output = directory / "results.json"
    output.write_text(json.dumps(report, indent=2) + "\n")
    print(f"REPORT {output}")
    return 0 if all(row["passed"] for row in report["results"]) else 1


if __name__ == "__main__":
    raise SystemExit(main())
