# HTTP/1 and WebSocket comparative performance

## Result and scope

The coordinated primary matrix completed **126/126 valid rows with zero request errors**.
A one-client-core control completed **30/30 valid rows with zero request errors**.
A later large-chunked supplement completed **24/24 valid rows with zero request errors**.
The combined evidence contains **180 valid measured rows**.
All warmups passed.
No measured row was excluded or replaced.
Six final resource-challenge groups also passed.

These are measurements of complete loopback implementations on a shared host.
They are not isolated FSM, parser, disk, or maximum broadcast-capacity benchmarks.
No production optimization or instrumented allocation executable entered these comparisons.

### Main findings

- **Small wrapper responses lose throughput at concurrency16.** Native median throughput was 28,851 requests/s versus Go's 66,780 for 128-byte responses. Both servers approached one CPU core.
- **Larger wrapper responses were closer.** At concurrency16 and 64 KiB, native reached 11,566 requests/s versus Go's 12,585. Native median per-trial p99 was 3.60 ms versus 4.85 ms.
- **Static results reverse by payload and connection mode.** Native persistent 128-byte requests and Go persistent 64 KiB requests showed roughly 40–50 ms latency plateaus. Their low CPU use makes a general implementation ranking inappropriate.
- **Small WebSocket results were close or workload-dependent.** For 4 KiB and four recipients, native delivered 9,396 publications/s versus Go's 8,463. The 128-byte/four-recipient case favored Go, with overlapping trial ranges.
- **Both 64 KiB WebSocket implementations were latency-limited.** They delivered approximately 22–23 complete publications/s, with approximately 49–52 ms p99 and low CPU use.
- **Native server RSS was lower in these runs.** Configuration medians of sampled peak RSS ranged from 2.49–4.88 MiB for native servers and 20.97–26.41 MiB for Go.
- **Client parallelism matters.** With three CPUs available, Go's concurrency16 client used about 1.9–2.3 core-equivalents while the native client used about one. The one-core control reduced, but did not remove, Go's throughput lead at concurrency16.
- **Client results include different payload-validation costs.** They measure complete validated workloads, not isolated HTTP library or runtime efficiency. The separate profile attributes 50.7% of native 64 KiB client userspace samples to scalar byte comparison.
- **Large chunked echo is now measured separately.** At 1 MiB and concurrency16, native reached 628 MiB/s per direction versus Go's 1,424 MiB/s.

The latency plateaus are consistent with a transport or packetization limit under the selected socket policies.
This harness did not establish the cause.
They are not evidence that payload copies consumed the missing time.
The parent owns profile, disassembly, and allocation-counter interpretation.

## Selected server results

Values are medians across three trials.
Rates are requests/s for HTTP and complete publications/s for WebSocket.
WebSocket fanout includes the sender.
Each publication completes only after all recipients receive the exact message.

| Case | Go rate | Native rate | Go p99 ms | Native p99 ms |
|---|---:|---:|---:|---:|
| Static 128 B, persistent c1 | 8,814 | 22.6 | 0.180 | 48.234 |
| Static 128 B, persistent c16 | 35,693 | 361.5 | 1.212 | 49.283 |
| Static 128 B, fresh c16 | 20,718 | 21,191 | 1.901 | 2.032 |
| Static 64 KiB, persistent c1 | 23.3 | 6,003 | 52.429 | 0.410 |
| Static 64 KiB, persistent c16 | 370.3 | 12,782 | 50.332 | 4.588 |
| Wrapper 128 B, persistent c1 | 9,949 | 9,731 | 0.172 | 0.164 |
| Wrapper 128 B, persistent c16 | 66,780 | 28,851 | 0.868 | 1.114 |
| Wrapper 64 KiB, persistent c1 | 6,041 | 5,802 | 0.385 | 0.401 |
| Wrapper 64 KiB, persistent c16 | 12,585 | 11,566 | 4.850 | 3.604 |
| Wrapper 13 B, real chunked/trailers, c1 | 9,027 | 8,422 | 0.188 | 0.188 |
| WebSocket 128 B, fanout1 | 16,897 | 16,641 | 0.092 | 0.092 |
| WebSocket 128 B, fanout4 | 12,073 | 11,257 | 0.127 | 0.139 |
| WebSocket 4 KiB, fanout1 | 13,744 | 13,909 | 0.156 | 0.125 |
| WebSocket 4 KiB, fanout4 | 8,463 | 9,396 | 0.287 | 0.246 |
| WebSocket 64 KiB, fanout1 | 22.5 | 22.9 | 49.283 | 49.283 |
| WebSocket 64 KiB, fanout4 | 22.9 | 22.7 | 49.283 | 52.429 |

[The full primary table](performance/PRIMARY_TABLE.md) includes trial ranges and CPU/RSS medians.
[The primary summary](performance/primary-summary.json) retains each trial's p50/p95/p99, counts, elapsed time, CPU, RSS, switches, and FDs.
The large static reversals and WebSocket plateaus remain in every table and archive.
They were not discarded as outliers.

## Large chunked streaming supplement

The supplement uses the existing native `/echo` route without production changes.
The benchmark-only Go reference uses a matching full-duplex echo handler.
Each request uploads 64 KiB or 1 MiB of ASCII `x` through chunked transfer coding.
Each response must contain the exact payload through chunked transfer coding, without `Content-Length`.
A fixed-length response cannot pass this case.

The native route closes each exchange.
The same Go load generator therefore uses a fresh connection for both server targets.
The native GET-only benchmark client does not participate in this POST supplement.
These results include upload, echo, connection establishment, and explicit cleanup.
They are not measurements of persistent chunked downloads alone.
The load generator collects at most the expected response size plus one byte for exact validation.
Client CPU includes that work.

Independent, untimed socket probes also passed for both targets and both sizes.
They require the final response head before any upload.
Then each exact response prefix must arrive before the next upload chunk.
The probes use a varying-byte correctness corpus, separate from the timed ASCII `x` payload.
Thus the supplement does not label whole-body buffering or fixed responses as streaming.

| Payload / concurrency | Go echoes/s | Native echoes/s | Go MiB/s per direction | Native MiB/s per direction | Go p99 ms | Native p99 ms |
|---|---:|---:|---:|---:|---:|---:|
| 64 KiB / c1 | 3,005 | 2,906 | 187.8 | 181.6 | 0.606 | 0.606 |
| 64 KiB / c16 | 7,614 | 5,764 | 475.9 | 360.3 | 5.767 | 6.291 |
| 1 MiB / c1 | 590.6 | 424.3 | 590.6 | 424.3 | 4.981 | 4.850 |
| 1 MiB / c16 | 1,423.8 | 628.3 | 1,423.8 | 628.3 | 31.457 | 38.797 |

Values are medians of three trials.
Per-direction throughput counts payload bytes only.
Each successful echo transfers that amount in both directions.
Headers and chunk delimiters are not included in the byte rate.
At 1 MiB/c16, native server CPU approached one core.
The Go reference used about 0.83 core-equivalents, and its load client used about 2.20.
These observations do not isolate copy cost or establish a maximum streaming capacity.

The supplement uses seed 955 and the same separate 500 ms warmup and 2000 ms measurement process policy.
Its initial host load averages were 7.16/6.61/6.81.
There is no exclusive-host claim.
All 24 measured rows and their warmups passed, with no stderr diagnostics or sampler errors.
Every measured exchange used a new connection.
No earlier primary or control result was replaced.

[The supplement table](performance/LARGE_CHUNKED_TABLE.md) retains trial ranges and CPU/RSS summaries.
[Its structured summary](performance/large-chunked-summary.json) retains all per-trial metrics.
[The independent streaming proofs](performance/large-chunked-progress.json) retain headers, corpus definitions, hashes, and prefix barriers.

## Native versus Go clients

Both clients used the same uncapped Go fixture and exact ASCII `x` payloads.
The native client received an explicit 1-billion-request cap.
No native invocation reached that cap.
The Go client had no proactive request cap.
Neither client replayed a failed request as successful work.

**These rates and CPU costs describe complete validated workloads, not isolated HTTP library or runtime efficiency.**
The validation strategies differ despite identical expected bytes:

| Client | Response handling | Exact payload comparison |
|---|---|---|
| Native | Consumes response frames incrementally without whole-body collection | `bench_client::validate_chunk` compares each byte with ASCII `x`. The parent identifies a scalar loop in the frozen executable. |
| Go | Collects the response with `io.ReadAll(io.LimitReader(..., size+1))` | `bytes.Equal` compares the collected body with a prebuilt expected buffer. Go's amd64 equality implementation has SIMD paths for large buffers. |

The Go path includes response-buffer allocation and collection costs.
The native path avoids whole-body collection but pays its scalar comparison cost.
The one-core control does not remove this difference.
Neither the rate difference nor the CPU difference belongs entirely to the client library.

The parent reports 483 of 953 userspace samples in `bench_client::validate_chunk` for the native 64 KiB client profile.
That is approximately 50.7%, versus 19 samples at staging `memcpy`.
The dominant sampled work is payload comparison, not transport or copy work.
These sample shares describe that profile, not every matrix row.
They cannot supply a corrected transport-only throughput or a predicted optimization gain.

The harness source establishes the Go collection and comparison calls in [`load.go`](../../interop/perf/load.go).
The installed Go 1.27.1 source defines `bytes.Equal` through string equality in `src/bytes/bytes.go`.
Its `src/internal/bytealg/equal_amd64.s` includes SSE and AVX2 large-buffer comparison paths.
This source evidence describes the implementation strategy, not a new Go profile or an exact SIMD instruction count.
The parent owns the separate profile artifacts and their binary mappings.
All frozen clients and exact byte comparisons remain unchanged throughout the matrix.

| Case | Go rate, three CPUs | Native rate, three CPUs | Go rate, one CPU | Native rate, one CPU |
|---|---:|---:|---:|---:|
| 128 B, persistent c1 | 10,765 | 12,966 | 13,711 | 12,667 |
| 128 B, persistent c16 | 63,507 | 31,494 | 47,403 | 31,758 |
| 64 KiB, persistent c1 | 5,882 | 6,116 | 7,806 | 6,309 |
| 64 KiB, persistent c16 | 12,942 | 8,153 | 12,288 | 8,256 |
| 128 B, fresh c16 | 24,224 | 10,872 | 14,380 | 10,462 |

The primary allocation permitted CPUs 2, 4, and 6 for both clients.
Go used `GOMAXPROCS=3`.
The additional control pinned both clients to CPU 2 and used `GOMAXPROCS=1` for Go.
At concurrency16, both clients approached one CPU core in that control.
Go retained approximately 1.49× persistent throughput and 1.37× fresh-connection throughput.

The concurrency1 ranking changed with the Go CPU configuration.
Thus the primary client rates do not establish a universal library ranking.
The control ran later on the same shared host with a different initial load average.
It is a bounded diagnostic, not a causal decomposition of all configuration and host effects.

[The control table](performance/SINGLE_CORE_CLIENT_TABLE.md) includes per-trial ranges.
[Its structured summary](performance/single-core-client-summary.json) preserves all 30 rows.
Native quantiles were **not pooled**.
Summary quantiles are medians and ranges of the three per-trial quantiles.

### Separate parent profile and allocation observations

The parent also reports 40 of 184 WebSocket samples in masking for the 4096-byte/fanout4 workload.
The 80-byte move in the deadline iterator has six samples.
These observations do not support attributing the workload's cost to that move alone.

Separate server-process allocation probes report 139 calls per 128-byte wrapper request and 256 calls per 64 KiB request.
Those instrumented observations are not allocation counts for the client or timing measurements from this matrix.
The parent retains the probe methodology and profile evidence separately.
No profile observation changes the archived rates, CPU counters, payload checks, or executable hashes.

## Method and error accounting

The primary matrix contains 21 paired configurations and three trials.
It covers sixteen server configurations and five client configurations.
The control adds five paired client configurations.
The large-chunked supplement adds four paired configurations, for 30 paired configurations overall.
Seeds 953, 954, and 955 determine case and target order.

Each row starts a new server.
A separate client invocation warms it for 500 ms.
A fresh client process then admits work for 2000 ms.
The native client's internal warmup is zero in both invocations.
HTTP measurement includes initial connection establishment.
WebSocket measurement starts after registration pings complete.

Admission stops at the actual interval end.
Actual elapsed time includes admitted-request completion and explicit connection cleanup.
Client measurement CPU uses that same actual boundary.
The server observer has its own recorded invocation-window denominator.
Whole-process resource counters remain separately labeled.

Every response requires the expected status, framing, length, and exact payload.
The chunked case is the actual 13-byte `first\nsecond\n` fixture with `X-Finished: yes`.
Every WebSocket recipient must observe the exact binary payload and publication sequence.
Ordinary broadcast timing has one outstanding publication and never overflows a recipient queue.
Both negative payload controls require nonzero exit, invalid status, and null throughput.

All 180 measured rows passed.
All corresponding warmups passed.
There were no request errors, sampler errors, stderr diagnostics, native histogram overflows, or native cap retirements.
No failed measured trial was silently replaced.

The histogram uses 2048 bins and 32 subdivisions per power of two.
Each percentile reports the mathematical bucket upper bound.
Relative bucket width is at most approximately 3.125%.
The long-latency cases have fewer observations, so their tail estimates need additional caution.
Three two-second trials do not provide statistical confidence intervals.

## Hardware, affinity, and host noise

The host reports an Intel Xeon Platinum 8370C at 2.80 GHz.
Linux reports 32 logical CPUs across 16 guest-visible physical cores.
SMT sibling pairs are adjacent: 0/1, 2/3, 4/5, and subsequent pairs.
The kernel is `6.6.137.mshv2-2.azl3`.

- Server: CPU 0. Go references use `GOMAXPROCS=1`.
- Primary client: CPUs 2/4/6. Go uses `GOMAXPROCS=3`.
- One-core control client: CPU 2. Go uses `GOMAXPROCS=1`.
- Orchestration and sampling: CPU 8.

These placements avoid server/client SMT sharing.
They do **not** reserve CPUs or establish an exclusive host.
The parent paused other builds and recordings during these runs.
An independent reviewer could read profiling data on CPU 8.
The initial primary load averages were 8.55/8.17/7.47.
The control started at 6.73/7.73/7.73.
Some trial ranges are material, including Go's 128-byte/concurrency16 server case.
No governor, sysctl, or global CPU configuration changed.

The load generator's aggregate three-core warning did not fire in the primary matrix.
That does not rule out a single-thread or pipeline bottleneck.
The native-client one-core-equivalent result motivated the additional control.
CPU counters for servers have kernel-tick granularity.
A zero sampled CPU delta means below that resolution, not zero work.

## Comparison limits

Both HTTP server references and native servers use a 1000-request connection cap.
The client-comparison fixture is explicitly uncapped.
Server socket policies match the inspected native examples:

| Server family | TCP_NODELAY | Keepalive | Requested send buffer |
|---|---|---|---|
| Static | false | disabled | OS default |
| Wrapper fixture | true | 30 s idle / 1 s interval / 30 probes | OS default |
| WebSocket | false | disabled | 65536 bytes |

Both clients enable TCP_NODELAY.
The Go client retains Go's keepalive defaults.
The native client declares 30 s / 1 s / 30 probes.
Neither keepalive timer is exercised by the short active-request intervals.
Requested socket buffer values are not claims about exact kernel buffer capacities.

The Go file server can use sendfile.
The native static driver performs explicit file-read operations.
File data is warm after the per-row warmup.
These are equivalent file-service outcomes, not identical data paths or cold-disk measurements.

The Go WebSocket payload caps exclude metadata, allocation-capacity slack, and fixed driver reservations.
Native chat accounts for those resources.
These are not identical memory-accounting budgets or RSS limits.
Ordinary tests remain below both budgets.
Slow-recipient controls use the common two-message queue cap.

Client process resource counters come from Go self-`rusage` or external native `wait4`.
The Go process snapshot precedes final JSON serialization.
Native `wait4` includes the complete process lifetime.
Only the separately aligned measurement CPU total supplies client CPU/operation comparisons.
No native per-phase user/system split is invented.
Server context counts are sampled per-thread lower bounds.
Transient threads between samples can be missed.
Native final FD samples precede exit and are not post-exit FD counts.
Post-load RSS indicates resident retention, not live allocations or a leak.

## Resource challenges

All six groups passed independently of the throughput matrix.
Each HTTP target received nine intentional resets across partial headers, partial bodies, and stalled responses.
Each WebSocket target received six intentional resets plus the slow-consumer case.
Healthy recovery and descriptor bounds remained enabled.

- Native static returned to FD baseline 6.
- Native wrapper and chat returned to FD baseline 10.
- All Go references returned to FD baseline 6.
- Go HTTP temporarily retained up to two extra descriptors and settled within 0.414 seconds.
- Both chat targets closed the slow recipient with 1008 under an explicit five-second close timeout.
- Both healthy recipients received all twelve exact messages.

These are bounded recovery observations, not exhaustive leak proofs.
Intentional aborts are not counted as successful throughput.
[The resource summary](performance/resource-summary.json) and complete compressed report preserve the samples.

## Frozen sources and artifacts

All native servers use parent integration `284812f1f9b59256907524a8b3ef5529e56379d2`, release plus debuginfo2:

| Artifact | SHA256 |
|---|---|
| Static server | `1ff4f196d5d9c4aeed5d3dc1904c69b26699f9cf23fba34ee6ce6ab9c389115d` |
| Wrapper server | `afae8760d40b321366dcc600a072c90e0a74a395403e7da9cfdb4cfee4f336f7` |
| WebSocket chat | `9c0214e1ca366b47d923543023abfd00697f388d800bdbdc5555af7243c5aea2` |

The native benchmark client uses canonical integration `295a956cb34f1717bc9768824570cce904786671`, which includes `d8d55c43`.
Its SHA256 is `67a184b21c692cf944157638b843e8dec99c89d683cd9413f8272689cb769839`.
Older `8f80`, `101b6827`, and `989c9127` client artifacts did not enter these measurements.

The Go tool SHA256 is `209f52448528c2a8cc2b249d5257b010521bf18c8b89b15406345ed2def01c84`.
Its measured harness source is commit `c35dd177720704a01f419011b290105ba0263043`.
The Go version is 1.27.1.
Build flags are `-trimpath -buildvcs=false -pgo=off`.
The pinned WebSocket dependency is Gorilla v1.5.3.
The installed Go compiler is Microsoft's `go1.27.1-2-microsoft` toolset.
[The executable build information](performance/go-build-info.txt) records its system-crypto and compatibility settings.
The Go references and load generator share one executable.
The native services use separate executables.
Their whole-process RSS includes those build and runtime differences.
The new Go reference request cap is a harness policy change, not a production optimization.
Earlier Go profiling artifacts have different hashes and must retain their own source scope.

The large-chunked supplement uses Go tool SHA256 `0ccd7b424609fa22cace0fe97c88d2f9315a168dfd1278ddd7853ef7a511382c`.
Its benchmark-only source commit is `e5b39de2e79f09a82ca8575a61270084c4c609cb`.
The native wrapper executable remains the same `afae8760` artifact.
The earlier primary and one-core control retain their original `209f5244` Go tool identity.

Native compiler/profile declarations, complete command arrays, source trees, limits, and hashes are embedded in the raw reports.
The parent supplied native source/profile mappings.
The harness independently checked executable hashes.

## Persistent evidence and reproduction

- [Archive index and checksums](performance/archive-index.json)
- [Primary full JSON, gzip](performance/measure-238a3833220146e88206ed7ef6abc716.json.gz)
- [One-core control full JSON, gzip](performance/measure-6eaba4398f324834bc2bc0d424655539.json.gz)
- [Resource challenge full JSON, gzip](performance/resource-challenges-final.json.gz)
- [Large-chunked full JSON, gzip](performance/measure-15e7a12f87274236821d9ee353b0c827.json.gz)
- [Primary structured summary](performance/primary-summary.json)
- [One-core structured summary](performance/single-core-client-summary.json)
- [Resource summary](performance/resource-summary.json)
- [Large-chunked structured summary](performance/large-chunked-summary.json)
- [Independent large-chunked streaming proofs](performance/large-chunked-progress.json)
- [Harness usage and contracts](../../interop/perf/README.md)

The compressed reports contain complete data, not references to ephemeral worktree reports.
The archive index includes compressed and uncompressed hashes.
`interop/perf/summarize.py` reads either JSON or gzip JSON and reproduces the summaries and tables.
The raw reports retain each actual command and invocation result.
No native percentile bins were fabricated.

Earlier smoke-only publications remain in this directory for provenance.
They are not comparative performance evidence.
The final findings above use only the selected measured archives.
