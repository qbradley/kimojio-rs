"""Focused wrapper startup diagnostics, never a substitute for suite qualification."""

from __future__ import annotations

import argparse
from collections import defaultdict
from pathlib import Path

from h2.settings import SettingCodes

from cases import Case
from peer import Peer
from profiles import trailer_values
from protocol_suite import request as protocol_request
from reference import serve
from socket_peer import PREFACE, PeerProcess, ScriptedPeer, prerequisites, require, run_command
from suite import ROOT, command, read_json, write_json


class RecordingSocket:
    """Record bounded frame metadata before the protocol library can reject input."""

    def __init__(self, sock):
        self.sock = sock
        self.buffers = {"in": bytearray(), "out": bytearray()}
        self.preface = True
        self.events = []
        self.bytes = defaultdict(int)
        self.flow = defaultdict(int)
        self.before_ack = defaultdict(int)
        self.settings_acks = 0
        self.frames = 0
        self.replay = bytearray()

    def __getattr__(self, name):
        return getattr(self.sock, name)

    def feed(self, direction, data):
        self.bytes[direction] += len(data)
        require(self.bytes[direction] <= 4 * 1024 * 1024, "diagnostic wire limit exceeded")
        buf = self.buffers[direction]
        buf.extend(data)
        if direction == "in" and self.preface:
            if len(buf) < len(PREFACE):
                return
            require(buf[:24] == PREFACE, "invalid preface")
            del buf[:24]
            self.preface = False
        while len(buf) >= 9:
            length = int.from_bytes(buf[:3], "big")
            require(length <= 65536, "diagnostic frame too large")
            if len(buf) < 9 + length:
                break
            kind, flags = buf[3:5]
            stream = int.from_bytes(buf[5:9], "big") & 0x7fffffff
            self.frames += 1
            require(self.frames <= 20000, "diagnostic frame count exceeded")
            event = {"direction": direction, "type": kind, "flags": flags,
                     "stream": stream, "length": length, "prior_settings_acks": self.settings_acks}
            if kind == 4 and not flags & 1:
                event["settings"] = {
                    int.from_bytes(buf[i:i + 2], "big"): int.from_bytes(buf[i + 2:i + 6], "big")
                    for i in range(9, 9 + length, 6)
                }
            if direction == "in" and kind == 4 and flags & 1:
                self.settings_acks += 1
            if direction == "in" and kind == 0:
                self.flow[stream] += length
                if not self.settings_acks:
                    self.before_ack[stream] += length
            if len(self.events) < 128:
                self.events.append(event)
            del buf[:9 + length]
        require(len(buf) <= 65545, "diagnostic parser buffer exceeded")

    def recv(self, size):
        if self.replay:
            data = bytes(self.replay[:size])
            del self.replay[:size]
            return data
        data = self.sock.recv(size)
        self.feed("in", data)
        return data

    def send(self, data):
        sent = self.sock.send(data)
        self.feed("out", data[:sent])
        return sent

    def report(self):
        return {"events": self.events, "bytes": dict(self.bytes), "frames": self.frames,
                "settings_acks": self.settings_acks, "flow": dict(self.flow),
                "flow_before_first_settings_ack": dict(self.before_ack)}


def echo_probe(adapter, directory, mode):
    directory.mkdir(parents=True)
    observations = []
    config = {"stream_window": 1024, "connection_window": 65535}

    def handler(wire, _):
        sock = RecordingSocket(wire.sock)
        peer = Peer(client=False, stream_window=65535 if mode == "settings-update" else 1024)
        if mode == "settings-update":
            peer.connection.update_settings({SettingCodes.INITIAL_WINDOW_SIZE: 1024})
        result, failure = None, None
        try:
            if mode == "withheld-settings":
                sock.settimeout(3)
                pending = bytearray()
                while not sock.flow:
                    data = sock.recv(65536)
                    require(data, "EOF before pre-SETTINGS DATA")
                    pending.extend(data)
                    require(len(pending) <= 131072, "startup prefix exceeded bound")
                require(sock.bytes["out"] == 0, "server sent bytes before the startup witness")
                sock.replay = pending
            result = serve(sock, config=config, timeout=8, peer=peer)
        except Exception as error:
            failure = f"{type(error).__name__}: {error}"
        finally:
            observations.append({"wire": sock.report(), "peer_error": failure, "peer_result": result})
            sock.close()

    with ScriptedPeer(handler, timeout=12) as server:
        spec = Case("reduced-single", 131087, config=config).spec(server.address, upload=True)
        spec["timeout_ms"] = 8000
        if mode == "warmup":
            spec["requests"].insert(0, {"method": "GET", "path": "/bytes/0", "body_bytes": 0, "trailers": []})
            spec["request_count"] = 2
        request_file, result_file = directory / "request.json", directory / "result.json"
        write_json(request_file, spec)
        completed = run_command(command(adapter, request_file, result_file), cwd=ROOT, timeout=11)
    require(len(observations) == 1, "missing socket observation")
    result = {
        "mode": mode, "exit_code": completed.returncode,
        "stderr": completed.stderr.decode(errors="replace"),
        "result": read_json(result_file) if result_file.exists() else None,
        **observations[0],
    }
    write_json(directory / "diagnostic.json", result)
    return result


def early_probe(adapter, binary, directory, *, tolerate_startup):
    directory.mkdir(parents=True)
    scenario = "early-response-startup-diagnostic" if tolerate_startup else "early-response"
    witness_file = directory / "peer.json"
    with PeerProcess([
        "env", "H2_PEER_TRACE=1", str(binary), "--scenario", scenario, "--report", str(witness_file),
    ], cwd=ROOT) as server:
        request_file, result_file = directory / "request.json", directory / "result.json"
        write_json(request_file, protocol_request("early-response", server.address))
        completed = run_command(command(adapter, request_file, result_file), cwd=ROOT, timeout=12)
        server_code = server.process.wait(timeout=3)
        for thread in server._threads:
            thread.join(timeout=1)
            require(not thread.is_alive(), "diagnostic reader survived peer exit")
        result = {
            "scenario": scenario, "exit_code": completed.returncode,
            "stderr": completed.stderr.decode(errors="replace"),
            "peer_exit_code": server_code, "peer_trace": server.diagnostics(),
            "result": read_json(result_file) if result_file.exists() else None,
            "peer": read_json(witness_file) if witness_file.exists() else None,
        }
    write_json(directory / "diagnostic.json", result)
    return result


def main():
    prerequisites()
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--go-peer", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    require("target" in args.output.resolve().parts, "diagnostics must reside under target")
    args.output.mkdir(parents=True)
    adapter = {"command": [str(args.binary.resolve()), "client", "{request_file}", "{result_file}"]}
    report = {"schema": 1, "evidence": "focused startup diagnostics NOT suite qualification", "echo": {}, "early": {}}
    for mode in ("strict", "withheld-settings", "settings-update", "warmup"):
        result = echo_probe(adapter, args.output / ("echo-" + mode), mode)
        report["echo"][mode] = result
        print("echo", mode, result["exit_code"], result["peer_error"], flush=True)
    for tolerate in (False, True):
        name = "tolerant" if tolerate else "strict"
        result = early_probe(adapter, args.go_peer.resolve(), args.output / ("early-" + name),
                             tolerate_startup=tolerate)
        report["early"][name] = result
        print("early", name, result["exit_code"], result["peer_exit_code"], flush=True)
    write_json(args.output / "report.json", report)


if __name__ == "__main__":
    main()
