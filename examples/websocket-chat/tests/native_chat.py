"""Actual-wire tests for the native executable. No server or protocol substitute."""
import base64
import os
import socket
import struct
import subprocess
import sys
import threading
import time


class Peer:
    def __init__(self, address, prefix=b"", receive_buffer=None):
        self.socket = socket.socket()
        if receive_buffer:
            self.socket.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, receive_buffer)
        self.socket.settimeout(3)
        self.socket.connect(address)
        self.buffer = bytearray()
        key = base64.b64encode(bytes(range(16)))
        request = (
            b"GET /chat HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\n"
            b"Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: "
            + key + b"\r\n\r\n"
        )
        self.socket.sendall(request + prefix)
        while b"\r\n\r\n" not in self.buffer:
            self.buffer.extend(self.socket.recv(4096))
        head, remaining = bytes(self.buffer).split(b"\r\n\r\n", 1)
        assert head.startswith(b"HTTP/1.1 101 "), head
        self.buffer = bytearray(remaining)

    @staticmethod
    def frame(kind, data, fin=True):
        mask = b"\x13\x27\x45\x61"
        size = len(data)
        header = bytes([(0x80 if fin else 0) | kind])
        if size < 126:
            header += bytes([0x80 | size])
        elif size < 65536:
            header += b"\xfe" + struct.pack("!H", size)
        else:
            header += b"\xff" + struct.pack("!Q", size)
        return header + mask + bytes(byte ^ mask[index % 4] for index, byte in enumerate(data))

    def send(self, kind, data, fin=True):
        self.socket.sendall(self.frame(kind, data, fin))

    def exact(self, count):
        while len(self.buffer) < count:
            chunk = self.socket.recv(max(4096, count - len(self.buffer)))
            if not chunk:
                raise AssertionError("early transport EOF")
            self.buffer.extend(chunk)
        value = bytes(self.buffer[:count])
        del self.buffer[:count]
        return value

    def event(self, frames=None):
        message = bytearray()
        kind = None
        for _ in range(1024):
            header = self.exact(2)
            assert not header[0] & 0x70, "unexpected reserved frame bits"
            assert not header[1] & 0x80, "server frame is masked"
            size = header[1] & 127
            if size == 126:
                size = struct.unpack("!H", self.exact(2))[0]
            elif size == 127:
                size = struct.unpack("!Q", self.exact(8))[0]
            assert size <= 1024 * 1024, "unbounded server frame"
            payload = self.exact(size)
            opcode = header[0] & 15
            if frames is not None:
                frames.append((bool(header[0] & 0x80), opcode, size))
            if opcode >= 8:
                assert header[0] & 0x80 and size <= 125, "invalid control frame"
                return opcode, payload
            if kind is None:
                assert opcode in (1, 2)
                kind = opcode
            else:
                assert opcode == 0
            message.extend(payload)
            if header[0] & 0x80:
                return kind, bytes(message)
        raise AssertionError("unbounded fragment count")

    def reset(self):
        self.socket.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
        self.socket.close()


def run(binary):
    process = subprocess.Popen(
        [binary, "--bind", "127.0.0.1:0", "--run-for-ms", "3000",
         "--shutdown-grace-ms", "100", "--max-clients", "4",
         "--max-queued-messages", "2", "--max-message-bytes", "65536",
         "--send-buffer-bytes", "4096", "--write-timeout-ms", "1500",
         "--close-timeout-ms", "500"],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )
    peers = []
    try:
        line = process.stdout.readline().strip()
        assert line.startswith("LISTEN "), line
        host, port = line.split()[1].rsplit(":", 1)
        address = host, int(port)
        fd_baseline = len(os.listdir(f"/proc/{process.pid}/fd"))
        first = Peer(address, Peer.frame(1, b"coalesced"))
        peers.append(first)
        assert first.event() == (1, b"coalesced")
        second = Peer(address)
        third = Peer(address)
        peers.extend([second, third])

        threads = [
            threading.Thread(target=peer.send, args=(kind, data))
            for peer, kind, data in [(first, 1, b"from-first"), (second, 2, b"from-second")]
        ]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
        observed = [[peer.event(), peer.event()] for peer in peers]
        assert observed[0] == observed[1] == observed[2], observed
        assert set(observed[0]) == {(1, b"from-first"), (2, b"from-second")}

        first.send(1, b"\xf0\x9f", fin=False)
        first.send(0, b"\x98\x80")
        for peer in peers:
            assert peer.event() == (1, "😀".encode())
        first.send(2, b"")
        for peer in peers:
            assert peer.event() == (2, b"")
        first.send(9, b"ping")
        assert first.event() == (10, b"ping")

        third.send(1, b"not complete", fin=False)
        third.send(0, b"\xff")
        opcode, payload = third.event()
        assert opcode == 8 and struct.unpack("!H", payload[:2])[0] == 1007
        third.socket.close()
        peers.remove(third)
        second.send(1, b"after-invalid")
        for peer in peers:
            assert peer.event() == (1, b"after-invalid")

        second.reset()
        peers.remove(second)
        # The two-client broadcast below includes the sender. The other peer
        # keeps its receive window small and does not read during publication.
        slow = Peer(address, receive_buffer=4096)
        peers.append(slow)
        for sequence in range(4):
            data = struct.pack("!I", sequence) + bytes([sequence]) * 32764
            first.send(2, data)
            assert first.event() == (2, data)
        for _ in range(10):
            opcode, payload = slow.event()
            if opcode == 8:
                assert struct.unpack("!H", payload[:2])[0] == 1008
                break
        else:
            raise AssertionError("slow recipient did not receive policy close")
        slow.socket.close()
        peers.remove(slow)
        first.send(1, b"still healthy")
        assert first.event() == (1, b"still healthy")
        first.send(8, struct.pack("!H", 1000))
        opcode, payload = first.event()
        assert opcode == 8 and payload == struct.pack("!H", 1000)
        first.socket.close()
        peers.clear()

        # New admission after original read/write/cancel/close settlement.
        recovered = Peer(address)
        peers.append(recovered)
        recovered.send(1, b"recovered")
        assert recovered.event() == (1, b"recovered")
        recovered.reset()
        peers.clear()
        deadline = time.monotonic() + 1
        while time.monotonic() < deadline:
            if len(os.listdir(f"/proc/{process.pid}/fd")) == fd_baseline:
                break
            time.sleep(0.005)
        fd_final = len(os.listdir(f"/proc/{process.pid}/fd"))
        assert fd_final == fd_baseline, (fd_baseline, fd_final)
        _, error = process.communicate(timeout=5)
        assert process.returncode == 0, error
        assert "STOP clients=0" in error, error
        print(f"PASS native chat: coalesced handoff, concurrent order, sender, UTF-8, empty, ping, slow1008, reset, recovery, bounded shutdown; FDs {fd_baseline}->{fd_final}")
    finally:
        for peer in peers:
            peer.socket.close()
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


def run_exact_message_boundary(binary):
    process = subprocess.Popen(
        [binary, "--bind", "127.0.0.1:0", "--run-for-ms", "2000",
         "--shutdown-grace-ms", "100", "--max-clients", "2",
         "--max-message-bytes", "20000", "--frame-bytes", "16384"],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )
    peers = []
    try:
        line = process.stdout.readline().strip()
        assert line.startswith("LISTEN "), line
        host, port = line.split()[1].rsplit(":", 1)
        address = host, int(port)
        fd_baseline = len(os.listdir(f"/proc/{process.pid}/fd"))
        sender = Peer(address)
        peers.append(sender)
        recipient = Peer(address)
        peers.append(recipient)

        # The CLI sets the outbound limit to 20,000. Its receive buffer remains
        # 16KiB, so this one masked frame requires incremental native reads.
        payload = bytes(index % 251 for index in range(20000))
        sender.send(2, payload)
        for peer in peers:
            fragments = []
            assert peer.event(fragments) == (2, payload)
            assert fragments == [(False, 2, 16384), (True, 0, 3616)], fragments

        follow_up = b"healthy after exact maximum"
        recipient.send(1, follow_up)
        for peer in peers:
            fragments = []
            assert peer.event(fragments) == (1, follow_up)
            assert fragments == [(True, 1, len(follow_up))], fragments

        for peer in peers:
            peer.send(8, struct.pack("!H", 1000))
            fragments = []
            assert peer.event(fragments) == (8, struct.pack("!H", 1000))
            assert fragments == [(True, 8, 2)], fragments
            assert not peer.buffer
            assert peer.socket.recv(1) == b"", "server did not finish normal close"
            peer.socket.close()
        peers.clear()
        assert len(os.listdir(f"/proc/{process.pid}/fd")) == fd_baseline
        _, error = process.communicate(timeout=4)
        assert process.returncode == 0, error
        assert "STOP clients=0" in error, error
        print("PASS native exact20,000 limit: 16KiB incremental receive, exact sender+peer broadcast, legal binary/continuation frames, healthy text broadcast, normal1000 closes")
    finally:
        for peer in peers:
            peer.socket.close()
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


if __name__ == "__main__":
    executable = os.path.abspath(sys.argv[1])
    run(executable)
    run_exact_message_boundary(executable)
