"""Short correctness checks for the performance tools, not measurements."""

import json
import os
from pathlib import Path
import sys
import unittest

from run import ARTIFACTS, ROOT, cases, digest, load_manifest, snapshot, validate_cpus
sys.path.insert(0, str(ROOT / "interop" / "websocket"))
from reference import BroadcastReference
from ws_wire import encode_frame, read_client_upgrade, read_frame, upgrade_response
from scripted import ScriptedPeer
from harness import PeerProcess, run_command


class PerfTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.identity = json.loads((ARTIFACTS / "tool-publication.json").read_text())
        cls.tool = cls.identity["binary"]

    def test_published_binary_matches(self):
        self.assertEqual(digest(self.tool), self.identity["sha256"])

    def test_matrix_is_representative(self):
        matrix = cases()
        self.assertEqual(len(matrix), 16)
        self.assertEqual(len([row for row in matrix if row["kind"] == "ws"]), 6)
        self.assertEqual([row["size"] for row in matrix if row.get("framing") == "trailers"], [13])

    def test_topology_rejects_shared_core(self):
        allowed = sorted(os.sched_getaffinity(0))
        with self.assertRaises(ValueError):
            validate_cpus(allowed[0], [allowed[0]])

    def test_metrics_have_cpu_memory_and_descriptors(self):
        sample = snapshot(os.getpid())
        self.assertGreater(sample["rss_kib"], 0)
        self.assertGreater(sample["fds"], 0)
        self.assertTrue(sample["sampled_thread_context_switches"])

    def test_manifest_requires_matching_hash(self):
        manifest = load_manifest(None, self.identity)
        self.assertEqual(len(manifest["targets"]), 3)
        self.assertTrue(all(target["sha256"] == self.identity["sha256"] for target in manifest["targets"]))

    def test_ws_load_against_independent_python_reference(self):
        with BroadcastReference(timeout=7) as server:
            result = run_command(
                [self.tool, "ws-load", "--url", f"ws://{server.address[0]}:{server.address[1]}/chat",
                 "--size", "64", "--fanout", "4", "--duration", "20ms"],
                cwd=ROOT, timeout=10,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            report = json.loads(result.stdout)
            self.assertTrue(report["valid"], report["errors"])
            self.assertEqual(report["deliveries"], 4 * report["successes"])
            self.assertEqual(report["intentional_normal_closes"], 4)
            self.assertIn("actual_admission_boundary", report["measurement_scope"])
            self.assertGreater(report["measurement_end_unix_ns"], report["measurement_start_unix_ns"])
            self.assertGreaterEqual(report["process_cpu_user_seconds"], report["cpu_user_seconds"])
            self.assertGreaterEqual(report["process_cpu_system_seconds"], report["cpu_system_seconds"])

    def test_corrupt_ws_payload_is_not_successful_throughput(self):
        def serve(wire, _index):
            wire.send(upgrade_response(read_client_upgrade(wire)))
            ping = read_frame(wire, expect_mask=True)
            self.assertEqual(ping.opcode, 9)
            wire.send(encode_frame(10, ping.payload))
            frame = read_frame(wire, expect_mask=True)
            wrong = frame.payload[:-1] + bytes([frame.payload[-1] ^ 1])
            wire.send(encode_frame(2, wrong))
            wire.expect_eof()

        with ScriptedPeer(serve) as server:
            result = run_command(
                [self.tool, "ws-load", "--url", f"ws://{server.address[0]}:{server.address[1]}/chat",
                 "--size", "64", "--duration", "20ms"], cwd=ROOT, timeout=10,
            )
            self.assertNotEqual(result.returncode, 0)
            report = json.loads(result.stdout)
            self.assertFalse(report["valid"])
            self.assertEqual(report["successes"], 0)
            self.assertIsNone(report["successes_per_second"])
            self.assertTrue(report["errors"])

    def test_wrong_fixed_http_bytes_fail_the_compiled_cli(self):
        def serve(wire, _index):
            wire.until(b"\r\n\r\n", 65536)
            wire.send(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nbad")
            wire.expect_eof()

        with ScriptedPeer(serve) as server:
            result = run_command(
                [self.tool, "http-load", "--url", f"http://{server.address[0]}:{server.address[1]}/fixture",
                 "--size", "3", "--duration", "20ms"], cwd=ROOT, timeout=10,
            )
            self.assertNotEqual(result.returncode, 0)
            report = json.loads(result.stdout)
            self.assertFalse(report["valid"])
            self.assertEqual(report["attempts"], 1)
            self.assertEqual(report["successes"], 0)
            self.assertIsNone(report["successes_per_second"])
            self.assertTrue(report["errors"])

    def test_external_wait4_resources(self):
        from native_client import execute
        result = execute([sys.executable, "-B", "-c", "print('wait4-child-complete')"], timeout=3)
        self.assertEqual(result["exit"], 0)
        self.assertIn("wait4-child-complete", result["diagnostics"])
        self.assertGreater(result["rusage"]["max_rss_kib"], 0)
        self.assertGreaterEqual(result["rusage"]["cpu_user_seconds"], 0)

    def test_summary_uses_per_trial_quantile_spread(self):
        from summarize import spread
        self.assertEqual(spread([1.0, 2.0, 100.0]), {"median": 2.0, "min": 1.0, "max": 100.0})
        self.assertIsNone(spread([None, None]))

    def test_summary_context_deltas_do_not_lose_departed_threads(self):
        from summarize import context_deltas
        row = {
            "server_before": {"sampled_thread_context_switches": {"1": {"voluntary_ctxt_switches": 10}}},
            "server_samples": [{"sampled_thread_context_switches": {
                "1": {"voluntary_ctxt_switches": 14}, "2": {"voluntary_ctxt_switches": 3}}}],
            "server_after": {"sampled_thread_context_switches": {"1": {"voluntary_ctxt_switches": 16}}},
        }
        self.assertEqual(context_deltas(row)["voluntary_ctxt_switches"], 9)

    def test_reference_echo_sends_prefixes_before_upload_completion(self):
        from duplex_suite import server_case
        with PeerProcess([self.tool, "http-serve", "--mode", "fixture", "--bind", "127.0.0.1:0"],
                         cwd=ROOT) as server:
            result = server_case(server.address, chunked=True)
            self.assertTrue(result["passed"], result)
            self.assertTrue(result["response_head_before_request_body"])


if __name__ == "__main__":
    unittest.main()
