import unittest

from compare import summarize, validate_result


class ComparisonTests(unittest.TestCase):
    def result(self):
        return {
            "valid": True,
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


if __name__ == "__main__":
    unittest.main()
