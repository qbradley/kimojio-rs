import unittest
from subprocess import CompletedProcess
from unittest.mock import patch

from profile_fsm import result, successful


class ProfileOutcomeTests(unittest.TestCase):
    def test_native_and_go_success_schemas(self):
        self.assertTrue(successful({"valid": True, "errors": 0, "count": 4}))
        self.assertTrue(successful({"valid": True, "errors": None, "successes": 4}))
        self.assertTrue(successful({"valid": True, "errors": [], "successes": 4}))

    def test_invalid_empty_and_malformed_results(self):
        cases = [
            {},
            {"valid": False, "errors": 0, "count": 4},
            {"valid": True, "errors": 1, "count": 4},
            {"valid": True, "errors": ["wrong bytes"], "successes": 4},
            {"valid": True, "count": 4},
            {"valid": True, "errors": 0, "count": 0},
            {"valid": True, "errors": False, "count": 4},
            {"valid": True, "errors": None, "successes": True},
        ]
        for value in cases:
            with self.subTest(value=value):
                self.assertFalse(successful(value))

    @patch("profile_fsm.subprocess.run")
    def test_nonzero_exit_cannot_supply_success(self, run):
        run.return_value = CompletedProcess(
            [], 1, '{"valid":true,"errors":0,"count":4}', "failed"
        )
        with self.assertRaisesRegex(RuntimeError, "load failed"):
            result(["fixture"], 1)

    @patch("profile_fsm.subprocess.run")
    def test_errorful_report_cannot_supply_success(self, run):
        run.return_value = CompletedProcess(
            [], 0, '{"valid":false,"errors":1,"count":4}', ""
        )
        with self.assertRaisesRegex(RuntimeError, "did not complete"):
            result(["fixture"], 1)


if __name__ == "__main__":
    unittest.main()
