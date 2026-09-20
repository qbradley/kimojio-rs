import contextlib
import io
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

from compare import main, summarize, validate_result


class ComparisonTests(unittest.TestCase):
    def result(self):
        return {
            "schema": 1,
            "valid": True,
            "transport": "unix_socketpair",
            "measurement_scope": "after_warmup_through_successful_connection_shutdown",
            "validation": "complete_payload_equality_and_status_in_both_directions",
            "connections_created": 1,
            "reconnects": 0,
            "warmup_exchanges": 10,
            "measured_exchanges": 100,
            "server_exchanges": 110,
            "request_bytes": 0,
            "response_bytes": 128,
            "chunk_bytes": 16384,
            "chunked": False,
            "validated_payload_bytes": 12800,
            "elapsed_seconds": 0.01,
            "nanoseconds_per_exchange": 100000,
        }

    def case(self):
        return {"iterations": 100, "request_bytes": 0, "response_bytes": 128}

    def test_exact_counts_and_timing(self):
        validate_result(self.result(), self.case(), 10)

    def test_reconnect_and_payload_mismatch_are_invalid(self):
        for key, value in [
            ("reconnects", 1),
            ("server_exchanges", 109),
            ("validated_payload_bytes", 12799),
            ("valid", False),
            ("measurement_scope", "before_shutdown"),
            ("validation", "length_only"),
            ("transport", "simulated"),
        ]:
            result = self.result()
            result[key] = value
            with self.assertRaises(ValueError):
                validate_result(result, self.case(), 10)

    def test_wrong_denominator_and_nonfinite_timing_are_invalid(self):
        for value in [0, -1, float("nan"), float("inf"), 1]:
            result = self.result()
            result["nanoseconds_per_exchange"] = value
            with self.assertRaises(ValueError):
                validate_result(result, self.case(), 10)

    def test_summary_retains_trial_range(self):
        rows = [
            {"candidate": "a", "case": "small", "result": {"nanoseconds_per_exchange": value}}
            for value in [30, 10, 20]
        ]
        summary, = summarize(rows)
        self.assertEqual(summary["median_ns"], 20)
        self.assertEqual(summary["min_ns"], 10)
        self.assertEqual(summary["max_ns"], 30)
        self.assertEqual(summary["trials"], 3)

    def test_native_backend_must_be_explicit_in_result(self):
        result = self.result()
        with self.assertRaises(ValueError):
            validate_result(result, self.case(), 10, "native")
        result["backend"] = "native"
        validate_result(result, self.case(), 10, "native")
        with self.assertRaises(ValueError):
            validate_result(result, self.case(), 10)

    def test_duplex_and_copy_modes_cannot_silently_use_the_baseline(self):
        case = self.case() | {"duplex": True}
        result = self.result()
        with self.assertRaises(ValueError):
            validate_result(result, case, 10)
        result.update(duplex=True, forwarding="lease", response_chunked=True)
        validate_result(result, case, 10)
        case["copy_forward"] = True
        with self.assertRaises(ValueError):
            validate_result(result, case, 10)
        result["forwarding"] = "copy"
        validate_result(result, case, 10)

    def test_success_without_new_output_cannot_reuse_a_stale_result(self):
        stale = self.result() | {
            "warmup_exchanges": 1000,
            "measured_exchanges": 20000,
            "server_exchanges": 21000,
            "validated_payload_bytes": 20000 * 128,
            "nanoseconds_per_exchange": 500,
        }
        validate_result(stale, self.case() | {"iterations": 20000}, 1000)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "binary"
            binary.write_bytes(b"frozen fixture")
            manifest = root / "manifest.json"
            manifest.write_text(json.dumps({"candidates": [{
                "name": "fixture", "revision": "fixture", "binary": str(binary),
                "build_command": "fixture",
            }]}))
            output = root / "report.json"
            artifacts = root / "report"
            artifacts.mkdir()
            previous = artifacts / "fixture-small-0.json"
            previous.write_text(json.dumps(stale))
            arguments = [
                "compare.py", "--manifest", str(manifest), "--output", str(output),
                "--case", "small", "--trials", "1",
            ]
            with (
                patch("sys.argv", arguments),
                patch("compare.subprocess.run", return_value=subprocess.CompletedProcess([], 0)),
                contextlib.redirect_stderr(io.StringIO()),
            ):
                self.assertEqual(main(), 1)
            report = json.loads(output.read_text())
            self.assertFalse(report["valid"])
            self.assertFalse(report["rows"][0]["valid"])
            self.assertEqual(report["summaries"], [])
            self.assertFalse(previous.exists())


if __name__ == "__main__":
    unittest.main()
