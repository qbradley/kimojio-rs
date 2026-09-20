"""Deterministic controls for report accounting and profile attribution."""
import json
from pathlib import Path
import unittest

from summarize import interval
from evidence_io import read_text

HERE = Path(__file__).resolve().parent


def records(name):
    return [json.loads(line) for line in read_text(HERE / name).splitlines()]


class EvidenceTests(unittest.TestCase):
    def test_primary_outcomes_and_cpu_clock(self):
        rows = records("matrix.jsonl")
        self.assertEqual(len(rows), 2030)
        self.assertEqual(len({(r["label"], r["report"]["backend"], r["report"]["phase"],
                              r["report"]["concurrency"], r["trial"]) for r in rows}), 400)
        for row in rows:
            r = row["report"]
            self.assertEqual(row["returncode"], 0)
            self.assertTrue(r["valid"])
            self.assertGreater(r["process_cpu_ns"], 0)
            exchanges = r["concurrency"] * (r["cohorts"] + r["warmup_cohorts"])
            self.assertEqual(r["client_retirements"], exchanges)
            self.assertEqual(r["server_retirements"], exchanges)
            self.assertEqual(r["drivers_closed_successfully"], 2)
            self.assertEqual(r["reconnects"], 0)
            self.assertEqual(r["checked_payload_bytes_including_warmup"],
                             exchanges * (r["request_bytes"] + r["response_bytes"]))
            if r["overlap_required"]:
                self.assertEqual(r["early_response_witnesses_including_warmup"], exchanges)

    def test_continuous_allocation_accounting(self):
        rows = records("allocations.jsonl")
        self.assertEqual(len(rows), 80)
        for row in rows:
            a = row["report"]["allocations"]
            self.assertEqual(sum(b["requested_live_start"] for b in a["origin_buckets"]),
                             a["requested_live_start"])
            self.assertEqual(sum(b["requested_live_end"] for b in a["origin_buckets"]),
                             a["requested_live_end"])
            self.assertGreaterEqual(a["requested_peak_live"],
                                    max(a["requested_live_start"], a["requested_live_end"]))

    def test_call_return_attribution_rejects_adjacent_cold_calls(self):
        profiles = json.loads(read_text(HERE / "profile-analysis.json"))
        self.assertEqual(len(profiles), 8)
        for profile in profiles:
            self.assertEqual(sum(profile["endpoint_buckets"].values()), profile["samples"])
            for site in profile["sampled_call_sites"]:
                self.assertLessEqual(site["exact_return_samples"], site["function_stack_samples"])
                self.assertFalse("support::compare" in site["symbol"] and "alloc" in site["target"])
                self.assertFalse("run_with_configuration" in site["symbol"] and site["offset"] == 0x91)
        native = next(p for p in profiles if p["profile"] == "native-duplex1m-fp")
        bcmp = next(s for s in native["sampled_call_sites"]
                    if "support::compare" in s["symbol"] and "bcmp" in s["target"])
        self.assertEqual(bcmp["exact_return_samples"], 235)

    def test_exact_bootstrap_controls(self):
        self.assertEqual(interval([3, 3, 3, 3, 3]), (3, 3))
        self.assertEqual(interval([1, 2, 3, 4, 5]), (1, 5))


if __name__ == "__main__":
    unittest.main()
