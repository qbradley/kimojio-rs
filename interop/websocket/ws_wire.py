"""Independent, bounded RFC 6455 wire fixtures and message checks."""

from __future__ import annotations

import base64
import codecs
import dataclasses
import hashlib
import os
from pathlib import Path
import struct
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "http1"))
from harness import ProtocolError, Wire, fields  # noqa: E402


GUID = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
KEY = b"dGhlIHNhbXBsZSBub25jZQ=="
MAX_PAYLOAD = 1024 * 1024


class WebSocketError(ProtocolError):
    def __init__(self, message: str, close_code: int = 1002):
        super().__init__(message)
        self.close_code = close_code


def accept_key(key: bytes) -> bytes:
    try:
        decoded = base64.b64decode(key, validate=True)
    except ValueError as error:
        raise WebSocketError("invalid handshake key") from error
    if len(decoded) != 16:
        raise WebSocketError("handshake key must decode to 16 bytes")
    return base64.b64encode(hashlib.sha1(key + GUID).digest())


def upgrade_request(path: str = "/chat", *, key: bytes = KEY) -> bytes:
    return (
        b"GET " + path.encode("ascii") + b" HTTP/1.1\r\nHost: localhost\r\n"
        b"Upgrade: websocket\r\nConnection: keep-alive, Upgrade\r\n"
        b"Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: " + key + b"\r\n\r\n"
    )


def upgrade_response(key: bytes = KEY) -> bytes:
    return (
        b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n"
        b"Connection: Upgrade\r\nSec-WebSocket-Accept: " + accept_key(key) + b"\r\n\r\n"
    )


def check_server_upgrade(wire: Wire, *, key: bytes = KEY) -> None:
    response = wire.response()
    if response.status != 101:
        raise WebSocketError(f"expected 101, received {response.status}")
    tokens = [
        token.strip().lower()
        for value in response.values(b"connection")
        for token in value.split(b",")
    ]
    if b"upgrade" not in tokens or [
        value.lower() for value in response.values(b"upgrade")
    ] != [b"websocket"]:
        raise WebSocketError("invalid response upgrade fields")
    if response.values(b"sec-websocket-accept") != [accept_key(key)]:
        raise WebSocketError("wrong or duplicate Sec-WebSocket-Accept")
    if response.values(b"sec-websocket-extensions") or response.values(b"sec-websocket-protocol"):
        raise WebSocketError("unsolicited extension or subprotocol")


def read_client_upgrade(wire: Wire, *, path: str = "/chat") -> bytes:
    raw = wire.until(b"\r\n\r\n", 65536)
    lines = raw[:-4].split(b"\r\n")
    if lines[0] != b"GET " + path.encode("ascii") + b" HTTP/1.1":
        raise WebSocketError("invalid upgrade request line")
    headers = fields(lines[1:])

    def values(name):
        return [value for key, value in headers if key == name]

    connection = [
        token.strip().lower()
        for value in values(b"connection")
        for token in value.split(b",")
    ]
    if (
        len(values(b"host")) != 1
        or [value.lower() for value in values(b"upgrade")] != [b"websocket"]
        or b"upgrade" not in connection
        or values(b"sec-websocket-version") != [b"13"]
        or len(values(b"sec-websocket-key")) != 1
    ):
        raise WebSocketError("invalid upgrade request fields")
    key = values(b"sec-websocket-key")[0]
    accept_key(key)
    return key


@dataclasses.dataclass(frozen=True)
class Frame:
    opcode: int
    payload: bytes
    fin: bool
    masked: bool
    wire: bytes


def encode_frame(
    opcode: int,
    payload: bytes = b"",
    *,
    fin: bool = True,
    mask: bytes | None = None,
) -> bytes:
    """Encode exact fixtures; reserved opcodes and invalid controls are intentional options."""
    if not 0 <= opcode <= 15:
        raise ValueError("opcode must fit four bits")
    size = len(payload)
    if size > MAX_PAYLOAD:
        raise ValueError("fixture payload exceeds limit")
    prefix = bytes([(0x80 if fin else 0) | opcode])
    flag = 0x80 if mask is not None else 0
    if size < 126:
        prefix += bytes([flag | size])
    elif size <= 65535:
        prefix += bytes([flag | 126]) + struct.pack("!H", size)
    else:
        prefix += bytes([flag | 127]) + struct.pack("!Q", size)
    if mask is not None:
        if len(mask) != 4:
            raise ValueError("mask must contain four bytes")
        prefix += mask
        payload = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
    return prefix + payload


def client_frame(opcode: int, payload: bytes = b"", *, fin: bool = True) -> bytes:
    return encode_frame(opcode, payload, fin=fin, mask=os.urandom(4))


def read_frame(wire: Wire, *, expect_mask: bool, max_payload: int = MAX_PAYLOAD) -> Frame:
    raw = bytearray(wire.take(2))
    first, second = raw
    fin, opcode, masked = bool(first & 0x80), first & 15, bool(second & 0x80)
    if first & 0x70:
        raise WebSocketError("RSV bits require a negotiated extension")
    if opcode not in (0, 1, 2, 8, 9, 10):
        raise WebSocketError("reserved opcode")
    if masked != expect_mask:
        raise WebSocketError("wrong masking direction")
    size = second & 0x7F
    if size == 126:
        extended = wire.take(2)
        raw.extend(extended)
        size = int.from_bytes(extended, "big")
        if size < 126:
            raise WebSocketError("nonminimal 16-bit length")
    elif size == 127:
        extended = wire.take(8)
        raw.extend(extended)
        size = int.from_bytes(extended, "big")
        if size < 65536 or size & (1 << 63):
            raise WebSocketError("invalid 64-bit length")
    if opcode >= 8 and (not fin or size > 125):
        raise WebSocketError("fragmented or oversized control frame")
    if size > max_payload:
        raise WebSocketError("frame exceeds configured limit", 1009)
    mask = wire.take(4) if masked else b""
    raw.extend(mask)
    payload = wire.take(size)
    raw.extend(payload)
    if masked:
        payload = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
    return Frame(opcode, payload, fin, masked, bytes(raw))


def close_payload(code: int = 1000, reason: str = "") -> bytes:
    payload = struct.pack("!H", code) + reason.encode("utf-8")
    if len(payload) > 125:
        raise ValueError("close payload exceeds 125 bytes")
    return payload


def check_close(payload: bytes) -> tuple[int | None, str]:
    if not payload:
        return None, ""
    if len(payload) == 1:
        raise WebSocketError("close payload has a partial status code")
    code = int.from_bytes(payload[:2], "big")
    if code not in (1000, 1001, 1002, 1003, 1007, 1008, 1009, 1010, 1011, 1012, 1013, 1014) and not (
        3000 <= code <= 4999
    ):
        raise WebSocketError("invalid close status code")
    try:
        reason = payload[2:].decode("utf-8")
    except UnicodeError as error:
        raise WebSocketError("invalid close reason UTF-8", 1007) from error
    return code, reason


class Messages:
    def __init__(self, *, max_message: int = MAX_PAYLOAD):
        self.max_message = max_message
        self.opcode: int | None = None
        self.payload = bytearray()
        self.decoder = None
        self.closed = False

    def consume(self, frame: Frame) -> tuple[str, bytes] | None:
        if self.closed:
            raise WebSocketError("frame after close")
        if frame.opcode == 8:
            check_close(frame.payload)
            self.closed = True
            return "close", frame.payload
        if frame.opcode in (9, 10):
            return ("ping" if frame.opcode == 9 else "pong"), frame.payload
        if frame.opcode == 0:
            if self.opcode is None:
                raise WebSocketError("continuation without a fragmented message")
        else:
            if self.opcode is not None:
                raise WebSocketError("new data frame during a fragmented message")
            self.opcode = frame.opcode
            self.decoder = codecs.getincrementaldecoder("utf-8")() if frame.opcode == 1 else None
        if len(self.payload) + len(frame.payload) > self.max_message:
            raise WebSocketError("message exceeds configured limit", 1009)
        if self.decoder is not None:
            try:
                self.decoder.decode(frame.payload, final=frame.fin)
            except UnicodeError as error:
                raise WebSocketError("invalid text UTF-8", 1007) from error
        self.payload.extend(frame.payload)
        if not frame.fin:
            return None
        event = ("text" if self.opcode == 1 else "binary"), bytes(self.payload)
        self.opcode = None
        self.payload.clear()
        self.decoder = None
        return event
