import copy
import unittest

from cases import Case, validate_results
from peer import digest_for
from profiles import WRAPPER_TRAILERS, prepend_warmups
from protocol_suite import request, validate


def stream_result(stream, length=0, status=200):
    return {
        "stream_id": stream, "status": status, "content_length": length,
        "bytes": length, "sha256": digest_for(stream, length), "trailers": [],
        "informational": [], "ended": True, "outcome": "complete", "error": None,
    }


class ProfileTests(unittest.TestCase):
    def test_profile_is_explicit_and_uses_existing_request_schema(self):
        cold = request("early-response", ("127.0.0.1", 1))
        warm = request("early-response", ("127.0.0.1", 1), profile="wrapper")
        self.assertEqual((cold["request_count"], cold["concurrency"]), (2, 2))
        self.assertEqual((warm["request_count"], warm["concurrency"]), (4, 2))
        self.assertEqual(warm["requests"][2:], cold["requests"])
        self.assertEqual(warm["actions"], [])
        self.assertEqual(set(warm), set(cold))
        with self.assertRaises(AssertionError):
            prepend_warmups(cold, 1)
        with self.assertRaises(AssertionError):
            request("early-response", ("127.0.0.1", 1), profile="wrapper", early_policy="application-cancel")

    def test_wrapper_trailers_compare_occurrences_without_value_normalization(self):
        case = Case("trailers", 7, route="trailers")
        stream = stream_result(1, 7)
        stream["trailers"] = [["X-Other", "marker"], ["X-List", "one"], ["x-list", "two"]]
        report = {
            "schema": 1, "streams": [stream],
            "connection": {"closed": True, "error": None, "outcome": "graceful"},
        }
        validate_results(case, report, profile="wrapper", expected_trailers=WRAPPER_TRAILERS)
        with self.assertRaisesRegex(AssertionError, "trailers"):
            validate_results(case, report, expected_trailers=[list(pair) for pair in WRAPPER_TRAILERS])
        for bad in (
            [["x-other", "marker"], ["x-list", "two"], ["x-list", "one"]],
            [["x-other", "marker"], ["x-list", "one"]],
            [["x-other", "marker"], ["x-list", "one, two"]],
            [["x-other", "marker"], ["x-list", "one"], ["x-list", "two"], ["x-list", "two"]],
        ):
            broken = copy.deepcopy(report)
            broken["streams"][0]["trailers"] = bad
            with self.assertRaisesRegex(AssertionError, "trailers"):
                validate_results(case, broken, profile="wrapper", expected_trailers=WRAPPER_TRAILERS)

    def test_wrapper_early_requires_barriers_exact_credit_and_retirement(self):
        response = stream_result(5, status=413)
        response.update(outcome="reset", error={"scope": "stream", "code": 0})
        report = {
            "schema": 1, "streams": [stream_result(1), stream_result(3), response, stream_result(7, 37)],
            "connection": {"closed": True, "error": None, "outcome": "graceful"},
        }
        witness = {
            "requests": 4, "peer_eof": True, "warmup_barrier": True, "early_response_barrier": True,
            "received": {"5": 1024}, "received_flow": {"5": 1024},
            "request_ended": {"1": True, "3": True, "5": False, "7": True},
            "window_updates": {}, "server_resets": {"5": 0}, "resets": {},
            "upload_at_response": 1024,
            "wrapper_upload_credit": {
                "stream_initial": 1024, "connection_initial": 65535,
                "stream_refunds": 0, "connection_refunds": 0, "flow": 1024,
                "stream_final": 0, "connection_final": 64511, "sha256": digest_for(5, 1024),
            },
        }
        validate("early-response", report, witness, profile="wrapper")
        for field in ("warmup_barrier", "early_response_barrier"):
            broken = copy.deepcopy(witness)
            broken[field] = False
            with self.assertRaises(AssertionError):
                validate("early-response", report, broken, profile="wrapper")
        broken = copy.deepcopy(witness)
        broken["received_flow"]["5"] = 1025
        with self.assertRaises(AssertionError):
            validate("early-response", report, broken, profile="wrapper")
        broken = copy.deepcopy(witness)
        broken["wrapper_upload_credit"]["sha256"] = digest_for(1, 1024)
        with self.assertRaises(AssertionError):
            validate("early-response", report, broken, profile="wrapper")
        broken = copy.deepcopy(witness)
        broken["upload_at_response"] = 512
        with self.assertRaises(AssertionError):
            validate("early-response", report, broken, profile="wrapper")
        for index, field, value in (
            (0, "outcome", None), (2, "outcome", None), (2, "outcome", "connection_failed"),
            (2, "ended", False), (2, "error", {"scope": "stream", "code": 8}),
            (3, "outcome", "connection_failed"),
        ):
            broken = copy.deepcopy(report)
            broken["streams"][index][field] = value
            with self.assertRaises(AssertionError):
                validate("early-response", broken, witness, profile="wrapper")


if __name__ == "__main__":
    unittest.main()
