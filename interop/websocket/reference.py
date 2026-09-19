"""Bounded Python-only broadcast reference, used to test the test harness."""

from __future__ import annotations

import contextlib
import argparse
import socket
import threading
import time

from ws_wire import (
    Messages, WebSocketError, Wire, close_payload, encode_frame,
    read_client_upgrade, read_frame, upgrade_response,
)


class BroadcastReference:
    def __init__(self, *, include_sender: bool = True, timeout: float = 5):
        self.include_sender = include_sender
        self.timeout = timeout
        self.listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(8)
        self.listener.settimeout(0.1)
        self.address = self.listener.getsockname()
        self.stopping = threading.Event()
        self.lock = threading.Lock()
        self.clients = {}
        self.sockets = []
        self.workers = []
        self.errors = []
        self.thread = threading.Thread(target=self._accept, daemon=True)

    def _accept(self):
        while not self.stopping.is_set():
            try:
                connection, _address = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            with self.lock:
                if self.stopping.is_set():
                    connection.close()
                    return
                if len(self.workers) >= 32:
                    connection.close()
                    self.errors.append("reference connection limit exceeded")
                    continue
                self.sockets.append(connection)
                worker = threading.Thread(target=self._serve, args=(connection,), daemon=True)
                self.workers.append(worker)
                worker.start()

    def _serve(self, connection):
        wire = Wire(connection, timeout=self.timeout)
        write_lock = threading.Lock()
        upgraded = False
        try:
            key = read_client_upgrade(wire)
            wire.send(upgrade_response(key))
            upgraded = True
            with self.lock:
                self.clients[id(wire)] = (wire, write_lock)
            messages = Messages()
            while not self.stopping.is_set():
                event = messages.consume(read_frame(wire, expect_mask=True))
                if event is None:
                    continue
                kind, payload = event
                if kind == "close":
                    with write_lock:
                        wire.send(encode_frame(8, payload))
                    break
                if kind == "ping":
                    with write_lock:
                        wire.send(encode_frame(10, payload))
                    continue
                if kind == "pong":
                    continue
                with self.lock:
                    recipients = list(self.clients.values())
                    for recipient, recipient_lock in recipients:
                        if not self.include_sender and recipient is wire:
                            continue
                        with recipient_lock:
                            recipient.send(encode_frame(1 if kind == "text" else 2, payload))
        except WebSocketError as error:
            with contextlib.suppress(OSError, AssertionError):
                with write_lock:
                    if upgraded:
                        wire.send(encode_frame(8, close_payload(error.close_code)))
                    else:
                        wire.send(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        except (OSError, AssertionError) as error:
            if not self.stopping.is_set():
                with self.lock:
                    self.errors.append(str(error))
        finally:
            with self.lock:
                self.clients.pop(id(wire), None)
            connection.close()

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, exc_type, *_args):
        self.stopping.set()
        self.listener.close()
        with self.lock:
            connections = list(self.sockets)
        for connection in connections:
            with contextlib.suppress(OSError):
                connection.shutdown(socket.SHUT_RDWR)
            connection.close()
        self.thread.join(timeout=1)
        for worker in self.workers:
            worker.join(timeout=1)
        if self.thread.is_alive() or any(worker.is_alive() for worker in self.workers):
            raise AssertionError("reference thread survived cleanup")
        if exc_type is None and self.errors:
            raise AssertionError(f"reference errors: {self.errors}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exclude-sender", action="store_true")
    parser.add_argument("--lifetime", type=float, default=10)
    args = parser.parse_args()
    if not 0 < args.lifetime <= 60:
        parser.error("lifetime must be positive and at most 60 seconds")
    with BroadcastReference(include_sender=not args.exclude_sender) as server:
        print(f"LISTEN {server.address[0]}:{server.address[1]}", flush=True)
        time.sleep(args.lifetime)


if __name__ == "__main__":
    main()
