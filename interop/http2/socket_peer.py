"""Bounded socket transport and wire accounting for the independent h2 peer."""

from __future__ import annotations

from collections import defaultdict, deque
import importlib.metadata
from pathlib import Path
import select
import socket
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "http1"))
from harness import PeerProcess, run_command  # noqa: E402
from scripted import ScriptedPeer  # noqa: E402

from h2.events import (  # noqa: E402
    DataReceived, InformationalResponseReceived, PingAckReceived,
    ResponseReceived, StreamEnded, TrailersReceived,
)

from peer import Peer, Received

MAX_WIRE = 128 * 1024 * 1024
MAX_QUEUE = 2 * 1024 * 1024
MAX_FRAMES = 300_000
PREFACE = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def prerequisites():
    version = importlib.metadata.version("h2")
    require(version == "4.3.0", f"python-h2==4.3.0 required, found {version}")
    for package in ("hpack", "hyperframe"):
        importlib.metadata.version(package)


class Trace:
    """Retain counters, not bodies or a complete wire transcript."""

    def __init__(self, preface=False):
        self.preface = preface
        self.buffer = bytearray()
        self.bytes = 0
        self.frames = 0
        self.flow = defaultdict(int)
        self.payload = defaultdict(int)
        self.updates = defaultdict(int)
        self.types = defaultdict(int)
        self.resets = {}
        self.goaway = []
        self.settings = []

    def feed(self, data):
        self.bytes += len(data)
        require(self.bytes <= MAX_WIRE, "socket byte limit exceeded")
        self.buffer.extend(data)
        if self.preface:
            if len(self.buffer) < len(PREFACE):
                return
            require(self.buffer[:24] == PREFACE, "invalid client preface")
            del self.buffer[:24]
            self.preface = False
        while len(self.buffer) >= 9:
            length = int.from_bytes(self.buffer[:3], "big")
            require(length <= MAX_QUEUE, "frame exceeds independent peer bound")
            if len(self.buffer) < length + 9:
                break
            kind, flags = self.buffer[3:5]
            stream = int.from_bytes(self.buffer[5:9], "big") & 0x7fffffff
            body = self.buffer[9:9 + length]
            del self.buffer[:9 + length]
            self.frames += 1
            require(self.frames <= MAX_FRAMES, "frame count limit exceeded")
            self.types[kind] += 1
            if kind == 0:
                self.flow[stream] += length
                padding = body[0] + 1 if flags & 8 and body else 0
                self.payload[stream] += length - padding
            elif kind == 8 and length == 4:
                self.updates[stream] += int.from_bytes(body, "big") & 0x7fffffff
            elif kind == 3 and length == 4:
                self.resets[stream] = int.from_bytes(body, "big")
            elif kind == 7 and length >= 8:
                require(len(self.goaway) < 16, "GOAWAY count limit exceeded")
                self.goaway.append({
                    "last_stream_id": int.from_bytes(body[:4], "big") & 0x7fffffff,
                    "code": int.from_bytes(body[4:8], "big"),
                })
            elif kind == 4 and not flags & 1:
                require(len(self.settings) < 64, "SETTINGS count limit exceeded")
                self.settings.append({
                    int.from_bytes(body[i:i + 2], "big"):
                    int.from_bytes(body[i + 2:i + 6], "big")
                    for i in range(0, length, 6)
                })
            require(
                max(len(self.flow), len(self.updates), len(self.resets)) <= 2048,
                "stream count limit exceeded",
            )
        require(len(self.buffer) <= MAX_QUEUE + 9, "frame buffer limit exceeded")


class Channel:
    def __init__(self, sock, peer, *, timeout=60, consume=True):
        self.sock = sock
        self.peer = peer
        self.deadline = time.monotonic() + timeout
        self.sock.setblocking(False)
        self.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.out = bytearray()
        self.inbound = Trace(preface=not peer.connection.config.client_side)
        self.outbound = Trace(preface=peer.connection.config.client_side)
        self.consume = consume
        self.eof = False
        self.results = {}
        self.closed = False
        self.deferred = []
        self.settlements = deque()

    def queue(self, raw=None):
        data = self.peer.take_wire() if raw is None else raw
        require(len(self.out) + len(data) <= MAX_QUEUE, "socket write queue exceeded")
        require(self.outbound.bytes + len(self.out) + len(data) <= MAX_WIRE, "socket byte limit exceeded")
        self.out.extend(data)

    def result(self, stream):
        return self.results.setdefault(stream, {
            "stream_id": stream, "status": None, "bytes": 0, "sha256": "",
            "content_length": None,
            "trailers": [], "informational": [], "ended": False, "outcome": None, "error": None,
        })

    def after_write(self, callback):
        boundary = self.outbound.bytes + len(self.out)
        self.settlements.append((boundary, callback))

    def step(self):
        self.queue()
        if self.deferred:
            events, self.deferred = self.deferred, []
            return events
        left = self.deadline - time.monotonic()
        if left <= 0:
            raise TimeoutError("HTTP/2 socket deadline expired")
        readable, writable, _ = select.select(
            [] if self.eof else [self.sock], [self.sock] if self.out else [], [], left,
        )
        if not readable and not writable:
            raise TimeoutError("HTTP/2 socket deadline expired")
        if writable:
            count = self.sock.send(self.out)
            require(count > 0, "socket write made no progress")
            self.outbound.feed(self.out[:count])
            del self.out[:count]
            while self.settlements and self.settlements[0][0] <= self.outbound.bytes:
                _boundary, settle = self.settlements.popleft()
                settle()
        events = []
        if readable:
            allowance = MAX_WIRE - self.inbound.bytes
            require(allowance > 0, "socket byte limit exceeded")
            data = self.sock.recv(min(65536, allowance))
            if not data:
                self.eof = True
            else:
                self.inbound.feed(data)
                events = self.peer.receive(data, consume=False)
                for event in events:
                    stream = getattr(event, "stream_id", None)
                    if isinstance(event, (ResponseReceived, InformationalResponseReceived)):
                        status = int(dict(event.headers)[b":status"])
                        if isinstance(event, InformationalResponseReceived):
                            self.result(stream)["informational"].append(status)
                        else:
                            self.result(stream)["status"] = status
                            length = dict(event.headers).get(b"content-length")
                            self.result(stream)["content_length"] = int(length) if length is not None else None
                    elif isinstance(event, TrailersReceived):
                        self.result(stream)["trailers"] = [
                            [name.decode("ascii"), value.decode("ascii")]
                            for name, value in event.headers
                        ]
                    elif isinstance(event, StreamEnded):
                        self.result(stream)["ended"] = True
                    if isinstance(event, DataReceived) and self.consume:
                        self.peer.consume(stream, event.flow_controlled_length)
        return events

    def handshake(self):
        self.peer.connection.ping(b"interop1")
        pending = []
        for _ in range(256):
            events = self.step()
            require(not self.eof, "EOF during handshake")
            pending.extend(event for event in events if getattr(event, "stream_id", 0))
            require(len(pending) <= 4096, "handshake event limit exceeded")
            if any(isinstance(event, PingAckReceived) for event in events):
                require(self.peer.remote_settings, "peer did not advertise SETTINGS")
                self.deferred = pending
                return
        raise AssertionError("handshake turn limit exceeded")

    def flush(self):
        while True:
            self.queue()
            if not self.out:
                break
            self.step()

    def close(self):
        if not self.closed:
            self.sock.close()
            self.closed = True

    def report(self):
        termination = self.peer.termination
        failed_connection = termination is not None and termination.error_code != 0
        for stream, result in self.results.items():
            received = self.peer.received.get(stream, Received())
            result["bytes"] = received.payload
            result["sha256"] = received.digest.hexdigest()
            if stream in self.peer.resets:
                result["error"] = {"scope": "stream", "code": self.peer.resets[stream]}
                result["outcome"] = "reset"
            else:
                live = self.peer.connection.streams.get(stream)
                result["outcome"] = (
                    "complete" if result["ended"] and (live is None or live.closed) and not self.out
                    else "connection_failed" if failed_connection or self.closed else None
                )
        return {
            "schema": 1, "streams": list(self.results.values()),
            "connection": {
                "closed": self.closed,
                "outcome": "protocol" if failed_connection else (
                    ("peer_closed" if self.eof else "graceful") if self.closed else None
                ),
                "error": None if termination is None or not termination.error_code else {
                    "scope": "connection", "code": int(termination.error_code),
                },
            },
        }


def credit_report(channel, sender, update_baseline):
    """Check exact DATA and WINDOW_UPDATE totals against the sender balances."""
    peer = channel.peer
    connection_initial = sender.initial_connection_credit
    require(connection_initial is not None, "sender did not capture initial connection credit")
    connection_updates = channel.inbound.updates[0] - update_baseline
    connection_final = connection_initial + connection_updates - sender.total_flow
    require(connection_final >= 0, "sender exceeded connection credit")
    require(
        connection_final == peer.connection.outbound_flow_control_window,
        "connection WINDOW_UPDATE accounting mismatch",
    )
    streams = []
    for stream, initial in sender.initial_stream_credit.items():
        send = sender.completed.get(stream) or sender.cancelled[stream]
        updates = channel.inbound.updates[stream]
        flow = channel.outbound.flow[stream]
        require(flow == send.flow_sent, "DATA flow accounting mismatch")
        require(
            channel.outbound.payload[stream] == send.sent,
            "DATA payload accounting mismatch",
        )
        final = initial + updates - flow
        require(final >= 0, "sender exceeded stream credit")
        live = peer.connection.streams.get(stream)
        # python-h2 ignores late WINDOW_UPDATE on closed streams. The wire
        # equation still includes those increments, but its retired object does not.
        if live is not None and not live.closed:
            require(final == live.outbound_flow_control_window, "stream credit mismatch")
        streams.append({
            "stream_id": stream, "initial": initial, "window_update": updates,
            "flow": flow, "final": final, "frames": send.frames,
            "cancelled": stream in sender.cancelled,
        })
    return {
        "initial_connection": connection_initial,
        "connection_window_update": connection_updates,
        "flow": sender.total_flow, "final_connection": connection_final,
        "streams": streams,
    }


def receive_credit_report(channel, initial_connection, update_baseline):
    channel.queue()
    pending = Trace()
    pending.feed(bytes(channel.outbound.buffer) + bytes(channel.out))
    total_flow = sum(channel.inbound.flow.values())
    updates = channel.outbound.updates[0] - update_baseline
    final = initial_connection + updates - total_flow
    generated = channel.peer.generated_connection_refunds
    abandoned = generated - updates - pending.updates[0]
    require(abandoned >= 0, "wire/pending refunds exceed generated connection credit")
    require(
        abandoned == 0 or channel.peer.termination is not None,
        "connection credit disappeared before GOAWAY",
    )
    require(
        initial_connection + generated - total_flow == channel.peer.connection.inbound_flow_control_window,
        f"receive connection credit mismatch: initial={initial_connection}, generated={generated}, "
        f"wire={updates}, pending={pending.updates[0]}, abandoned={abandoned}, flow={total_flow}, "
        f"actual={channel.peer.connection.inbound_flow_control_window}",
    )
    streams = []
    for stream in channel.results:
        received = channel.peer.received.get(stream, Received())
        require(received.flow == channel.inbound.flow[stream], "received DATA flow mismatch")
        require(received.payload == channel.inbound.payload[stream], "received DATA payload mismatch")
        initial = channel.peer.connection.local_settings.initial_window_size
        stream_updates = channel.outbound.updates[stream]
        balance = initial + stream_updates - received.flow
        require(balance >= 0, "receive stream credit became negative")
        streams.append({
            "stream_id": stream, "initial": initial, "window_update": stream_updates,
            "flow": received.flow, "final": balance,
        })
    return {
        "initial_connection": initial_connection, "connection_window_update": updates,
        "pending_connection_window_update": pending.updates[0],
        "goaway_discarded_connection_window_update": abandoned,
        "flow": total_flow, "final_connection": final, "streams": streams,
    }


def configured_peer(*, client, config):
    return Peer(
        client=client,
        stream_window=config.get("stream_window", 65535),
        connection_window=config.get("connection_window", 65535),
    )
