"""Harness tests and explicit fault controls; these do not exercise Rust."""

import json
from pathlib import Path
import sys
import unittest
import uuid

from cases import Case, qualify, validate_results
from peer import Peer, digest_for
from reference import client, serve
from socket_peer import Channel, ScriptedPeer, Trace, require, run_command
from suite import ROOT, adapter, client_case, command, server_case


class SocketTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.directory = ROOT / "target" / "http2-interop-tests" / uuid.uuid4().hex
        cls.directory.mkdir(parents=True)
        source = str(Path(__file__).with_name("reference.py"))
        cls.client_adapter = {
            "command": [sys.executable, source, "client", "{request_file}", "{result_file}"],
        }
        cls.server_adapter = {
            "command": [sys.executable, source, "server", "{request_file}"],
        }

    def test_socket_both_roles(self):
        case = Case(
            "socket", 131087, config={"stream_window": 1024, "connection_window": 65535},
            qualification="single",
        )
        client_case(self.client_adapter, case, self.directory / "client")
        server_case(self.server_adapter, case, self.directory / "server", upload=True)

    def test_server_flags_preserve_default_omission(self):
        path = self.directory / "flags.json"
        spec = {
            "schema": 1, "command": [
                "fixture", "server", "--stream-window", "{stream_window}",
                "--connection-window", "{connection_window}",
            ],
        }
        path.write_text(json.dumps(spec))
        parsed = adapter(path, client_role=False)
        self.assertEqual(command(parsed, path)[3::2], ["default", "default"])
        self.assertEqual(
            command(parsed, path, config={"stream_window": 1024, "connection_window": 65535})[3::2],
            ["1024", "65535"],
        )
        spec["command"] = spec["command"][:4]
        path.write_text(json.dumps(spec))
        with self.assertRaisesRegex(AssertionError, "connection_window"):
            adapter(path, client_role=False)

    def test_early_success_does_not_end_upload_in_reference_client(self):
        from duplex_probe import run_probe

        report = run_probe(self.client_adapter, self.directory / "early-success", timeout_ms=3000)
        self.assertTrue(report["passed"], report)
        self.assertEqual(report["classification"], "completed-upload-after-final-response")
        self.assertFalse(report["peer"]["peer_resets"])
        self.assertFalse(report["peer"]["peer_goaway"])
        self.assertIsNotNone(report["peer"]["native_socket_close"])

    def test_early_413_without_cancel_action_does_not_end_upload(self):
        from duplex_probe import run_probe

        report = run_probe(
            self.client_adapter, self.directory / "early-413-no-action",
            timeout_ms=3000, status=413,
        )
        self.assertTrue(report["passed"], report)
        self.assertEqual(report["result"]["streams"][0]["outcome"], "complete")
        self.assertFalse(report["peer"]["native_resets"])

    def test_early_success_without_credit_is_an_explicit_negative_control(self):
        from duplex_probe import run_probe

        report = run_probe(
            self.client_adapter, self.directory / "early-success-no-credit",
            consume=False, timeout_ms=300,
        )
        self.assertFalse(report["passed"])
        self.assertEqual(report["classification"], "no-credit-negative-control")
        self.assertEqual(report["peer"]["received_bytes"], 65535)
        self.assertFalse(report["peer"]["request_ended"])
        self.assertFalse(report["peer"]["window_updates_sent"])

    def test_requests_before_handshake_barrier_are_not_lost(self):
        import socket

        observations = []

        def handler(wire, _index):
            observations.append(serve(wire.sock, config={}, timeout=3))

        with ScriptedPeer(handler, timeout=3) as server:
            peer = Peer(client=True)
            peer.request(1, "/bytes/37")
            channel = Channel(socket.create_connection(server.address), peer, timeout=3)
            try:
                channel.handshake()
                while not channel.results.get(1, {}).get("ended"):
                    channel.step()
                channel.flush()
            finally:
                channel.close()
            validate_results(Case("early-headers", 37), channel.report())
        self.assertEqual(observations[0]["requests"], 1)

    def test_echo_headers_and_data_precede_request_end(self):
        import socket

        observations = []

        def handler(wire, _index):
            observations.append(serve(wire.sock, config={"stream_window": 1024}, timeout=3))

        with ScriptedPeer(handler, timeout=3) as server:
            peer = Peer(client=True, stream_window=1024)
            channel = Channel(socket.create_connection(server.address), peer, timeout=3)
            try:
                channel.handshake()
                peer.connection.send_headers(1, [
                    (b":method", b"POST"), (b":scheme", b"http"),
                    (b":authority", b"localhost"), (b":path", b"/echo"),
                    (b"content-length", b"37"),
                ])
                while channel.results.get(1, {}).get("status") is None:
                    channel.step()
                self.assertEqual(channel.results[1]["status"], 200)
                self.assertNotIn(1, peer.received)
                peer.connection.send_data(1, b"\x01" * 17)
                while 1 not in peer.received or peer.received[1].payload < 17:
                    channel.step()
                self.assertFalse(peer.received[1].ended)
                peer.connection.send_data(1, b"\x01" * 20, end_stream=True)
                while not peer.received[1].ended:
                    channel.step()
                channel.flush()
            finally:
                channel.close()
            validate_results(Case("echo", 37), channel.report(), echo=True)
        self.assertEqual(observations[0]["echo_retention"]["bytes"], 20)
        self.assertEqual(observations[0]["echo_retention"]["items"], 1)

    def test_actual_eight_megabyte_windows(self):
        case = Case(
            "8mib", 16 * 1024 * 1024 + 17,
            config={"stream_window": 8 * 1024 * 1024, "connection_window": 8 * 1024 * 1024},
            qualification="single",
        )
        client_case(self.client_adapter, case, self.directory / "client-8mib")
        server_case(self.server_adapter, case, self.directory / "server-8mib", upload=True)

    def test_actual_eight_megabyte_aggregate_serial_and_concurrent(self):
        for concurrency in (1, 8):
            case = Case(
                f"8mib-{concurrency}", 32768, 600, concurrency,
                {"stream_window": 8 * 1024 * 1024, "connection_window": 8 * 1024 * 1024},
                qualification="aggregate",
            )
            client_case(self.client_adapter, case, self.directory / f"aggregate-client-{concurrency}")
            server_case(self.server_adapter, case, self.directory / f"aggregate-server-{concurrency}", upload=True)

    def no_credit_server(self, *, stream_window, length, count):
        observed = []

        def handler(wire, _index):
            peer = Peer(client=False, stream_window=stream_window)
            channel = Channel(wire.sock, peer, timeout=3, consume=False)
            try:
                channel.handshake()
                while not channel.eof:
                    channel.step()
                observed.append({
                    "bytes": sum(received.payload for received in peer.received.values()),
                    "updates": dict(channel.outbound.updates),
                    "ended": sum(received.ended for received in peer.received.values()),
                })
            finally:
                channel.close()

        with ScriptedPeer(handler, timeout=3) as server:
            case = Case("no-credit", length, count, count)
            spec = case.spec(server.address, upload=True)
            spec["timeout_ms"] = 300
            with self.assertRaises(TimeoutError):
                client(spec)
        self.assertEqual(len(observed), 1)
        self.assertEqual(observed[0]["updates"], {})
        return observed[0]

    def test_no_credit_server_stops_exact_advertised_stream(self):
        observed = self.no_credit_server(stream_window=1024, length=131087, count=1)
        self.assertEqual(observed["bytes"], 1024)
        self.assertEqual(observed["ended"], 0)

    def test_no_credit_server_stops_exact_connection_aggregate(self):
        observed = self.no_credit_server(stream_window=65535, length=32768, count=8)
        self.assertEqual(observed["bytes"], 65535)

    def test_buffered_echo_server_is_rejected_by_duplex_probe(self):
        observed = []

        def handler(wire, _index):
            peer = Peer(client=False)
            channel = Channel(wire.sock, peer, timeout=3)
            try:
                channel.handshake()
                while not channel.eof:
                    channel.step()
                observed.append((peer.received[1].payload, peer.received[1].ended))
            finally:
                channel.close()

        with ScriptedPeer(handler, timeout=3) as server:
            spec = Case("buffered-echo", 131087).spec(server.address, upload=True)
            spec["timeout_ms"] = 300
            with self.assertRaises(TimeoutError):
                client(spec)
        self.assertEqual(observed, [(16384, False)])

    def test_small_workload_cannot_qualify_large_windows(self):
        credit = {
            "initial_connection": 8 * 1024 * 1024, "connection_window_update": 1,
            "streams": [{"initial": 8 * 1024 * 1024, "window_update": 1}],
        }
        with self.assertRaisesRegex(AssertionError, "twice BOTH"):
            qualify(Case("bad", 131071, qualification="single"), credit)
        with self.assertRaisesRegex(AssertionError, "twice actual"):
            qualify(Case("bad", 512, 160, qualification="aggregate"), credit)

    def test_default_over_eight_megabytes_fails_loudly(self):
        credit = {
            "initial_connection": 9 * 1024 * 1024, "connection_window_update": 1,
            "streams": [{"initial": 65535, "window_update": 1}],
        }
        with self.assertRaisesRegex(AssertionError, "8 MiB"):
            qualify(Case("default-aggregate", 32768, 600, qualification="aggregate"), credit)

    def test_result_hash_end_and_close_are_required(self):
        case = Case("result", 3)
        result = {
            "schema": 1, "connection": {"closed": True, "error": None, "outcome": "graceful"},
            "streams": [{
                "stream_id": 1, "status": 200, "bytes": 3,
                "content_length": 3,
                "sha256": digest_for(1, 3), "trailers": [], "informational": [],
                "ended": True, "outcome": "complete", "error": None,
            }],
        }
        validate_results(case, result)
        for field, value in (("bytes", 2), ("sha256", "0" * 64), ("ended", False), ("trailers", [["x", "y"]])):
            broken = json.loads(json.dumps(result))
            broken["streams"][0][field] = value
            with self.assertRaises(AssertionError):
                validate_results(case, broken)
        result["connection"]["closed"] = False
        with self.assertRaisesRegex(AssertionError, "explicitly close"):
            validate_results(case, result)

    def test_receive_success_cannot_hide_failed_terminal_outcomes(self):
        case = Case("terminal", 3)
        report = {
            "schema": 1, "connection": {"closed": True, "error": None, "outcome": "graceful"},
            "streams": [{
                "stream_id": 1, "status": 200, "content_length": 3,
                "bytes": 3, "sha256": digest_for(1, 3),
                "trailers": [], "informational": [], "ended": True,
                "outcome": "complete", "error": None,
            }],
        }
        validate_results(case, report)
        for outcome in ("connection_failed", "unprocessed", "deadline", "reset", "source_failed", None):
            broken = json.loads(json.dumps(report))
            broken["streams"][0]["outcome"] = outcome
            with self.assertRaisesRegex(AssertionError, "terminal outcome"):
                validate_results(case, broken)
        for outcome in ("aborted", "io_failed", "resource_exhausted", "protocol", None):
            broken = json.loads(json.dumps(report))
            broken["connection"]["outcome"] = outcome
            with self.assertRaisesRegex(AssertionError, "connection terminal outcome"):
                validate_results(case, broken)
        for target in ("stream", "connection"):
            broken = json.loads(json.dumps(report))
            del (broken["streams"][0] if target == "stream" else broken["connection"])["outcome"]
            with self.assertRaisesRegex(AssertionError, "terminal outcome"):
                validate_results(case, broken)
        report["connection"]["outcome"] = "peer_closed"
        validate_results(case, report)

    def test_head_requires_declared_length_despite_empty_body(self):
        case = Case("head", 999, method="HEAD")
        result = {
            "schema": 1, "connection": {"closed": True, "error": None, "outcome": "graceful"},
            "streams": [{
                "stream_id": 1, "status": 200, "content_length": 999,
                "bytes": 0, "sha256": digest_for(1, 0), "trailers": [],
                "informational": [], "ended": True, "outcome": "complete", "error": None,
            }],
        }
        validate_results(case, result)
        result["streams"][0]["content_length"] = 0
        with self.assertRaisesRegex(AssertionError, "declared content length"):
            validate_results(case, result)

    def test_empty_frame_resource_limit_requires_successful_sibling(self):
        case = Case("empty-data-sibling", 4096, 2, 2)
        report = {
            "schema": 1, "connection": {"closed": True, "error": None, "outcome": "graceful"},
            "streams": [{
                "stream_id": stream, "status": 200, "content_length": 4096,
                "bytes": 0 if stream == 1 else 4096,
                "sha256": digest_for(stream, 0 if stream == 1 else 4096),
                "trailers": [], "informational": [], "ended": stream == 3,
                "outcome": "reset" if stream == 1 else "complete",
                "error": {"scope": "stream", "code": 11} if stream == 1 else None,
            } for stream in (1, 3)],
        }
        validate_results(case, report)
        broken = json.loads(json.dumps(report))
        broken["streams"][0]["outcome"] = "connection_failed"
        with self.assertRaisesRegex(AssertionError, "terminal outcome"):
            validate_results(case, broken)
        for stream_index in (0, 1):
            broken = json.loads(json.dumps(report))
            broken["streams"][stream_index]["error"] = {"scope": "stream", "code": 8}
            with self.assertRaises(AssertionError):
                validate_results(case, broken)
        report["connection"]["error"] = {"scope": "connection", "code": 11}
        with self.assertRaises(AssertionError):
            validate_results(case, report)

    def test_early_rejection_requires_peer_signal_not_global_failure(self):
        from protocol_suite import request, validate

        spec = request("early-response", ("127.0.0.1", 1))
        self.assertEqual(spec["actions"], [])

        report = {
            "schema": 1, "connection": {"closed": True, "error": None, "outcome": "graceful"},
            "streams": [{
                "stream_id": stream, "status": 413 if stream == 1 else 200,
                "content_length": 0 if stream == 1 else 37,
                "bytes": 0 if stream == 1 else 37,
                "sha256": digest_for(stream, 0 if stream == 1 else 37),
                "trailers": [], "informational": [], "ended": True,
                "outcome": "reset" if stream == 1 else "complete",
                "error": {"scope": "stream", "code": 0} if stream == 1 else None,
            } for stream in (1, 3)],
        }
        witness = {
            "peer_eof": True, "requests": 2, "received": {"1": 1024},
            "window_updates": {}, "resets": {}, "server_resets": {"1": 0},
            "early_response_barrier": True,
            "request_ended": {"1": False, "3": True},
        }
        validate("early-response", report, witness)
        broken = json.loads(json.dumps(report))
        broken["streams"][0]["outcome"] = "connection_failed"
        with self.assertRaisesRegex(AssertionError, "terminal outcome"):
            validate("early-response", broken, witness)
        broken["streams"][0]["error"] = None
        with self.assertRaises(AssertionError):
            validate("early-response", broken, witness)
        witness["early_response_barrier"] = False
        with self.assertRaisesRegex(AssertionError, "barrier"):
            validate("early-response", report, witness)
        witness["early_response_barrier"] = True
        witness["server_resets"] = {}
        with self.assertRaisesRegex(AssertionError, "RST_STREAM\\(NO_ERROR\\)"):
            validate("early-response", report, witness)

    def test_bounded_process_output_and_timeout(self):
        with self.assertRaisesRegex(AssertionError, "output exceeds"):
            run_command(
                [sys.executable, "-c", "import os; os.write(1, b'x' * 65536)"],
                cwd=ROOT, max_output=1024,
            )
        with self.assertRaises(TimeoutError):
            run_command(
                [sys.executable, "-c", "import time; time.sleep(5)"], cwd=ROOT, timeout=0.1,
            )

    def test_trace_fragmentation_padding_and_counters(self):
        from hyperframe.frame import DataFrame, WindowUpdateFrame
        data = DataFrame(1)
        data.data = b"abc"
        data.flags.add("PADDED")
        data.pad_length = 17
        update = WindowUpdateFrame(0)
        update.window_increment = 21
        trace = Trace()
        for byte in data.serialize() + update.serialize():
            trace.feed(bytes([byte]))
        self.assertEqual(trace.payload[1], 3)
        self.assertEqual(trace.flow[1], 21)
        self.assertEqual(trace.updates[0], 21)
        self.assertFalse(trace.buffer)


if __name__ == "__main__":
    unittest.main()
