"""Self-tests for the independently encoded native chat application checks."""

import socket
import unittest

from chat_suite import (
    binary_boundaries, bytewise_upgrade_and_frames, concurrent_publishers,
    strict_handshakes, validated_only,
)
from harness import ProtocolError
from reference import BroadcastReference
from scripted import ScriptedPeer


class ChatContractTests(unittest.TestCase):
    def test_binary_boundaries_against_reference(self):
        with BroadcastReference() as server:
            result = binary_boundaries(server.address)
            self.assertEqual(result["binary_lengths"], [0, 1, 125, 126, 65535, 65536])

    def test_invalid_fragments_never_broadcast_against_reference(self):
        with BroadcastReference() as server:
            self.assertTrue(validated_only(server.address)["recipient_sentinel_exact"])

    def test_bytewise_exchange_against_reference(self):
        with BroadcastReference() as server:
            self.assertEqual(bytewise_upgrade_and_frames(server.address)["write_chunk_bytes"], 1)

    def test_concurrent_publishers_against_reference(self):
        with BroadcastReference() as server:
            self.assertEqual(concurrent_publishers(server.address)["recipients"], 3)

    def test_strict_handshake_response_contract(self):
        def serve(wire, index):
            wire.until(b"\r\n\r\n", 65536)
            status = b"426 Upgrade Required" if index == 0 else b"400 Bad Request"
            extra = b"Sec-WebSocket-Version: 13\r\n" if index == 0 else b""
            wire.send(b"HTTP/1.1 " + status + b"\r\n" + extra + b"Content-Length: 0\r\nConnection: close\r\n\r\n")
            wire.sock.shutdown(socket.SHUT_WR)
            wire.expect_eof()

        with ScriptedPeer(serve, connections=4) as peer:
            result = strict_handshakes(peer.address)
            self.assertEqual(result["handshake-wrong-version"], 426)
            self.assertEqual(len(result), 4)

    def test_wrong_version_requires_426_and_version_header(self):
        for response in (
            b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n",
            b"HTTP/1.1 426 Upgrade Required\r\nContent-Length: 0\r\n\r\n",
            b"HTTP/1.1 426 Upgrade Required\r\nSec-WebSocket-Version: 12\r\nContent-Length: 0\r\n\r\n",
        ):
            def serve(wire, _index):
                wire.until(b"\r\n\r\n", 65536)
                wire.send(response)
                wire.sock.shutdown(socket.SHUT_WR)
                wire.expect_eof()

            with self.subTest(response=response), ScriptedPeer(serve) as peer:
                with self.assertRaises(ProtocolError):
                    strict_handshakes(peer.address)


if __name__ == "__main__":
    unittest.main()
