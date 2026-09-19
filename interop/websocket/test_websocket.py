"""Independent WebSocket harness self-tests, not Rust target tests."""

from __future__ import annotations

import json
from pathlib import Path
import shutil
import socket
import sys
import unittest

from ws_wire import (
    KEY, Messages, WebSocketError, Wire, accept_key, check_close,
    check_server_upgrade, client_frame, close_payload, encode_frame,
    read_client_upgrade, read_frame, upgrade_request, upgrade_response,
)
from harness import ProtocolError, run_command
from scripted import ScriptedPeer
from reference import BroadcastReference
from ws_suite import event as receive_event, run_suite


WORKSPACE = Path(__file__).resolve().parents[2]


class MemorySocket:
    def __init__(self, parts):
        self.parts = list(parts)

    def settimeout(self, _timeout):
        pass

    def recv(self, count):
        while self.parts and not self.parts[0]:
            self.parts.pop(0)
        if not self.parts:
            return b""
        result = self.parts[0][:count]
        self.parts[0] = self.parts[0][count:]
        return result


def decode(raw, *, masked=False):
    return read_frame(Wire(MemorySocket([raw])), expect_mask=masked)


class FrameTests(unittest.TestCase):
    def test_language_neutral_rfc_vectors_every_split(self):
        corpus = json.loads((Path(__file__).parent / "frame_cases.json").read_text())
        for case in corpus["cases"]:
            raw = bytes.fromhex(case["hex"])
            for cut in range(len(raw) + 1):
                with self.subTest(case=case["name"], cut=cut):
                    wire = Wire(MemorySocket([raw[:cut], raw[cut:]]))
                    if "error_code" in case:
                        with self.assertRaises(WebSocketError) as caught:
                            read_frame(wire, expect_mask=case["expect_mask"])
                        self.assertEqual(caught.exception.close_code, case["error_code"])
                    else:
                        frame = read_frame(wire, expect_mask=case["expect_mask"])
                        self.assertEqual(frame.wire, raw)
                        self.assertEqual(frame.opcode, case["opcode"])
                        self.assertEqual(frame.payload, bytes.fromhex(case["payload_hex"]))
                        self.assertEqual(frame.fin, case["fin"])
                        wire.expect_eof()

    def test_encoder_matches_independent_rfc_vector(self):
        self.assertEqual(
            encode_frame(1, b"Hello", mask=bytes.fromhex("37fa213d")),
            bytes.fromhex("818537fa213d7f9f4d5158"),
        )

    def test_length_boundaries(self):
        for size in (0, 1, 125, 126, 65535, 65536):
            for mask in (None, b"\x01\x02\x03\x04"):
                with self.subTest(size=size, mask=mask):
                    raw = encode_frame(2, b"x" * size, mask=mask)
                    for cut in sorted({0, 1, 2, min(4, len(raw)), min(10, len(raw)), len(raw)}):
                        wire = Wire(MemorySocket([raw[:cut], raw[cut:]]))
                        self.assertEqual(
                            read_frame(wire, expect_mask=mask is not None).payload, b"x" * size
                        )
                        wire.expect_eof()

    def test_every_eof_position_is_truncated(self):
        raw = encode_frame(1, b"masked payload", mask=b"abcd")
        for cut in range(len(raw)):
            with self.subTest(cut=cut), self.assertRaises(ProtocolError):
                decode(raw[:cut], masked=True)

    def test_interleaved_control_and_split_utf8(self):
        raw = (
            encode_frame(1, b"\xe2", fin=False)
            + encode_frame(9, b"ping")
            + encode_frame(10, b"pong")
            + encode_frame(0, b"\x82", fin=False)
            + encode_frame(0, b"\xac")
        )
        for cut in range(len(raw) + 1):
            wire = Wire(MemorySocket([raw[:cut], raw[cut:]]))
            messages = Messages()
            events = []
            for _ in range(5):
                event = messages.consume(read_frame(wire, expect_mask=False))
                if event is not None:
                    events.append(event)
            self.assertEqual(events, [("ping", b"ping"), ("pong", b"pong"), ("text", b"\xe2\x82\xac")])
            wire.expect_eof()

    def test_invalid_fragment_sequences_and_utf8(self):
        sequences = [
            [(0, b"x", True)],
            [(1, b"a", False), (1, b"b", True)],
            [(1, b"\xff", True)],
            [(1, b"\xe2", True)],
            [(1, b"\xe2", False), (0, b"x", True)],
        ]
        for sequence in sequences:
            with self.subTest(sequence=sequence), self.assertRaises(WebSocketError):
                messages = Messages()
                for opcode, payload, fin in sequence:
                    messages.consume(decode(encode_frame(opcode, payload, fin=fin)))

    def test_socket_event_helper_retains_fragment_state(self):
        wire = Wire(MemorySocket([
            encode_frame(1, b"first-", fin=False)
            + encode_frame(10, b"pong")
            + encode_frame(0, b"second")
        ]))
        self.assertEqual(receive_event(wire), ("pong", b"pong"))
        self.assertEqual(receive_event(wire), ("text", b"first-second"))
        wire.expect_eof()

    def test_close_payload_rules(self):
        for code in (1000, 1001, 1002, 1007, 1011, 3000, 4999):
            self.assertEqual(check_close(close_payload(code, "\u20ac")), (code, "\u20ac"))
        self.assertEqual(check_close(b""), (None, ""))
        for payload in (b"\x03", close_payload(1005), close_payload(1006), close_payload(1015), close_payload(5000), b"\x03\xe8\xff"):
            with self.subTest(payload=payload), self.assertRaises(WebSocketError):
                check_close(payload)

    def test_message_and_frame_limits(self):
        with self.assertRaises(WebSocketError) as caught:
            read_frame(Wire(MemorySocket([encode_frame(2, b"123")])), expect_mask=False, max_payload=2)
        self.assertEqual(caught.exception.close_code, 1009)
        messages = Messages(max_message=2)
        messages.consume(decode(encode_frame(2, b"12", fin=False)))
        with self.assertRaises(WebSocketError) as caught:
            messages.consume(decode(encode_frame(0, b"3")))
        self.assertEqual(caught.exception.close_code, 1009)


class HandshakeTests(unittest.TestCase):
    def test_rfc_handshake_accept(self):
        self.assertEqual(accept_key(KEY), b"s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")

    def test_upgrade_preserves_coalesced_frames_every_split(self):
        frame = encode_frame(1, b"first-server-message")
        raw = upgrade_response() + frame
        for cut in range(len(raw) + 1):
            wire = Wire(MemorySocket([raw[:cut], raw[cut:]]))
            check_server_upgrade(wire)
            self.assertEqual(read_frame(wire, expect_mask=False).wire, frame)
            wire.expect_eof()
        frame = client_frame(1, b"first-client-message")
        raw = upgrade_request() + frame
        for cut in range(len(raw) + 1):
            wire = Wire(MemorySocket([raw[:cut], raw[cut:]]))
            self.assertEqual(read_client_upgrade(wire), KEY)
            self.assertEqual(read_frame(wire, expect_mask=True).wire, frame)
            wire.expect_eof()

    def test_bad_accept_and_unsolicited_extensions(self):
        for raw in (
            upgrade_response().replace(b"s3pPLMBiTxaQ9kYGzzhZRbK+xOo=", b"wrong"),
            upgrade_response().replace(b"\r\n\r\n", b"\r\nSec-WebSocket-Extensions: permessage-deflate\r\n\r\n"),
            upgrade_response().replace(b"Connection: Upgrade", b"Connection: close"),
            upgrade_response().replace(b"\r\n\r\n", b"\r\nSec-WebSocket-Accept: duplicate\r\n\r\n"),
        ):
            with self.subTest(raw=raw), self.assertRaises(WebSocketError):
                check_server_upgrade(Wire(MemorySocket([raw])))
        for key in (b"bad", b"YQ=="):
            with self.assertRaises(WebSocketError):
                accept_key(key)


class SocketReferenceTests(unittest.TestCase):
    def test_raw_broadcast_suite_including_sender(self):
        with BroadcastReference() as server:
            results = run_suite(server.address, include_sender=True)
            self.assertEqual([row for row in results if not row["passed"]], [])
            self.assertEqual(len(results), 19)

    def test_raw_broadcast_suite_excluding_sender(self):
        with BroadcastReference(include_sender=False) as server:
            results = run_suite(server.address, include_sender=False)
            self.assertEqual([row for row in results if not row["passed"]], [])

    def test_node_builtin_against_fragmented_python_peer(self):
        node = shutil.which("node")
        if node is None:
            raise RuntimeError("Node is required for this positive peer test")
        observations = []

        def serve(wire, _index):
            key = read_client_upgrade(wire)
            wire.send(upgrade_response(key))
            message = read_frame(wire, expect_mask=True)
            self.assertEqual(message.opcode, 1)
            self.assertEqual(message.payload, b"node-independent")
            wire.send(
                encode_frame(1, message.payload[:3], fin=False)
                + encode_frame(9, b"automatic-pong")
                + encode_frame(0, message.payload[3:])
            )
            for _ in range(3):
                frame = read_frame(wire, expect_mask=True)
                observations.append(frame.opcode)
                if frame.opcode == 10:
                    self.assertEqual(frame.payload, b"automatic-pong")
                elif frame.opcode == 8:
                    self.assertEqual(check_close(frame.payload), (1000, "done"))
                    wire.send(encode_frame(8, frame.payload))
                    break
                else:
                    self.fail(f"unexpected Node opcode {frame.opcode}")
            wire.sock.shutdown(socket.SHUT_WR)
            wire.expect_eof()

        with ScriptedPeer(serve, timeout=7) as peer:
            result = run_command(
                [
                    node, "interop/websocket/node_peer.js",
                    f"ws://{peer.address[0]}:{peer.address[1]}/chat", "node-independent",
                ],
                cwd=WORKSPACE,
                timeout=7,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            report = json.loads(result.stdout)
            self.assertTrue(report["received"] and report["clean"])
        self.assertEqual(observations, [10, 8])


if __name__ == "__main__":
    unittest.main()
