"""Independent HTTP/2 peers with explicit consumption and bounded send turns."""

from __future__ import annotations

from collections import deque
from dataclasses import dataclass, field
import hashlib

from h2.config import H2Configuration
from h2.connection import H2Connection
from h2.events import (
    ConnectionTerminated,
    DataReceived,
    RemoteSettingsChanged,
    StreamEnded,
    StreamReset,
    WindowUpdated,
)
from h2.settings import SettingCodes, Settings


@dataclass
class Received:
    payload: int = 0
    flow: int = 0
    unconsumed: int = 0
    ended: bool = False
    digest: object = field(default_factory=hashlib.sha256)


class Peer:
    """Let python-h2 enforce framing and both flow-control windows."""

    def __init__(
        self, *, client: bool, stream_window: int = 65535,
        connection_window: int = 65535,
    ):
        if not 0 < stream_window <= 2**31 - 1:
            raise ValueError("stream window must be in 1..2^31-1")
        if not 65535 <= connection_window <= 2**31 - 1:
            raise ValueError("connection window must be in 65535..2^31-1")
        self.connection = H2Connection(
            config=H2Configuration(client_side=client, header_encoding=None)
        )
        initial = dict(self.connection.local_settings)
        initial[SettingCodes.INITIAL_WINDOW_SIZE] = stream_window
        if client:
            initial[SettingCodes.ENABLE_PUSH] = 0
        self.connection.local_settings = Settings(client=client, initial_values=initial)
        if not client:
            # python-h2's default server settings retain the RFC 7540 value.
            # RFC 9113 prohibits servers from sending ENABLE_PUSH, even zero.
            del self.connection.local_settings[SettingCodes.ENABLE_PUSH]
        self.connection.initiate_connection()
        if connection_window > 65535:
            self.connection.increment_flow_control_window(connection_window - 65535)
        self.received: dict[int, Received] = {}
        self.window_updates: dict[int, int] = {}
        self.remote_settings: list[dict[int, int]] = []
        self.resets: dict[int, int] = {}
        self.termination = None
        self.generated_connection_refunds = 0

    def receive(self, wire: bytes, *, consume: bool = True) -> list:
        events = self.connection.receive_data(wire)
        for event in events:
            if isinstance(event, DataReceived):
                received = self.received.setdefault(event.stream_id, Received())
                received.payload += len(event.data)
                received.flow += event.flow_controlled_length
                received.unconsumed += event.flow_controlled_length
                received.digest.update(event.data)
                if consume:
                    self.consume(event.stream_id, event.flow_controlled_length)
            elif isinstance(event, StreamEnded):
                self.received.setdefault(event.stream_id, Received()).ended = True
            elif isinstance(event, WindowUpdated):
                self.window_updates[event.stream_id] = (
                    self.window_updates.get(event.stream_id, 0) + event.delta
                )
            elif isinstance(event, RemoteSettingsChanged):
                self.remote_settings.append(
                    {int(key): value.new_value for key, value in event.changed_settings.items()}
                )
            elif isinstance(event, StreamReset):
                self.resets[event.stream_id] = int(event.error_code)
            elif isinstance(event, ConnectionTerminated):
                self.termination = event
        return events

    def consume(self, stream_id: int, flow_bytes: int | None = None) -> None:
        received = self.received[stream_id]
        amount = received.unconsumed if flow_bytes is None else flow_bytes
        if not 0 <= amount <= received.unconsumed:
            raise ValueError("consumption exceeds received flow-controlled bytes")
        before = self.connection.inbound_flow_control_window
        self.connection.acknowledge_received_data(amount, stream_id)
        granted = self.connection.inbound_flow_control_window - before
        if granted < 0:
            raise AssertionError("consumption reduced connection credit")
        self.generated_connection_refunds += granted
        received.unconsumed -= amount

    def take_wire(self) -> bytes:
        return self.connection.data_to_send()

    def request(self, stream_id: int, path: str = "/download") -> None:
        self.connection.send_headers(
            stream_id,
            [
                (b":method", b"GET"),
                (b":scheme", b"http"),
                (b":authority", b"localhost"),
                (b":path", path.encode("ascii")),
            ],
            end_stream=True,
        )


@dataclass
class Fragment:
    stream_id: int
    data: bytes
    flow: int
    offset: int = 0
    settled: bool = False


@dataclass
class Send:
    stream_id: int
    length: int
    padding: int | None
    trailers: bool | tuple
    sent: int = 0
    flow_sent: int = 0
    frames: int = 0
    fragments: deque[Fragment] | None = None
    source_ended: bool = False


class CreditSender:
    """Send at most one frame per ready stream in each bounded round."""

    def __init__(self, peer: Peer):
        self.peer = peer
        self.pending: deque[Send] = deque()
        self.completed: dict[int, Send] = {}
        self.cancelled: dict[int, Send] = {}
        self.initial_connection_credit: int | None = None
        self.initial_stream_credit: dict[int, int] = {}
        self.total_payload = 0
        self.total_flow = 0
        self.streaming: dict[int, Send] = {}
        self.releases: list[Fragment] = []
        self.owned_bytes = 0
        self.owned_items = 0
        self.peak_owned_bytes = 0
        self.peak_owned_items = 0
        self.storage_limit = peer.connection.inbound_flow_control_window

    def response(
        self,
        stream_id: int,
        length: int,
        *,
        padding: int | None = None,
        trailers: bool | tuple = False,
        informational: bool = False,
    ) -> None:
        if length < 0 or padding is not None and not 0 <= padding <= 255:
            raise ValueError("invalid response length or padding")
        if stream_id in self.initial_stream_credit:
            raise ValueError("response already started")
        connection = self.peer.connection
        if informational:
            connection.send_headers(
                stream_id, [(b":status", b"103"), (b"link", b"</asset>")]
            )
        connection.send_headers(
            stream_id,
            [(b":status", b"200"), (b"content-length", str(length).encode())],
            end_stream=length == 0 and not trailers,
        )
        self.start(stream_id, length, padding=padding, trailers=trailers)

    def start(
        self, stream_id: int, length: int, *,
        padding: int | None = None, trailers: bool | tuple = False, streaming: bool = False,
    ) -> None:
        """Queue an already opened response or request body."""
        if length < 0 or padding is not None and not 0 <= padding <= 255:
            raise ValueError("invalid body length or padding")
        if stream_id in self.initial_stream_credit:
            raise ValueError("body already started")
        connection = self.peer.connection
        # Capture the peer's actual balances, not assumed defaults.
        self.initial_stream_credit[stream_id] = (
            connection.streams[stream_id].outbound_flow_control_window
        )
        if self.initial_connection_credit is None:
            self.initial_connection_credit = connection.outbound_flow_control_window
        send = Send(stream_id, length, padding, trailers)
        if streaming:
            send.fragments = deque()
            self.streaming[stream_id] = send
            self.pending.append(send)
        elif length == 0:
            self._finish(send)
        else:
            self.pending.append(send)

    def echo_response(self, stream_id: int, content_length: int | None) -> None:
        headers = [(b":status", b"200")]
        if content_length is not None:
            headers.append((b"content-length", str(content_length).encode()))
        self.peer.connection.send_headers(stream_id, headers)
        self.start(stream_id, 0, streaming=True)

    def feed(self, stream_id: int, data: bytes, flow: int) -> None:
        send = self.streaming[stream_id]
        if send.source_ended or flow < len(data):
            raise ValueError("invalid streaming fragment")
        if self.owned_bytes + flow > self.storage_limit or self.owned_items == 4096:
            raise ValueError("streaming echo retained-storage limit exceeded")
        send.fragments.append(Fragment(stream_id, data, flow))
        send.length += len(data)
        self.owned_bytes += flow
        self.owned_items += 1
        self.peak_owned_bytes = max(self.peak_owned_bytes, self.owned_bytes)
        self.peak_owned_items = max(self.peak_owned_items, self.owned_items)

    def end(self, stream_id: int) -> None:
        self.streaming[stream_id].source_ended = True

    def cancel(self, stream_id: int) -> None:
        if stream_id in self.streaming:
            raise ValueError("streaming echo cancellation requires fragment settlement")
        for send in self.pending:
            if send.stream_id == stream_id:
                self.pending.remove(send)
                self.cancelled[stream_id] = send
                return
        self.cancelled[stream_id] = self.completed.pop(stream_id)

    def settle(self, fragment: Fragment, *, consume: bool = True) -> None:
        """Return ownership and input credit only after the echo write settles."""
        if fragment.settled or fragment.offset != len(fragment.data):
            raise ValueError("fragment was already released or not fully sent")
        if consume:
            self.peer.consume(fragment.stream_id, fragment.flow)
        fragment.settled = True
        self.owned_bytes -= fragment.flow
        self.owned_items -= 1

    def _finish(self, send: Send) -> None:
        if send.trailers:
            self.peer.connection.send_headers(
                send.stream_id,
                [(b"x-end", b"done")] if send.trailers is True else list(send.trailers),
                end_stream=True,
            )
        self.completed[send.stream_id] = send

    def drive(self, frame_budget: int = 64, *, blocked_streams=()) -> int:
        if frame_budget <= 0:
            raise ValueError("frame budget must be positive")
        connection = self.peer.connection
        sent = 0
        for _ in range(min(len(self.pending), frame_budget)):
            send = self.pending.popleft()
            if send.stream_id in blocked_streams:
                self.pending.append(send)
                continue
            padding_cost = 0 if send.padding is None else send.padding + 1
            available = min(
                connection.local_flow_control_window(send.stream_id),
                connection.max_outbound_frame_size,
            ) - padding_cost
            fragment = send.fragments[0] if send.fragments else None
            remaining = (
                len(fragment.data) - fragment.offset if fragment is not None
                else send.length - send.sent
            )
            amount = min(remaining, max(0, available))
            empty_frame = send.fragments is not None and remaining == 0 and (
                fragment is not None or send.source_ended
            )
            if amount == 0 and not empty_frame:
                self.pending.append(send)
                continue
            final = send.sent + amount == send.length and (
                send.fragments is None or send.source_ended and len(send.fragments) <= 1
            )
            connection.send_data(
                send.stream_id,
                fragment.data[fragment.offset:fragment.offset + amount]
                if fragment is not None else bytes([send.stream_id % 251]) * amount,
                end_stream=final and not send.trailers,
                pad_length=send.padding,
            )
            send.sent += amount
            send.flow_sent += amount + padding_cost
            send.frames += 1
            self.total_payload += amount
            self.total_flow += amount + padding_cost
            sent += 1
            if fragment is not None:
                fragment.offset += amount
                if fragment.offset == len(fragment.data):
                    send.fragments.popleft()
                    self.releases.append(fragment)
            if final:
                self._finish(send)
            else:
                self.pending.append(send)
        return sent


def digest_for(stream_id: int, length: int) -> str:
    digest = hashlib.sha256()
    block = bytes([stream_id % 251]) * 16384
    while length:
        amount = min(length, len(block))
        digest.update(block[:amount])
        length -= amount
    return digest.hexdigest()


def exchange(left: Peer, right: Peer, *, consume: bool = True) -> bool:
    """Move currently queued bytes once in both directions."""
    progressed = False
    for source, target in ((left, right), (right, left)):
        wire = source.take_wire()
        if wire:
            target.receive(wire, consume=consume)
            progressed = True
    return progressed


def handshake(client: Peer, server: Peer) -> None:
    for _ in range(16):
        if not exchange(client, server):
            return
    raise AssertionError("handshake did not quiesce")
