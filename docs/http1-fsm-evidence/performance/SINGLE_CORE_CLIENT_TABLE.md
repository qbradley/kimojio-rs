| Comparison | Case | Implementation | ops/s median [min–max] | p99 ms median [min–max] | client CPU cores | server CPU cores | server RSS MiB |
|---|---|---|---:|---:|---:|---:|---:|
| clients | fixture 128B c1 | go-client | 13711.0 [13690.1–14554.5] | 0.135 [0.113–0.135] | 0.64 | 0.55 | 24.15 |
| clients | fixture 128B c1 | native-client | 12666.8 [12513.8–13191.4] | 0.125 [0.119–0.125] | 0.57 | 0.51 | 26.09 |
| clients | fixture 128B c16 | go-client | 47403.2 [46552.8–52052.9] | 0.983 [0.852–1.016] | 0.97 | 0.86 | 24.17 |
| clients | fixture 128B c16 | native-client | 31758.0 [30059.8–31780.4] | 0.983 [0.721–0.999] | 0.99 | 0.74 | 26.11 |
| clients | fixture 128B c16 fresh | go-client | 14380.4 [13782.1–14478.1] | 2.097 [2.097–2.425] | 0.99 | 0.62 | 24.19 |
| clients | fixture 128B c16 fresh | native-client | 10462.4 [9869.4–10918.3] | 1.868 [1.769–1.933] | 1.00 | 0.61 | 26.18 |
| clients | fixture 65536B c1 | go-client | 7806.1 [7457.0–8044.1] | 0.250 [0.250–0.279] | 0.69 | 0.49 | 24.05 |
| clients | fixture 65536B c1 | native-client | 6309.0 [6159.2–6340.4] | 0.229 [0.225–0.238] | 0.72 | 0.40 | 25.96 |
| clients | fixture 65536B c16 | go-client | 12288.2 [12106.1–12496.5] | 2.884 [2.818–3.015] | 1.00 | 0.62 | 25.65 |
| clients | fixture 65536B c16 | native-client | 8255.9 [8024.5–8605.5] | 3.146 [3.015–3.408] | 1.00 | 0.44 | 25.95 |
