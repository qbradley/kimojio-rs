"""Bounded, standard-library HTTP/1 interoperability helpers."""

from __future__ import annotations

import collections
import contextlib
import dataclasses
import ipaddress
import os
from pathlib import Path
import re
import signal
import socket
import subprocess
import threading
import time
from typing import Sequence


class ProtocolError(AssertionError):
    pass


class PeerProcess:
    """Own a command and its process group; drain both output pipes continuously."""

    def __init__(
        self,
        command: Sequence[str],
        *,
        cwd: Path,
        startup_timeout: float = 5,
        stop_timeout: float = 2,
        wait_for_readiness: bool = True,
        capture_limit: int = 128 * 1024,
    ):
        if not command or isinstance(command, str):
            raise ValueError("command must be a nonempty argument array")
        if capture_limit < 1 or startup_timeout <= 0 or stop_timeout <= 0:
            raise ValueError("process limits must be positive")
        self.command = list(command)
        self.cwd = cwd
        self.startup_timeout = startup_timeout
        self.stop_timeout = stop_timeout
        self.wait_for_readiness = wait_for_readiness
        self.capture_limit = capture_limit
        self.process: subprocess.Popen | None = None
        self.address: tuple[str, int] | None = None
        self.output = collections.deque()
        self._buffered_bytes = 0
        self._ready = threading.Event()
        self._lock = threading.Lock()
        self._error: str | None = None
        self._threads: list[threading.Thread] = []
        self.output_bytes = 0

    def _drain(self, pipe, label: str) -> None:
        pending = b""
        try:
            while data := os.read(pipe.fileno(), 4096):
                with self._lock:
                    self.output.append((label, data))
                    self.output_bytes += len(data)
                    self._buffered_bytes += len(data)
                    while self._buffered_bytes > self.capture_limit:
                        _label, old = self.output.popleft()
                        self._buffered_bytes -= len(old)
                if not self.wait_for_readiness or label != "stdout" or self._ready.is_set():
                    continue
                pending += data
                while b"\n" in pending:
                    line, pending = pending.split(b"\n", 1)
                    if not line.startswith(b"LISTEN "):
                        continue
                    try:
                        host, port = line[7:].decode("ascii").strip().rsplit(":", 1)
                        if ipaddress.ip_address(host) != ipaddress.ip_address("127.0.0.1"):
                            raise ValueError("readiness address must be 127.0.0.1")
                        if not 0 < int(port) <= 65535:
                            raise ValueError("invalid readiness port")
                        self.address = (host, int(port))
                    except (ValueError, UnicodeError) as error:
                        self._error = str(error)
                    self._ready.set()
                    break
                if len(pending) > 8192:
                    self._error = "readiness line exceeds 8192 bytes"
                    self._ready.set()
        except OSError:
            pass
        finally:
            pipe.close()

    def diagnostics(self) -> str:
        with self._lock:
            streams = {
                label: b"".join(data for stream, data in self.output if stream == label)
                for label in ("stdout", "stderr")
            }
            return "\n".join(
                f"{label}: {data.decode('utf-8', errors='replace')}"
                for label, data in streams.items() if data
            )

    def __enter__(self) -> "PeerProcess":
        self.process = subprocess.Popen(
            self.command,
            cwd=self.cwd,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
        for label in ("stdout", "stderr"):
            thread = threading.Thread(
                target=self._drain, args=(getattr(self.process, label), label), daemon=True
            )
            thread.start()
            self._threads.append(thread)
        if not self.wait_for_readiness:
            return self
        deadline = time.monotonic() + self.startup_timeout
        try:
            while not self._ready.wait(min(0.02, max(0, deadline - time.monotonic()))):
                if self.process.poll() is not None:
                    raise RuntimeError("peer exited before LISTEN")
                if time.monotonic() >= deadline:
                    raise TimeoutError("peer did not emit LISTEN before deadline")
            if self._error:
                raise RuntimeError(self._error)
            if self.process.poll() is not None:
                raise RuntimeError("peer exited after LISTEN")
            return self
        except BaseException as error:
            self.close()
            error.add_note(self.diagnostics())
            raise

    def close(self) -> None:
        if self.process is None:
            return
        # The group belongs to this child because Popen used start_new_session.
        with contextlib.suppress(ProcessLookupError):
            os.killpg(self.process.pid, signal.SIGTERM)
        try:
            self.process.wait(timeout=self.stop_timeout)
        except subprocess.TimeoutExpired:
            pass
        # Reap descendants that kept a pipe open or ignored SIGTERM.
        with contextlib.suppress(ProcessLookupError):
            os.killpg(self.process.pid, signal.SIGKILL)
        self.process.wait(timeout=self.stop_timeout)
        for thread in self._threads:
            thread.join(timeout=self.stop_timeout)
        self.process = None

    def __exit__(self, *_args) -> None:
        self.close()


def run_command(
    command: Sequence[str],
    *,
    cwd: Path,
    timeout: float = 5,
    max_output: int = 128 * 1024,
) -> subprocess.CompletedProcess:
    """Run an arbitrary client command; fail on timeout or excessive output."""
    with PeerProcess(
        command, cwd=cwd, wait_for_readiness=False, capture_limit=max_output
    ) as child:
        deadline = time.monotonic() + timeout
        while child.process.poll() is None:
            if time.monotonic() >= deadline:
                raise TimeoutError("client command deadline expired")
            if child.output_bytes > max_output:
                raise ProtocolError("client command output exceeds limit")
            time.sleep(0.01)
        for thread in child._threads:
            thread.join(timeout=max(0, deadline - time.monotonic()))
            if thread.is_alive():
                raise TimeoutError("client descendants kept output pipes open")
        with child._lock:
            if child.output_bytes > max_output:
                raise ProtocolError("client command output exceeds limit")
            stdout = b"".join(data for label, data in child.output if label == "stdout")
            stderr = b"".join(data for label, data in child.output if label == "stderr")
        return subprocess.CompletedProcess(
            list(command), child.process.returncode, stdout, stderr
        )


TOKEN = re.compile(rb"^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$")
STATUS = re.compile(rb"^HTTP/(1\.[01]) ([1-5][0-9]{2}) ([\t\x20-\x7e\x80-\xff]*)$")
CHUNK_EXTENSIONS = re.compile(
    rb"""^(?:[ \t]*;[ \t]*[!#$%&'*+\-.^_`|~0-9A-Za-z]+"""
    rb"""(?:[ \t]*=[ \t]*(?:[!#$%&'*+\-.^_`|~0-9A-Za-z]+|"""
    rb'''"(?:[\t !#-\[\]-~\x80-\xff]|\\[\t -~\x80-\xff])*"))?)*$'''
)


@dataclasses.dataclass(frozen=True)
class Response:
    version: str
    status: int
    headers: tuple[tuple[bytes, bytes], ...]
    body: bytes
    trailers: tuple[tuple[bytes, bytes], ...]
    wire: bytes

    def values(self, name: bytes) -> list[bytes]:
        return [value for key, value in self.headers if key == name.lower()]


def fields(lines: list[bytes]) -> tuple[tuple[bytes, bytes], ...]:
    result = []
    for line in lines:
        name, sep, value = line.partition(b":")
        if not sep or not TOKEN.fullmatch(name):
            raise ProtocolError(f"invalid field name: {line!r}")
        if any(byte < 32 and byte != 9 or byte == 127 for byte in value):
            raise ProtocolError(f"invalid field value: {line!r}")
        result.append((name.lower(), value.strip(b" \t")))
    return tuple(result)


class Wire:
    """A strict response reader with one absolute deadline per connection."""

    def __init__(
        self,
        sock: socket.socket,
        *,
        timeout: float = 3,
        max_head: int = 65536,
        max_body: int = 8 * 1024 * 1024,
        max_wire: int = 16 * 1024 * 1024,
        recv_size: int = 65536,
    ):
        self.sock = sock
        self.deadline = time.monotonic() + timeout
        self.max_head = max_head
        self.max_body = max_body
        self.max_wire = max_wire
        self.recv_size = recv_size
        if min(max_head, max_body, max_wire, recv_size) < 1:
            raise ValueError("wire size limits must be positive")
        self.buffer = bytearray()
        self.eof = False
        self._wire = bytearray()

    @classmethod
    def connect(cls, address: tuple[str, int], **kwargs) -> "Wire":
        timeout = kwargs.get("timeout", 3)
        sock = socket.create_connection(address, timeout=timeout)
        sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        return cls(sock, **kwargs)

    def __enter__(self) -> "Wire":
        return self

    def __exit__(self, *_args) -> None:
        self.sock.close()

    def _timeout(self) -> None:
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("wire deadline expired")
        self.sock.settimeout(remaining)

    def _receive(self) -> None:
        if self.eof:
            raise ProtocolError("unexpected EOF")
        self._timeout()
        data = self.sock.recv(self.recv_size)
        if not data:
            self.eof = True
        else:
            self.buffer.extend(data)

    def send(self, data: bytes, *, chunk_size: int | None = None) -> None:
        chunk_size = chunk_size or max(1, len(data))
        if chunk_size < 1:
            raise ValueError("chunk size must be positive")
        for offset in range(0, len(data), chunk_size):
            self._timeout()
            self.sock.sendall(data[offset : offset + chunk_size])

    def take(self, count: int) -> bytes:
        if len(self._wire) + count > self.max_wire:
            raise ProtocolError("wire message exceeds limit")
        while len(self.buffer) < count:
            self._receive()
        result = bytes(self.buffer[:count])
        del self.buffer[:count]
        self._wire.extend(result)
        return result

    def until(self, delimiter: bytes, limit: int) -> bytes:
        while True:
            end = self.buffer.find(delimiter)
            if end >= 0:
                count = end + len(delimiter)
                if count > limit:
                    raise ProtocolError("field section exceeds limit")
                return self.take(count)
            if len(self.buffer) >= limit:
                raise ProtocolError("field section exceeds limit")
            self._receive()

    def response(self, method: str = "GET") -> Response:
        self._wire.clear()
        head = self.until(b"\r\n\r\n", self.max_head)
        lines = head[:-4].split(b"\r\n")
        match = STATUS.fullmatch(lines[0])
        if not match:
            raise ProtocolError(f"invalid status line: {lines[0]!r}")
        version, status = match[1].decode(), int(match[2])
        headers = fields(lines[1:])
        tunnel = method == "CONNECT" and 200 <= status < 300
        lengths = [value for key, value in headers if key == b"content-length"]
        transfers = [value.lower() for key, value in headers if key == b"transfer-encoding"]
        if tunnel:
            # Successful CONNECT switches protocols regardless of framing fields.
            lengths, transfers = [], []
        length = None
        if lengths:
            parts = [part.strip() for value in lengths for part in value.split(b",")]
            if any(not re.fullmatch(rb"[0-9]+", part) for part in parts):
                raise ProtocolError("invalid Content-Length")
            if len(set(parts)) != 1:
                raise ProtocolError("conflicting Content-Length")
            # Bound conversion as well as allocation.
            if len(parts[0]) > 20:
                raise ProtocolError("Content-Length overflow")
            length = int(parts[0])
        if transfers and lengths:
            raise ProtocolError("ambiguous Transfer-Encoding with Content-Length")
        if transfers and transfers != [b"chunked"]:
            raise ProtocolError("unsupported or malformed Transfer-Encoding")
        if status < 200 or status == 204:
            if transfers or lengths:
                raise ProtocolError("framing fields on body-forbidden response")
        trailers: tuple[tuple[bytes, bytes], ...] = ()
        body = b""
        bodyless = method == "HEAD" or status < 200 or status in (204, 304)
        if not bodyless and not tunnel:
            if transfers:
                chunks = []
                total = 0
                trailer_lines = []
                while True:
                    line = self.until(b"\r\n", self.max_head)[:-2]
                    size, sep, extension = line.partition(b";")
                    if sep:
                        size = size.rstrip(b" \t")
                    if not re.fullmatch(rb"[0-9A-Fa-f]{1,16}", size):
                        raise ProtocolError("invalid chunk size")
                    if sep and not CHUNK_EXTENSIONS.fullmatch(sep + extension):
                        raise ProtocolError("invalid chunk extension")
                    count = int(size, 16)
                    total += count
                    if total > self.max_body:
                        raise ProtocolError("body exceeds limit")
                    if count == 0:
                        trailer_size = 0
                        while True:
                            line = self.until(b"\r\n", self.max_head)
                            trailer_size += len(line)
                            if trailer_size > self.max_head:
                                raise ProtocolError("trailers exceed limit")
                            if line == b"\r\n":
                                break
                            trailer_lines.append(line[:-2])
                        trailers = fields(trailer_lines)
                        if any(
                            key in (b"content-length", b"transfer-encoding")
                            for key, _value in trailers
                        ):
                            raise ProtocolError("framing field in trailers")
                        break
                    chunks.append(self.take(count))
                    if self.take(2) != b"\r\n":
                        raise ProtocolError("invalid chunk terminator")
                body = b"".join(chunks)
            elif length is not None:
                if length > self.max_body:
                    raise ProtocolError("body exceeds limit")
                body = self.take(length)
            else:
                while not self.eof:
                    if len(self.buffer) > self.max_body:
                        raise ProtocolError("body exceeds limit")
                    self._receive()
                if len(self.buffer) > self.max_body:
                    raise ProtocolError("body exceeds limit")
                body = self.take(len(self.buffer))
        return Response(version, status, headers, body, trailers, bytes(self._wire))

    def final_response(self, method: str = "GET") -> tuple[list[Response], Response]:
        interim = []
        for _ in range(17):
            response = self.response(method)
            if response.status >= 200 or response.status == 101:
                return interim, response
            interim.append(response)
        raise ProtocolError("too many informational responses")

    def expect_eof(self, *, allow_reset: bool = False) -> None:
        if self.buffer:
            raise ProtocolError(f"unexpected trailing bytes: {bytes(self.buffer[:80])!r}")
        if not self.eof:
            try:
                self._receive()
            except ConnectionResetError:
                if not allow_reset:
                    raise
                self.eof = True
        if self.buffer or not self.eof:
            raise ProtocolError("expected EOF without trailing bytes")


def request(
    target: str = "/fixture.bin",
    *,
    method: str = "GET",
    close: bool = False,
    version: str = "1.1",
) -> bytes:
    return (
        f"{method} {target} HTTP/{version}\r\nHost: localhost\r\n"
        + ("Connection: close\r\n" if close else "")
        + "\r\n"
    ).encode("ascii")
