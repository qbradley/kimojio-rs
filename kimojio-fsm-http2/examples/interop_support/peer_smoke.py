"""Independent python-h2 smoke checks; pass the native fixture binary path.

Run from the worktree root. Scratch JSON stays under target/fixture-checks.
This is a correctness check, not a performance measurement.
"""

import hashlib
import json
from pathlib import Path
import socket
import subprocess
import sys
import threading

import h2.config
import h2.connection
import h2.events
import h2.settings


BINARY = str(Path(sys.argv[1]).resolve())
SCRATCH = Path("target/fixture-checks")
SCRATCH.mkdir(parents=True, exist_ok=True)
LARGE = 16 * 1024 * 1024 + 17


def digest(byte, length):
    result = hashlib.sha256()
    while length:
        n = min(16384, length)
        result.update(bytes([byte]) * n)
        length -= n
    return result.hexdigest()


def python_client(requests, concurrency, window):
    process = subprocess.Popen(
        [BINARY, "server", "--stream-window", str(window)],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )
    try:
        line = process.stdout.readline().strip()
        assert line.startswith("LISTEN 127.0.0.1:"), line
        port = int(line.rsplit(":", 1)[1])
        with socket.create_connection(("127.0.0.1", port), timeout=20) as sock:
            conn = h2.connection.H2Connection(
                # h2 4.3's outbound validator requires :path for classic CONNECT.
                config=h2.config.H2Configuration(
                    header_encoding="utf-8", validate_outbound_headers=False
                )
            )
            conn.initiate_connection()
            conn.update_settings({h2.settings.SettingCodes.INITIAL_WINDOW_SIZE: window})
            sock.sendall(conn.data_to_send())
            while True:
                data = sock.recv(65536)
                assert data, "EOF during SETTINGS exchange"
                events = conn.receive_data(data)
                sock.sendall(conn.data_to_send())
                if any(isinstance(e, h2.events.RemoteSettingsChanged) for e in events):
                    break
            next_request = 0
            active = {}
            completed = 0
            while completed < len(requests):
                while len(active) < concurrency and next_request < len(requests):
                    request = requests[next_request]
                    sid = next_request * 2 + 1
                    method, path, upload, expected, trailers, info, status = request
                    headers = [(":method", method), (":authority", "localhost")]
                    if method != "CONNECT":
                        headers += [(":scheme", "http"), (":path", path)]
                    conn.send_headers(sid, headers, end_stream=upload == 0)
                    active[sid] = {
                        "request": request, "remaining": upload, "bytes": 0,
                        "hash": hashlib.sha256(), "trailers": [], "info": [],
                        "status": None, "ended": False,
                    }
                    next_request += 1
                for sid, state in active.items():
                    n = min(
                        state["remaining"], conn.local_flow_control_window(sid),
                        conn.max_outbound_frame_size,
                    )
                    if n > 0:
                        state["remaining"] -= n
                        conn.send_data(sid, bytes([sid % 251]) * n,
                                       end_stream=state["remaining"] == 0)
                data = conn.data_to_send()
                if data:
                    sock.sendall(data)
                data = sock.recv(65536)
                assert data, "premature EOF"
                for event in conn.receive_data(data):
                    if isinstance(event, h2.events.ResponseReceived):
                        state = active[event.stream_id]
                        state["status"] = int(dict(event.headers)[":status"])
                        if state["request"][1] == "/early":
                            assert state["remaining"], "response was not early"
                    elif isinstance(event, h2.events.InformationalResponseReceived):
                        active[event.stream_id]["info"].append(
                            int(dict(event.headers)[":status"])
                        )
                    elif isinstance(event, h2.events.TrailersReceived):
                        active[event.stream_id]["trailers"] = event.headers
                    elif isinstance(event, h2.events.DataReceived):
                        state = active[event.stream_id]
                        state["bytes"] += len(event.data)
                        state["hash"].update(event.data)
                        conn.acknowledge_received_data(
                            event.flow_controlled_length, event.stream_id
                        )
                    elif isinstance(event, h2.events.StreamEnded):
                        active[event.stream_id]["ended"] = True
                    elif isinstance(event, (h2.events.StreamReset, h2.events.ConnectionTerminated)):
                        raise AssertionError(event)
                for sid, state in list(active.items()):
                    if state["ended"] and state["remaining"] == 0:
                        _, _, _, expected, trailers, info, status = state["request"]
                        assert state["bytes"] == expected, state
                        assert state["hash"].hexdigest() == digest(sid % 251, expected)
                        assert state["trailers"] == trailers, state
                        assert state["info"] == info, state
                        assert state["status"] == status, state
                        del active[sid]
                        completed += 1
            sock.sendall(conn.data_to_send())
    finally:
        process.terminate()
        process.wait(timeout=5)
        stderr = process.stderr.read()
        assert not stderr, stderr


def python_server(requests, concurrency, window, finish_response_early=False):
    listener = socket.socket()
    listener.bind(("127.0.0.1", 0))
    listener.listen()
    errors = []

    def serve():
        try:
            with listener, listener.accept()[0] as sock:
                sock.settimeout(25)
                conn = h2.connection.H2Connection(
                    config=h2.config.H2Configuration(
                        client_side=False, header_encoding="utf-8"
                    )
                )
                # RFC 9113 forbids a server from emitting SETTINGS_ENABLE_PUSH.
                del conn.local_settings[h2.settings.SettingCodes.ENABLE_PUSH]
                conn.initiate_connection()
                conn.update_settings({h2.settings.SettingCodes.INITIAL_WINDOW_SIZE: window})
                sock.sendall(conn.data_to_send())
                pending = {}
                uploads = {}
                received_end = set()
                while True:
                    data = sock.recv(65536)
                    if not data:
                        for sid, (count, _) in uploads.items():
                            if count != requests[sid // 2]["body_bytes"]:
                                errors.append(f"upload {sid}: {count} bytes; pending responses {pending}")
                        return
                    for event in conn.receive_data(data):
                        if isinstance(event, h2.events.RequestReceived):
                            sid = event.stream_id
                            request = requests[sid // 2]
                            length = request["expected"]
                            response = [(":status", "200"), ("content-length", str(length))]
                            ended = length == 0 and (
                                request["body_bytes"] == 0 or finish_response_early
                            )
                            conn.send_headers(sid, response, end_stream=ended)
                            if not ended:
                                pending[sid] = length
                            uploads[sid] = [0, hashlib.sha256()]
                        elif isinstance(event, h2.events.DataReceived):
                            uploads[event.stream_id][0] += len(event.data)
                            uploads[event.stream_id][1].update(event.data)
                            if event.flow_controlled_length:
                                conn.increment_flow_control_window(event.flow_controlled_length)
                                conn.increment_flow_control_window(
                                    event.flow_controlled_length, event.stream_id
                                )
                        elif isinstance(event, h2.events.StreamEnded):
                            sid = event.stream_id
                            received_end.add(sid)
                            expected = requests[sid // 2]["body_bytes"]
                            assert uploads[sid][0] == expected
                            assert uploads[sid][1].hexdigest() == digest(sid % 251, expected)
                        elif isinstance(event, h2.events.ConnectionTerminated):
                            assert event.error_code == 0, event
                    for sid in list(pending):
                        if pending[sid] == 0:
                            if sid in received_end or finish_response_early:
                                conn.end_stream(sid)
                                del pending[sid]
                            continue
                        while pending[sid]:
                            n = min(pending[sid], conn.local_flow_control_window(sid),
                                    conn.max_outbound_frame_size)
                            if not finish_response_early and sid not in received_end and n == pending[sid]:
                                n -= 1
                            if not n:
                                break
                            pending[sid] -= n
                            conn.send_data(sid, bytes([sid % 251]) * n,
                                           end_stream=pending[sid] == 0)
                        if not pending[sid]:
                            del pending[sid]
                    data = conn.data_to_send()
                    if data:
                        sock.sendall(data)
        except Exception as error:
            errors.append(repr(error))

    port = listener.getsockname()[1]
    thread = threading.Thread(target=serve)
    thread.start()
    request = {
        "schema": 1, "host": "127.0.0.1", "port": port, "timeout_ms": 20000,
        "config": {"stream_window": window}, "request_count": len(requests),
        "concurrency": concurrency,
        "requests": [{k: v for k, v in r.items() if k != "expected"} for r in requests],
        "actions": [],
    }
    source, result = SCRATCH / "request.json", SCRATCH / "result.json"
    source.write_text(json.dumps(request))
    run = subprocess.run([BINARY, "client", str(source), str(result)],
                         capture_output=True, text=True, timeout=25)
    thread.join(timeout=22)
    assert not thread.is_alive(), "peer watchdog"
    assert run.returncode == 0, (run.stderr, errors)
    assert not errors, (errors, run.stderr)
    report = json.loads(result.read_text())
    assert report["connection"] == {
        "error": None, "outcome": "graceful", "closed": True
    }, report
    assert len(report["streams"]) == len(requests)
    for stream, request in zip(report["streams"], requests):
        assert stream["ended"] and stream["error"] is None, stream
        assert stream["outcome"] == "complete", stream
        assert stream["bytes"] == request["expected"], stream
        assert stream["sha256"] == digest(stream["stream_id"] % 251, request["expected"])
    source.unlink()
    result.unlink()


def run():
    def req(method, path, upload, expected, trailers=None, info=None, status=200):
        return method, path, upload, expected, trailers or [], info or [], status

    for window in [1024, 65535]:
        python_client([req("GET", "/bytes/131071", 0, 131071)], 1, window)
        python_server([{"method": "POST", "path": "/", "body_bytes": 131071,
                        "expected": 131071}], 1, window)
        print(f"PASS both directions 131071 bytes, window={window}", flush=True)
    python_client([
        req("GET", "/trailers/37", 0, 37, [("x-end", "done")]),
        req("GET", "/informational/37", 0, 37, info=[103]),
        req("HEAD", "/bytes/131071", 0, 0),
        req("GET", "/no-content", 0, 0, status=204),
        req("POST", "/early", 131071, 0, status=413),
    ], 5, 1024)
    print("PASS trailers/informational/HEAD/204/early", flush=True)
    for method, path in [("POST", "/echo"), ("CONNECT", "")]:
        python_client([req(method, path, LARGE, LARGE)], 1, 65535)
        print(f"PASS {method} streaming echo {LARGE} bytes", flush=True)
    python_client([req("GET", f"/bytes/{LARGE}", 0, LARGE)], 1, 65535)
    python_server([{"method": "POST", "path": "/", "body_bytes": LARGE,
                    "expected": LARGE}], 1, 65535)
    print(f"PASS both directions large body {LARGE}", flush=True)
    for concurrency in [1, 16]:
        python_client([req("GET", "/bytes/32768", 0, 32768)] * 600, concurrency, 65535)
        python_server([{"method": "POST", "path": "/", "body_bytes": 32768,
                        "expected": 32768}] * 600, concurrency, 65535)
        print(f"PASS both directions 600*32768, concurrency={concurrency}", flush=True)


if __name__ == "__main__":
    run()
