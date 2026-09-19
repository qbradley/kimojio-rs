"""Generic independent server endpoint for future command-driven client tests."""

from __future__ import annotations

import contextlib
import socket
import threading
import time
from typing import Callable

from harness import Wire


class ScriptedPeer:
    """Accept a fixed connection count and propagate worker assertions to the caller."""

    def __init__(
        self,
        handler: Callable[[Wire, int], None],
        *,
        connections: int = 1,
        timeout: float = 5,
    ):
        self.handler = handler
        self.connections = connections
        self.timeout = timeout
        self.listener: socket.socket | None = None
        self.address: tuple[str, int] | None = None
        self.thread: threading.Thread | None = None
        self.error: BaseException | None = None
        self._active: socket.socket | None = None
        self._stopping = threading.Event()
        self._lock = threading.Lock()

    def _serve(self) -> None:
        deadline = time.monotonic() + self.timeout
        try:
            for index in range(self.connections):
                self.listener.settimeout(max(0.001, deadline - time.monotonic()))
                connection, _address = self.listener.accept()
                with self._lock:
                    self._active = connection
                    if self._stopping.is_set():
                        connection.close()
                        return
                with Wire(connection, timeout=max(0.001, deadline - time.monotonic())) as wire:
                    self.handler(wire, index)
                with self._lock:
                    self._active = None
        except BaseException as error:
            if not self._stopping.is_set():
                self.error = error

    def __enter__(self) -> "ScriptedPeer":
        self.listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(self.connections)
        self.address = self.listener.getsockname()
        self.thread = threading.Thread(target=self._serve, daemon=True)
        self.thread.start()
        return self

    def finish(self) -> None:
        self.thread.join(timeout=self.timeout)
        if self.thread.is_alive():
            raise TimeoutError("scripted peer did not finish")
        if self.error:
            raise self.error

    def close(self) -> None:
        self._stopping.set()
        with self._lock:
            if self._active is not None:
                with contextlib.suppress(OSError):
                    self._active.shutdown(socket.SHUT_RDWR)
                self._active.close()
            if self.listener is not None:
                with contextlib.suppress(OSError):
                    self.listener.shutdown(socket.SHUT_RDWR)
                self.listener.close()
        if self.thread is not None:
            self.thread.join(timeout=self.timeout)
            if self.thread.is_alive():
                raise TimeoutError("scripted peer thread survived cleanup")

    def __exit__(self, exc_type, *_args) -> None:
        try:
            if exc_type is None:
                self.finish()
        finally:
            self.close()
