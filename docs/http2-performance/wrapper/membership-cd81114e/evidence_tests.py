"""Deterministic outcome, accounting, isolation, and archive controls."""
import gzip
import hashlib
import json
from pathlib import Path
import unittest

HERE = Path(__file__).resolve().parent


def read(path):
    if path.exists():
        return path.read_text()
    with gzip.open(str(path) + ".gz", "rt") as stream:
        return stream.read()


def rows(path):
    return [json.loads(line) for line in read(path).splitlines()]


class EvidenceTests(unittest.TestCase):
    def test_all_timed_workloads_and_groups(self):
        freeze = json.loads((HERE / "freeze.json").read_text())
        for name, expected in (("matrix.jsonl", 4140), ("sensitivity.jsonl", 108),
                               ("full-refresh/matrix.jsonl", 2000)):
            records = rows(HERE / name)
            self.assertEqual(len(records), expected)
            groups = {}
            for record in records:
                report = record["report"]
                version = record.get("version", "candidate")
                binary = freeze["baseline_binaries" if version == "baseline" else "binaries"]["runtime"]["path"]
                self.assertEqual(record["command"][3], binary)
                self.assertNotIn("failure", record)
                self.assertEqual(record["returncode"], 0)
                self.assertTrue(report["valid"])
                self.assertGreater(report["process_cpu_ns"], 0)
                count = report["concurrency"] * (report["cohorts"] + report["warmup_cohorts"])
                self.assertEqual(report["client_retirements"], count)
                self.assertEqual(report["server_retirements"], count)
                self.assertEqual(report["drivers_closed_successfully"], 2)
                self.assertEqual(report["connections_created"], 1)
                self.assertEqual(report["reconnects"], 0)
                self.assertIsNone(report["allocations"])
                self.assertEqual(report["checked_payload_bytes_including_warmup"],
                                 count * (report["request_bytes"] + report["response_bytes"]))
                if report["overlap_required"]:
                    self.assertEqual(report["early_response_witnesses_including_warmup"], count)
                if not name.startswith("full-refresh"):
                    key = (record["label"], report["concurrency"], report["phase"],
                           report["backend"], record["variant"])
                    groups.setdefault(key, set()).add((record["version"], record["trial"]))
            for observed in groups.values():
                self.assertEqual(observed, {(v, t) for v in ("baseline", "candidate") for t in range(9)})

    def test_cost_gate_uses_both_affected_backends(self):
        groups = json.loads(read(HERE / "aggregate.json"))
        self.assertEqual({r["backend"] for r in groups}, {"native", "generic", "both_affected_backends"})
        for row in groups:
            if row["category"] == "all":
                self.assertLess(row["bootstrap95_high"], 1)
        for name in ("matrix-summary.json", "sensitivity-summary.json"):
            for row in json.loads(read(HERE / name)):
                self.assertLessEqual(row["wall_bootstrap95_low"], 1)

    def test_actual_memberships_and_layout(self):
        folder = HERE / "membership-distribution"
        expected = {("native", "empty"): {"0": 768, "1": 4973},
                    ("generic", "empty"): {"0": 768, "1": 5143},
                    ("native", "duplex"): {"0": 1057, "1": 10273},
                    ("generic", "duplex"): {"0": 1057, "1": 11448}}
        for version in ("baseline", "candidate"):
            for (backend, case), counts in expected.items():
                record = json.loads(read(folder / f"{version}-{backend}-{case}.json"))
                self.assertIsNone(record["failure"])
                self.assertEqual(record["counts_by_membership_length"], counts)
                self.assertEqual(record["wait_data_bytes"], 80)
                self.assertEqual(record["rc_wait_allocation_bytes"], 96)
                self.assertEqual(record["membership_storage_bytes"], 24)

    def test_removed_sites_and_bounded_retention(self):
        records = json.loads(read(HERE / "allocation-site-summary.json"))
        for backend, removed in (("native", 341115), ("generic", 329414)):
            def cohort(version):
                return next(r for r in records if r["version"] == version
                            and r["case"] == backend + "-static-32"
                            and r["label"] == "connection_live" and r["cohort"] == 32)
            baseline, candidate = cohort("baseline"), cohort("candidate")
            self.assertEqual(baseline["groups"]["reverse_membership_vector"]["allocations"], removed)
            self.assertNotIn("reverse_membership_vector", candidate["groups"])
            self.assertEqual(baseline["allocations"] - candidate["allocations"], removed)
            self.assertEqual(baseline["groups"]["wait_data"], candidate["groups"]["wait_data"])
        for record in records:
            self.assertGreaterEqual(record["unattributed_live"], 0)
            if record["label"] == "post_runtime_cleanup":
                self.assertEqual(record["live"], 548 if record["case"].startswith(("wait-", "nop-")) else 1572)
            if (record["version"] == "candidate" and record["case"].endswith("empty-4096")
                    and record["label"] == "connection_live" and record["cohort"] in (256, 1024, 4096)):
                self.assertEqual(record["live"], 258090 if record["case"].startswith("native") else 338227)

    def test_frozen_binaries_and_archives(self):
        freeze = json.loads((HERE / "freeze.json").read_text())
        self.assertEqual(freeze["async_stream_blob"], "97c2eb6367dc06d02e65293e9f314b92a09ec3ec")
        for binaries in (freeze["baseline_binaries"], freeze["binaries"]):
            for binary in binaries.values():
                self.assertEqual(hashlib.sha256(Path(binary["path"]).read_bytes()).hexdigest(), binary["sha256"])
        for record in json.loads((HERE / "compressed-evidence.json").read_text()):
            data = (HERE / record["gzip_file"]).read_bytes()
            self.assertEqual(hashlib.sha256(data).hexdigest(), record["gzip_sha256"])
            self.assertEqual(hashlib.sha256(gzip.decompress(data)).hexdigest(), record["sha256"])


if __name__ == "__main__":
    unittest.main()
