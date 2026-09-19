"""Build and test the independent standard-library Go WebSocket peer."""

import json
from pathlib import Path
import socket
import unittest

from ws_wire import (
    check_close, encode_frame, read_client_upgrade, read_frame, upgrade_response,
)
from harness import run_command
from reference import BroadcastReference
from scripted import ScriptedPeer


WORKSPACE = Path(__file__).resolve().parents[2]
ARTIFACTS = WORKSPACE / "target" / "interop"
PEER = ARTIFACTS / "websocket-peer"


class GoPeerTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cache = ARTIFACTS / "go-cache"
        scratch = ARTIFACTS / "go-tmp"
        cache.mkdir(parents=True, exist_ok=True)
        scratch.mkdir(parents=True, exist_ok=True)
        result = run_command(
            [
                "env", f"GOCACHE={cache}", f"GOTMPDIR={scratch}", f"TMPDIR={scratch}",
                "GOTOOLCHAIN=local", "GOPROXY=off", "GOSUMDB=off", "go", "build",
                "-o", str(PEER), "interop/websocket/peer.go",
            ],
            cwd=WORKSPACE, timeout=60,
        )
        if result.returncode:
            raise RuntimeError(result.stderr.decode(errors="replace"))

    def run_peer(self, address):
        return run_command(
            [str(PEER), "--address", f"{address[0]}:{address[1]}"],
            cwd=WORKSPACE, timeout=7,
        )

    def test_go_against_reference(self):
        with BroadcastReference() as reference:
            result = self.run_peer(reference.address)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads(result.stdout)["binary_bytes"], 65792)

    def test_go_fragments_and_interleaved_ping(self):
        def serve(wire, _index):
            wire.send(upgrade_response(read_client_upgrade(wire)))
            text = read_frame(wire, expect_mask=True)
            self.assertEqual((text.opcode, text.payload), (1, "independent-Go-\u20ac".encode()))
            wire.send(
                encode_frame(1, text.payload[:2], fin=False)
                + encode_frame(9, b"automatic-pong")
                + encode_frame(0, text.payload[2:])
            )
            pong = read_frame(wire, expect_mask=True)
            self.assertEqual((pong.opcode, pong.payload), (10, b"automatic-pong"))
            binary = read_frame(wire, expect_mask=True)
            self.assertEqual((binary.opcode, binary.payload), (2, bytes(range(256)) * 257))
            wire.send(
                encode_frame(2, binary.payload[:32768], fin=False)
                + encode_frame(0, binary.payload[32768:])
            )
            ping = read_frame(wire, expect_mask=True)
            self.assertEqual((ping.opcode, ping.payload), (9, b"Go-ping"))
            wire.send(encode_frame(10, ping.payload))
            closing = read_frame(wire, expect_mask=True)
            self.assertEqual((closing.opcode, check_close(closing.payload)), (8, (1000, "done")))
            wire.send(encode_frame(8, closing.payload))
            wire.sock.shutdown(socket.SHUT_WR)
            wire.expect_eof()

        with ScriptedPeer(serve) as peer:
            result = self.run_peer(peer.address)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertTrue(json.loads(result.stdout)["clean_eof"])

    def test_go_rejects_masked_server_frame(self):
        def serve(wire, _index):
            wire.send(upgrade_response(read_client_upgrade(wire)))
            text = read_frame(wire, expect_mask=True)
            wire.send(encode_frame(1, text.payload, mask=b"abcd"))
            wire.expect_eof()

        with ScriptedPeer(serve) as peer:
            result = self.run_peer(peer.address)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(b"masked", result.stderr)


if __name__ == "__main__":
    unittest.main()
