# Paired PoC timing results

Each case has nine alternating baseline/candidate pairs.
Each cell shows median microseconds per exchange and the minimum–maximum range.
The ratio interval is a percentile bootstrap of paired-ratio medians.
It uses 10000 resamples and seed 20260920.
The interval is descriptive and assumes that the pairs represent the measurement conditions.

| Case | C | Fragment | Baseline µs | PoC µs | Median paired change | 95% ratio interval |
|---|---:|---:|---:|---:|---:|---:|
| empty | 1 | 65536 | 2.894 (2.844–3.581) | 2.874 (2.855–3.065) | -0.70% | 0.9663–1.0359 |
| empty | 128 | 65536 | 2.670 (2.662–2.781) | 2.666 (2.657–2.728) | -0.21% | 0.9811–1.0023 |
| empty | 1 | 17 | 3.702 (3.648–3.854) | 3.658 (3.638–3.727) | -0.84% | 0.9731–1.0062 |
| duplex | 8 | 65536 | 3.576 (3.568–3.633) | 3.554 (3.531–3.840) | -0.61% | 0.9884–1.0034 |
| 32k | 8 | 65536 | 6.760 (6.664–7.061) | 6.662 (6.613–6.984) | -1.45% | 0.9684–1.0077 |
| 1m | 8 | 65536 | 153.891 (153.598–220.862) | 153.018 (152.357–203.737) | -0.86% | 0.9831–1.0028 |

All six intervals include 1.0.
The measurements do not support acceptance of the PoC as a performance improvement.
All 108 timed runs retained the strict workload assertions and passed.
