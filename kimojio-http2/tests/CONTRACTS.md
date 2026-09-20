# Wrapper contract evidence

This matrix describes assertions in this crate, not a claim that every listed contract has complete qualification.
The evidence uses real Kimojio runtime operations and established sockets.
Some tests also use a direct-core peer or a controllable generic transport.
This document does not substitute for independent peer qualification, kernel-race coverage, or performance measurements.

## Test names

`acceptance::name`, `generic::name`, `native_server::name`, `informational::name`, and `observation::name` identify integration binaries and exact test filters.
Their source files have the corresponding names in this directory.
`native_tests::name` identifies a library test in `../src/native_tests.rs`.
`io::stream::tests::name` identifies a library test in `../src/io/stream/tests.rs`.

## Contract matrix

| # | Contract | Exact tests | Observable sequence | Remaining limits |
| --- | --- | --- | --- | --- |
| 1 | Queued cancellation before admission and under full submission pressure | `acceptance::full_submission_budget_cancellation_has_no_headers_source_poll_or_sibling_reset`<br>`native_tests::bounded_queue_reports_item_and_storage_overflow`<br>`observation::unadmitted_rejection_and_queued_cancellation_have_no_synthetic_identity_or_retirement` | One case fills the item budget before the driver starts. Another fills it behind an active sibling. Overflow returns `Error::Limit`. Cancellation produces no admitted ID, receive END, retirement, peer request, or source poll. The active sibling and a later request complete. | The request channel is native and unbounded, with explicit item/storage reservations. There is no physically full bounded request sender. Storage-overflow rejection has coverage, but cancellation under storage-only pressure does not have a separate sequence. |
| 2 | Terminal delivery and response-body drop before or after receive END | `acceptance::terminal_response_drop_crossproduct_preserves_delayed_upload_and_retirement`<br>`native_server::dropping_request_after_terminal_delivery_does_not_reset_response` | Nine client sequences combine HEADERS-END, DATA-END, and trailers-END with three notification schedules. Each sequence asserts the authoritative receive state at the drop. The upload starts only after the drop, delivers 65,536 bytes, and retires without a receipt or contextual error. A sibling then completes. | The exact timing crossproduct uses native transport. Generic early-response tests cover successful behavior, not this forced notification schedule. Arbitrary kernel batching is not an enumerated crossproduct. |
| 3 | Early RST_STREAM(NO_ERROR) preserves receive END and exposes retirement separately | `native_tests::reset_zero_retirement_is_independent_of_a_failed_buffer_receipt` | Both 200 and 413 complete their receive half before the upload starts. The peer then sends reset0. The report preserves receive `Complete`, actual retirement `Reset(0)`, and a separate exact failed receipt. Its accepted prefix is greater than zero and less than 65,536. A sibling and graceful close succeed. | The peer resets after its first received DATA. This is not every legal reset position or cancellation ordering. |
| 4 | Native-I/O sources and handlers cancel locally, without orphaned originals | `native_tests::producer_native_read_is_revoked_without_cancelling_sibling_io`<br>`native_server::handler_and_response_source_native_reads_cancel_without_affecting_siblings`<br>`generic::stream_cancellation_releases_handler_and_source_io_without_stopping_siblings` | A source or handler enters a real native read before stream cancellation. The server test subsequently writes and reads a sentinel through the same descriptor. Siblings complete without connection cancellation. The generic test observes both task guards after cancellation. | The tests do not trace every native cancellation CQE. The client-source case does not independently reuse its descriptor after cancellation. Cancellation-ignoring non-native futures remain outside this guarantee. |
| 5 | Held chunks remain readable after actual close and delay retirement | `native_tests::held_chunk_outlives_actual_close_and_release_completes_retirement`<br>`native_server::server_request_chunk_survives_actual_close_until_release`<br>`observation::observed_retirement_still_waits_for_leases_after_actual_descriptor_close`<br>`generic::generic_close_precedes_held_chunk_release_and_retirement`<br>`generic::server_close_precedes_held_request_release_and_retirement` | The connection closes while an application retains a chunk. The chunk remains readable. The driver or retirement waiter remains pending. Release permits the actual retirement and driver return. Generic probes mark close only after the native close operation completes. | These are explicit cooperative shutdown sequences. Dropping the entire driver is not equivalent to polling it through close and retirement. |
| 6 | A release alone wakes blocked capacity | `generic::lease_release_alone_wakes_blocked_admission_without_new_read_completion`<br>`generic::receive_page_exhaustion_is_terminal_not_release_backpressure` | A held chunk prevents retirement and occupies the sole active-stream slot. A second request waits for admission. Releasing the chunk admits that request while read/write call counts remain unchanged and the original read remains pending. No new application command or clock advance triggers admission. | This proves stream-admission capacity, not suspended receive-page allocation. Receive-page exhaustion is terminal: the second test observes `ResourceExhausted`, actual close, readable held chunks, and failed retirement after release. It does not resume that failed connection. |
| 7 | Metadata pressure retries only transient failures, without replay or invalid commitment | `native_tests::blocked_metadata_retries_and_queued_cancellation_preserve_siblings`<br>`native_server::response_and_trailer_admission_pressure_never_replays_headers`<br>`informational::queued_information_precedes_final_metadata_under_admission_pressure`<br>`acceptance::permanent_metadata_rejection_preserves_identity_sources_and_later_field_decoding` | Local concurrency pressure releases a queued valid request. Small item/byte budgets exercise 24 concurrent response, trailer, and informational exchanges. Permanent oversize returns both `Capacity` and `Message` in separate cases. Rejected requests have no admission or source poll. Later requests retain consecutive IDs and correctly decoded repeated fields. | Peer concurrency changing from zero to positive has no direct wrapper test here. The stress tests do not count internal `Blocked` attempts. Correct subsequent decoding is black-box evidence, not an assertion of byte-identical HPACK state. |
| 8 | Cancellation acknowledgment, original settlement, late progress, and close ordering remain distinct | `io::stream::tests::cancellation_settles_write_all_continuations_without_cancelling_siblings`<br>`io::stream::tests::cancellation_preserves_a_late_original_read_success`<br>`native_tests::reset_zero_retirement_is_independent_of_a_failed_buffer_receipt`<br>`generic::graceful_close_failure_is_not_success_in_either_role` | Real native operations produce positive results before the generic original returns. Cancellation preserves late success and cancels subsequent write-all operations. Unrelated native I/O completes. The reset test supplies an exact positive native buffer prefix. Generic close probes require settled originals and reader release. | The wrapper acknowledges a core cancel command before it polls the original again. The public API cannot independently schedule both core ACK/completion orders. Native close-failure injection and nondeterministic kernel-race enumeration remain unqualified. Generic close errors are adapter fault injection, not failed native close syscalls. |
| 9 | Hidden progress from failed generic write-all remains a lower bound, without replay | `generic::partial_write_failure_keeps_lower_bound_receipt_errno_and_close_error`<br>`generic::server_partial_write_failure_preserves_its_original_receipt_and_source` | The transport really writes a positive payload prefix, then returns `Errno::IO` without a count. The failed receipt has `exact == false` and an accepted count no greater than that prefix. The payload has one submission. The driver retains the source error and any additional close error. | The underlying trait cannot expose the hidden prefix exactly. This fixture does not qualify every external `SplittableStream` implementation. |
| 10 | An unread request does not reset an early response, and discarded leases do not prevent completion | `native_server::unread_request_drop_preserves_each_response_end_shape`<br>`native_server::early_final_response_keeps_a_delayed_upload_alive`<br>`native_server::dropping_request_after_terminal_delivery_does_not_reset_response`<br>`generic::unread_requests_do_not_cancel_generic_early_responses_or_uploads` | Nine native cases combine empty, DATA, and trailer request/response shapes. Early 413 also precedes a gated upload. Generic cases preserve all 64 upload chunks after request abandonment. The body results, stream retirement, and connection drivers complete. | Retirement and continued window progress demonstrate lease reclamation. These tests do not measure the server queue depth at abandonment. They do not enumerate every partial unread-body position. |
| 11 | Virtual time, deadlines, cancellation, and errors have distinct causes | `acceptance::virtual_application_deadlines_cancel_admission_and_body_without_masking_source_errors`<br>`native_tests::settings_timeout_uses_virtual_clock_and_closes_descriptor`<br>`native_server::graceful_server_deadline_revokes_a_pending_handler_in_virtual_time`<br>`generic::generic_graceful_deadline_uses_virtual_time_and_closes_after_handler_settlement`<br>`generic::read_failure_preserves_source_errno_and_closes_original_halves` | Virtual time expires SETTINGS, application admission/body waits, and native/generic graceful deadlines. The expired queued request gets no ID, peer request, or source poll. The expired body wait preserves incomplete receive state before explicit cancellation produces `Reset(8)`. An earlier source error remains `Error::Application`, without clock advancement. Siblings and connection drivers complete. The separate generic I/O-failure test retains `Errno::NOENT`. | Application deadlines use `operations::timeout_at`, not a new wrapper deadline API. Core per-stream deadlines, simultaneous error/expiry ties, and alarm-failure injection remain unqualified here. The timeout cases require `virtual-clock`. |
| 12 | Same-connection and cross-connection duplex forwarding exceeds actual windows | `native_server::repeated_concurrent_streaming_echo_exceeds_both_actual_windows`<br>`native_server::reset_during_zero_copy_echo_releases_both_directions_and_preserves_sibling`<br>`generic::paused_generic_consumer_bounds_production_and_preserves_sibling_progress`<br>`acceptance::cross_connection_duplex_forwarding_overlaps_both_windows_and_retires_all_streams` | The cross-connection relay forwards 1,572,864 bytes and trailers through two connections with 65,535-byte stream and connection windows. Echoed DATA arrives before upload EOF. A paused consumer limits production while a sibling completes. Frontend/backend client reports complete, and all four drivers return after graceful close. | The pause proves bounded production, not an exact peak count of every internal lease. The cross-connection sequence uses native adapters. It is not a TLS, generic cross-connection, or performance qualification. |

## Notification schedules in contract 2

The new test uses only public wrapper futures, body methods, and `RequestObserver`.
Its application driver polls the application immediately after each connection poll.
It does not inject a core completion or edit a core notification queue.

| Schedule | Turn budget | Assertion immediately before body drop |
| --- | --- | --- |
| Pending notification | 1 | Both `IncomingBody::receive_outcome()` and the observer still contain `None`. |
| Delivered notification | 1 | Both contain `Some(StreamOutcome::Complete)`, and `frame()` returns receive EOF. |
| Batched processing | 64 | The same completed receive outcome precedes the drop. |

Every schedule runs once for each terminal shape.
The server receives the entire delayed upload after the client drops its response.
The test also joins the server's body-reader tasks.
No content-length calculation substitutes for the actual receive notification.

## Scope of this evidence change

This change adds seven tests and strengthens the positive-prefix assertion in the existing reset0 test.
The SETTINGS timeout test also requires the actual protocol timeout code and no remaining virtual timers.
It changes no production wrapper, core, runtime, independent peer fixture, or benchmark implementation.
The failed receive-allocation experiment led to a terminal-resource test, not a change in resource policy.

The following gaps remain explicit:

- Zero-to-positive peer concurrency at the wrapper boundary.
- Internal HPACK-state equality and counts of blocked metadata attempts.
- Both externally controlled cancellation ACK/completion orders.
- Native close-failure injection and enumeration of kernel races.
- Core per-stream deadlines, alarm-failure injection, and error/deadline ties.
- Exact aggregate lease peaks and generic cross-connection forwarding.

These gaps describe the crate tests, not all repository evidence.
The independent `admission-recovery` peer case covers concurrency changes from zero to positive for both wrapper transports.
That case passed on fixture `b3789dc6`, with unchanged production wrapper source `d8ca94b6`.
The [qualification ledger](../../docs/http2-qualification.md) records the peer profile, binary hash, and remaining exclusions.

## Reproduction

From the repository root, run the applicable integration binary or library filter.
The new focused integration cases use these commands:

```sh
cargo test -p kimojio-http2 --all-features --test acceptance
cargo test -p kimojio-http2 --test generic lease_release_alone
cargo test -p kimojio-http2 --test generic receive_page_exhaustion
cargo test -p kimojio-http2 --lib reset_zero_retirement
```

The worktree checkpoint uses the assigned private target and CPUs 8–31:

```sh
taskset -c 8-31 env CARGO_TARGET_DIR=/workspace/kimojio-rs/target/http2-program/build-http2-wrapper cargo fmt -p kimojio-http2
taskset -c 8-31 env CARGO_TARGET_DIR=/workspace/kimojio-rs/target/http2-program/build-http2-wrapper cargo clippy -p kimojio-http2
taskset -c 8-31 env CARGO_TARGET_DIR=/workspace/kimojio-rs/target/http2-program/build-http2-wrapper cargo clippy -p kimojio-http2 --all-targets --all-features
taskset -c 8-31 env CARGO_TARGET_DIR=/workspace/kimojio-rs/target/http2-program/build-http2-wrapper cargo test -p kimojio-http2 --all-features
taskset -c 8-31 env CARGO_TARGET_DIR=/workspace/kimojio-rs/target/http2-program/build-http2-wrapper cargo test -p kimojio-http2 --release --all-features
```

Formatting and both clippy commands passed.
The complete all-features suite passed in debug and release: 74 tests and 5 doctests in each configuration.
These results qualify only the observations in the matrix, not its remaining gaps.
