"""Tests for fixture-route tracing and failure reporting."""

import base64
import socket
import unittest
from unittest.mock import patch

from fixture_server_suite import CASES, TraceWire, probe, run_suite
from harness import ProtocolError, request
from scripted import ScriptedPeer
from server_suite import EAGER_NEGATIVES, run_case as static_case


class FixtureSuiteTests(unittest.TestCase):
    def test_static_eager_prefixes_forward_the_short_deadline(self):
        for name in EAGER_NEGATIVES:
            with self.subTest(name=name):
                with patch("server_suite.Wire.connect", side_effect=ConnectionRefusedError) as connect:
                    with self.assertRaises(ConnectionRefusedError):
                        static_case(("127.0.0.1", 1), name)
                    connect.assert_called_once_with(("127.0.0.1", 1), timeout=1)

    def test_wire_trace_preserves_requests_and_responses(self):
        response = b"HTTP/1.1 204 No Content\r\n\r\n"

        def serve(wire, _index):
            self.assertEqual(wire.until(b"\r\n\r\n", 4096), request(close=True))
            wire.send(response, chunk_size=1)
            wire.expect_eof()

        with ScriptedPeer(serve) as peer:
            with TraceWire.connect(peer.address) as wire:
                wire.send(request(close=True), chunk_size=1)
                self.assertEqual(wire.response().status, 204)
            received = b"".join(
                base64.b64decode(row["base64"])
                for row in wire.trace if row["event"] == "receive"
            )
            self.assertEqual(received, response)
            self.assertEqual(base64.b64decode(wire.trace[0]["base64"]), request(close=True))
            self.assertTrue(wire.trace[0]["completed"])

    def test_connection_failures_do_not_lose_the_report(self):
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as unavailable:
            unavailable.bind(("127.0.0.1", 0))
            results = run_suite(unavailable.getsockname())
        self.assertEqual([row["case"] for row in results], list(CASES))
        self.assertTrue(all(not row["passed"] and row["error"] for row in results))

    def test_early_rejection_does_not_ignore_premature_continue(self):
        def serve(wire, _index):
            wire.until(b"\r\n\r\n", 4096)
            wire.send(
                b"HTTP/1.1 100 Continue\r\n\r\n"
                b"HTTP/1.1 413 Content Too Large\r\nContent-Length: 0\r\n\r\n"
            )
            wire.expect_eof()

        with ScriptedPeer(serve) as peer:
            with TraceWire.connect(peer.address) as wire:
                with self.assertRaisesRegex(ProtocolError, "expected 413, received 100"):
                    probe(wire, "early-expect")

    def test_timeout_response_is_not_malformed_request_rejection(self):
        def serve(wire, _index):
            wire.until(b"\r\n\r\n", 4096)
            wire.send(b"HTTP/1.1 408 Request Timeout\r\nContent-Length: 0\r\n\r\n")

        with ScriptedPeer(serve) as peer:
            with TraceWire.connect(peer.address) as wire:
                with self.assertRaisesRegex(ProtocolError, "needs 400, received 408"):
                    probe(wire, "missing-host")

    def test_http10_does_not_accept_wrong_version_or_chunking(self):
        body = b"hello from kimojio-http1\n"
        for response in (
            b"HTTP/1.1 200 OK\r\nContent-Length: " + str(len(body)).encode() + b"\r\n\r\n" + body,
            b"HTTP/1.0 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n"
            + f"{len(body):x}\r\n".encode() + body + b"\r\n0\r\n\r\n",
        ):
            with self.subTest(response=response):
                def serve(wire, _index):
                    wire.until(b"\r\n\r\n", 4096)
                    wire.send(response)

                with ScriptedPeer(serve) as peer:
                    with TraceWire.connect(peer.address) as wire:
                        with self.assertRaises(ProtocolError):
                            probe(wire, "http10-close")

    def test_eager_prefixes_send_neither_suffix_nor_eof(self):
        for name, prefix in EAGER_NEGATIVES.items():
            for suite in ("static", "fixture"):
                with self.subTest(name=name, suite=suite):
                    def serve(wire, _index):
                        self.assertEqual(wire.take(len(prefix)), prefix)
                        self.assertEqual(wire.buffer, b"")
                        wire.sock.settimeout(0.05)
                        with self.assertRaises(TimeoutError):
                            wire.sock.recv(1, socket.MSG_PEEK)
                        wire.send(
                            b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n"
                            b"Connection: close\r\n\r\n"
                        )

                    with ScriptedPeer(serve) as peer:
                        if suite == "static":
                            static_case(peer.address, name)
                        else:
                            with TraceWire.connect(peer.address, timeout=1) as wire:
                                probe(wire, name)


if __name__ == "__main__":
    unittest.main()
