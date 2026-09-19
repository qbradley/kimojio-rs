| Comparison | Case | Implementation | ops/s median [min–max] | p99 ms median [min–max] | client CPU cores | server CPU cores | server RSS MiB |
|---|---|---|---:|---:|---:|---:|---:|
| servers | fixture 65536B c1 fresh chunked POST echo | go-fixture | 3004.9 [2800.4–3032.2] | 0.606 [0.573–0.623] | 1.18 | 0.57 | 24.13 |
| servers | fixture 65536B c1 fresh chunked POST echo | rust-wrapper | 2906.3 [2849.6–2973.7] | 0.606 [0.590–0.639] | 1.14 | 0.53 | 2.99 |
| servers | fixture 65536B c16 fresh chunked POST echo | go-fixture | 7613.8 [7288.5–8158.7] | 5.767 [5.767–6.423] | 2.20 | 0.87 | 24.26 |
| servers | fixture 65536B c16 fresh chunked POST echo | rust-wrapper | 5764.1 [5423.4–5856.2] | 6.291 [4.719–6.685] | 1.90 | 0.96 | 5.05 |
| servers | fixture 1048576B c1 fresh chunked POST echo | go-fixture | 590.6 [522.0–629.9] | 4.981 [4.063–7.733] | 1.34 | 0.54 | 26.19 |
| servers | fixture 1048576B c1 fresh chunked POST echo | rust-wrapper | 424.3 [368.9–465.0] | 4.850 [4.719–7.602] | 1.25 | 0.59 | 2.93 |
| servers | fixture 1048576B c16 fresh chunked POST echo | go-fixture | 1423.8 [1332.4–1438.5] | 31.457 [31.457–31.982] | 2.20 | 0.83 | 24.25 |
| servers | fixture 1048576B c16 fresh chunked POST echo | rust-wrapper | 628.3 [589.2–659.3] | 38.797 [28.836–39.846] | 1.26 | 0.99 | 4.96 |
