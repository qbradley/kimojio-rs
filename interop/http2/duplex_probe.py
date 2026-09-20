"""Diagnose an early final response without peer closure or reset."""

from __future__ import annotations

import argparse
from pathlib import Path
import sys
import time
import uuid

from h2.events import DataReceived, RequestReceived, StreamEnded

from peer import Peer, Received, digest_for
from socket_peer import Channel, ScriptedPeer, prerequisites, require, run_command
from suite import ROOT, adapter, command, read_json, write_json

BODY = 131087


def run_probe(client_adapter, directory, *, consume=True, timeout_ms=8000):
    prerequisites()
    directory.mkdir(parents=True)
    observations = []

    def handler(wire, _index):
        peer = Peer(client=False)
        channel = Channel(wire.sock, peer, timeout=timeout_ms / 1000 + 4, consume=False)
        started = time.monotonic()
        events_log = []
        response_sent_before_request_end = False
        failure = None
        native_socket_close = None

        def record(name, **fields):
            require(len(events_log) < 256, "probe event limit exceeded")
            events_log.append({"event": name, "elapsed_ms": round((time.monotonic() - started) * 1000, 3), **fields})

        try:
            channel.handshake()
            record("handshake")
            while not channel.eof:
                for event in channel.step():
                    if isinstance(event, RequestReceived):
                        require(event.stream_id == 1, "probe needs exactly stream 1")
                        require(dict(event.headers)[b":method"] == b"POST", "probe needs POST")
                        received = peer.received.get(1, Received())
                        response_sent_before_request_end = not received.ended
                        peer.connection.send_headers(
                            1, [(b":status", b"200"), (b"content-length", b"0")], end_stream=True,
                        )
                        record("response-end-queued", upload_bytes=received.payload, upload_ended=received.ended)
                    elif isinstance(event, DataReceived):
                        require(event.data == b"\x01" * len(event.data), "upload payload mismatch")
                        if consume:
                            peer.consume(1, event.flow_controlled_length)
                        record("upload-data", payload=len(event.data), flow=event.flow_controlled_length)
                    elif isinstance(event, StreamEnded):
                        record("upload-end")
            record("native-eof")
            native_socket_close = "eof"
        except (ConnectionResetError, BrokenPipeError) as error:
            native_socket_close = "reset" if isinstance(error, ConnectionResetError) else "write-rejected"
            record("native-transport-close", kind=native_socket_close)
        except Exception as error:
            failure = f"{type(error).__name__}: {error}"
        finally:
            received = peer.received.get(1, Received())
            observations.append({
                "received_bytes": received.payload, "received_flow": received.flow,
                "received_sha256": received.digest.hexdigest(), "request_ended": received.ended,
                "native_eof": channel.eof,
                "native_socket_close": native_socket_close,
                "response_end_before_request_end": response_sent_before_request_end,
                "window_updates_sent": dict(channel.outbound.updates),
                "peer_resets": channel.outbound.resets, "peer_goaway": channel.outbound.goaway,
                "native_resets": channel.inbound.resets, "native_goaway": channel.inbound.goaway,
                "peer_error": failure, "events": events_log,
            })
            channel.close()

    command_failure = None
    completed = None
    result = None
    with ScriptedPeer(handler, timeout=timeout_ms / 1000 + 6) as server:
        request_file, result_file = directory / "request.json", directory / "result.json"
        write_json(request_file, {
            "schema": 1, "host": server.address[0], "port": server.address[1],
            "timeout_ms": timeout_ms, "config": {}, "request_count": 1, "concurrency": 1,
            "requests": [{
                "method": "POST", "path": "/early-success-drain",
                "body_bytes": BODY, "trailers": [],
            }],
            "actions": [],
        })
        try:
            completed = run_command(
                ["env", "H2_FIXTURE_TRACE=1", *command(client_adapter, request_file, result_file)],
                cwd=ROOT, timeout=timeout_ms / 1000 + 3,
            )
            if result_file.is_file():
                result = read_json(result_file)
        except Exception as error:
            command_failure = f"{type(error).__name__}: {error}"
    require(len(observations) == 1, "probe peer did not return observations")
    peer = observations[0]
    streams = result.get("streams", []) if result else []
    stream = streams[0] if len(streams) == 1 else {}
    response_ok = (
        result is not None and result.get("schema") == 1
        and result.get("connection") == {"closed": True, "error": None}
        and stream.get("stream_id") == 1 and stream.get("status") == 200
        and stream.get("content_length") == 0 and stream.get("bytes") == 0
        and stream.get("sha256") == digest_for(1, 0) and stream.get("ended") is True
        and stream.get("error") is None and stream.get("trailers") == []
        and stream.get("informational") == []
    )
    peer_clean = (
        peer["peer_error"] is None and peer["native_socket_close"] is not None
        and not peer["peer_resets"] and not peer["peer_goaway"]
    )
    passed = (
        completed is not None and completed.returncode == 0 and command_failure is None
        and response_ok and peer_clean and not peer["native_resets"]
        and peer["response_end_before_request_end"]
        and peer["received_bytes"] == BODY and peer["request_ended"]
        and peer["received_sha256"] == digest_for(1, BODY)
        and peer["window_updates_sent"].get(0, 0) > 0
        and peer["window_updates_sent"].get(1, 0) > 0
    )
    if passed:
        classification = "completed-upload-after-final-response"
    elif not consume:
        classification = "no-credit-negative-control"
    elif not peer_clean:
        classification = "peer-or-transport-failure-not-classified-as-core"
    elif peer["native_resets"]:
        classification = "native-reset-observed"
    else:
        classification = "native-failure-with-peer-open-and-no-rst-stream"
    report = {
        "schema": 1, "passed": passed, "classification": classification,
        "consume": consume, "expected_upload_bytes": BODY, "peer": peer,
        "command_failure": command_failure,
        "exit_code": completed.returncode if completed else None,
        "stdout": completed.stdout.decode(errors="replace") if completed else "",
        "stderr": completed.stderr.decode(errors="replace") if completed else "",
        "result": result,
    }
    write_json(directory / "report.json", report)
    return report


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--client-adapter", type=Path)
    parser.add_argument("--selftest", action="store_true")
    parser.add_argument("--withhold-credit", action="store_true")
    parser.add_argument("--output", type=Path, default=ROOT / "target" / "http2-duplex-probe" / uuid.uuid4().hex)
    args = parser.parse_args()
    require(args.selftest != bool(args.client_adapter), "select selftest or a client adapter")
    directory = args.output.resolve()
    require("target" in directory.parts, "reports must reside under target")
    if args.selftest:
        client_adapter = {
            "command": [sys.executable, str(Path(__file__).with_name("reference.py")), "client", "{request_file}", "{result_file}"],
        }
    else:
        client_adapter = adapter(args.client_adapter, client_role=True)
    report = run_probe(client_adapter, directory, consume=not args.withhold_credit)
    print(report["classification"], directory / "report.json")
    require(report["passed"], "duplex probe failed; inspect the bounded peer observations and fixture trace")


if __name__ == "__main__":
    main()
