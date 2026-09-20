"""Deterministic controls for the completed, bounded paired experiment."""
import gzip
import hashlib
import json
from pathlib import Path
import unittest

from summarize import interval, read_text

HERE = Path(__file__).resolve().parent


def records(path):
    return [json.loads(line) for line in read_text(path).splitlines()]


class EvidenceTests(unittest.TestCase):
    def test_nine_groups_and_strict_outcomes(self):
        for name, expected in (("matrix", 4140), ("sensitivity", 108)):
            rows = records(HERE / (name + ".jsonl"))
            self.assertEqual(len(rows), expected)
            groups = {}
            for row in rows:
                r = row["report"]
                self.assertNotIn("failure", row)
                self.assertEqual(row["returncode"], 0)
                self.assertTrue(r["valid"])
                self.assertGreater(r["process_cpu_ns"], 0)
                count = r["concurrency"] * (r["cohorts"] + r["warmup_cohorts"])
                self.assertEqual(r["client_retirements"], count)
                self.assertEqual(r["server_retirements"], count)
                self.assertEqual(r["drivers_closed_successfully"], 2)
                self.assertEqual(r["connections_created"], 1)
                self.assertEqual(r["reconnects"], 0)
                self.assertEqual(r["checked_payload_bytes_including_warmup"],
                                 count * (r["request_bytes"] + r["response_bytes"]))
                self.assertIsNone(r["allocations"])
                if r["overlap_required"]:
                    self.assertEqual(r["early_response_witnesses_including_warmup"], count)
                key = (row["label"], r["backend"], r["concurrency"], r["phase"], row["variant"])
                groups.setdefault(key, set()).add((row["version"], row["trial"]))
            for values in groups.values():
                self.assertEqual(values, {(v, t) for v in ("baseline", "candidate") for t in range(9)})

    def test_all_post_pre_runtime_counters_match(self):
        comparisons = json.loads(read_text(HERE / "retention-comparison.json"))
        self.assertEqual(len(comparisons), 48)
        for row in comparisons:
            self.assertEqual(row["all_origin_counters_equal"], row["label"] != "pre_runtime")
        for version in ("baseline", "candidate"):
            for backend, plateau in (("native", 258314), ("generic", 338451)):
                rows = records(HERE / "retention" / version / (backend + "-empty-4096.jsonl"))
                for row in rows:
                    live = sum(c["live"] for c in row["counts"])
                    self.assertGreaterEqual(live, sum(s["live_bytes"] for s in row["sites"]))
                    if row["label"] == "connection_live" and row["cohort"] in (256, 1024, 4096):
                        self.assertEqual(live, plateau)
                self.assertEqual(sum(c["live"] for c in rows[-1]["counts"]), 1572)

    def test_actual_copy_sites_and_unresolved_general_gain(self):
        profiles = json.loads(read_text(HERE / "profile-analysis.json"))
        for profile, address, count in zip(profiles, (0x9EC73, 0x9EC1D), (350, 0)):
            site = next(s for s in profile["copy_and_allocation_sites"] if s["address"] == address)
            self.assertEqual(site["exact_return_samples"], count)
            self.assertEqual(sum(profile["endpoint_buckets"].values()), profile["samples"])
        groups = json.loads(read_text(HERE / "aggregate.json"))
        large = next(r for r in groups if r["category"] == "large" and r["backend"] == "generic")
        broad = next(r for r in groups if r["category"] == "all" and r["backend"] == "generic")
        self.assertLess(large["bootstrap95_high"], 1)
        self.assertLess(broad["bootstrap95_low"], 1)
        self.assertGreater(broad["bootstrap95_high"], 1)
        self.assertEqual(interval([1] * 9), (1, 1))

    def test_archives_and_binary_identity(self):
        for record in json.loads((HERE / "compressed-evidence.json").read_text()):
            data = (HERE / record["gzip_file"]).read_bytes()
            self.assertEqual(hashlib.sha256(data).hexdigest(), record["gzip_sha256"])
            self.assertEqual(hashlib.sha256(gzip.decompress(data)).hexdigest(), record["sha256"])
        freeze = json.loads((HERE / "freeze.json").read_text())
        for binaries in (freeze["baseline_binaries"], freeze["binaries"]):
            for binary in binaries.values():
                self.assertEqual(hashlib.sha256(Path(binary["path"]).read_bytes()).hexdigest(),
                                 binary["sha256"])


if __name__ == "__main__":
    unittest.main()
