import unittest
import run

class ValidationTests(unittest.TestCase):
    def good(self):
        return dict(valid=True, errors=[], successes=100, attempts=100,
                    new_connections_total=16, measured_new_connections=0,
                    measured_seconds=2., requests_per_second=50.)

    def test_reject_errors_reconnects_and_bad_rates(self):
        run.validate(self.good(), 16)
        for key, value in [("valid", False), ("errors", ["bad payload"]),
                           ("attempts", 101), ("measured_new_connections", 1),
                           ("new_connections_total", 15), ("requests_per_second", 51.),
                           ("requests_per_second", float("nan")), ("measured_seconds", 0.)]:
            report = self.good(); report[key] = value
            with self.assertRaises(ValueError): run.validate(report, 16)

    def test_cpu_interpolation_requires_bracketing_samples(self):
        points = [dict(unix_ns=10, user=1.), dict(unix_ns=20, user=3.)]
        self.assertEqual(run.interpolate(points, 15, "user"), 2.)
        with self.assertRaises(ValueError): run.interpolate(points, 21, "user")

if __name__ == '__main__': unittest.main()
