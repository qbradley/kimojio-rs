"""Native result normalization tests; these do not execute a Rust binary."""

import base64
import json
import unittest

from harness import PeerProcess, ProtocolError
from kimojio_adapter import normalize
from server_suite import WORKSPACE


def row(body=b"body"):
    return {
        "status": 200,
        "body_base64": base64.b64encode(body).decode(),
        "headers": [["x-test", base64.b64encode(b"value").decode()]],
        "trailers": [["X-End", "ZG9uZQ=="]],
        "error": None,
    }


def ndjson(*records):
    return b"".join(json.dumps(record).encode() + b"\n" for record in records)


class NativeAdapterTests(unittest.TestCase):
    def test_binary_body_and_duplicate_fields_are_lossless(self):
        payload = bytes(range(256)) + b"\nSTATUS 200\nTRAILER x:fake\n"
        record = row(payload)
        record["trailers"].append(["x-end", base64.b64encode(b"second\xff").decode()])
        report = normalize(ndjson(record, row(b"")), 0)
        self.assertEqual(len(report["exchanges"]), 2)
        self.assertEqual(base64.b64decode(report["exchanges"][0]["body_base64"]), payload)
        self.assertEqual(report["exchanges"][0]["trailers"], {"x-end": ["done", "second\xff"]})
        self.assertEqual(report["exchanges"][1]["index"], 1)

    def test_reported_error_remains_distinct_from_timeout(self):
        self.assertEqual(
            normalize(ndjson({"error": "Protocol"}), 1)["failure"]["kind"], "rejected"
        )
        self.assertEqual(
            normalize(ndjson({"error": "ReadTimeout"}), 1)["failure"]["kind"], "timeout"
        )
        report = normalize(ndjson(row(), {"error": "Protocol"}), 1)
        self.assertEqual(len(report["exchanges"]), 1)

    def test_crash_or_exit_mismatch_cannot_pass_as_rejection(self):
        for raw, code in (
            (b"", 1),
            (ndjson(row()), 1),
            (ndjson({"error": "Protocol"}), 0),
            (ndjson({"error": "Protocol"}), -11),
            (ndjson({"error": "Protocol"}), 101),
        ):
            with self.subTest(code=code, raw=raw), self.assertRaises(ProtocolError):
                normalize(raw, code)

    def test_invalid_native_records_fail(self):
        bad_body = row()
        bad_body["body_base64"] = "not!base64"
        bad_fields = row()
        bad_fields["trailers"] = {"x": "value"}
        for raw in (
            ndjson([]),
            ndjson(bad_body),
            ndjson(bad_fields),
            ndjson({"error": 7}),
            ndjson({"error": "Protocol"}, row()),
        ):
            with self.subTest(raw=raw), self.assertRaises((AssertionError, ValueError)):
                normalize(raw, 1)

    def test_diagnostics_do_not_insert_labels_inside_lines(self):
        child = PeerProcess(["unused"], cwd=WORKSPACE)
        child.output.extend([("stderr", b"stack "), ("stdout", b"ready\n"), ("stderr", b"trace\n")])
        self.assertEqual(child.diagnostics(), "stdout: ready\n\nstderr: stack trace\n")


if __name__ == "__main__":
    unittest.main()
