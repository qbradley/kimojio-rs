#[path = "../benches/support/mod.rs"]
mod support;
use support::*;

#[test]
fn timed_matrix_qualifies_bytes_counts_and_settlement_on_both_backends() {
    for backend in [Backend::Native, Backend::Stream] {
        for (bytes, chunked, chunk_bytes) in [
            (0, false, IO_BYTES),
            (128, false, IO_BYTES),
            (1024 * 1024, true, IO_BYTES),
            (8192, true, 512),
        ] {
            let fixture = Fixture::new(bytes, chunked, chunk_bytes);
            let options = Options::new(backend);
            let checked = run::<true>(&fixture, options, 3);
            let timed = run::<false>(&fixture, options, 3);
            assert_eq!(checked.measured, 3);
            assert_eq!(checked.diagnostics.unwrap().retired, 2 * (WARMUP + 3));
            assert!(timed.diagnostics.is_none());
            assert_eq!(checked.server.exchanges, WARMUP + 3);
            assert_eq!(checked.server.bytes, (WARMUP + 3) * bytes as u64);
            assert_eq!(checked.client.bytes, checked.server.bytes);
            assert_eq!(checked.client.exchanges, checked.server.exchanges);
            // Kernel read segmentation may differ; bytes and exchanges may not.
            assert_eq!(timed.client.bytes, checked.client.bytes);
            assert_eq!(timed.server.bytes, checked.server.bytes);
            assert_eq!(timed.server.exchanges, checked.server.exchanges);
            assert!(!checked.elapsed.is_zero() && !timed.elapsed.is_zero());
            eprintln!(
                "{} bytes={bytes} chunked={chunked}: {checked:?}",
                backend.name()
            );
        }
    }
}

#[test]
fn controls_and_duplex_forwarding_preserve_success_and_payloads() {
    for backend in [Backend::Native, Backend::Stream] {
        for mode in [Mode::Exchange, Mode::Forward, Mode::CopyForward] {
            for deadlines in [false, true] {
                // Uneven frames and bodies also exercise partial final frames.
                for (bytes, chunked, chunk_bytes) in [
                    (0, true, 7),
                    (79, true, 7),
                    (128, false, IO_BYTES),
                    (65_537, false, 4096),
                ] {
                    let fixture = Fixture::new(bytes, chunked, chunk_bytes);
                    let outcome = run::<true>(
                        &fixture,
                        Options {
                            backend,
                            mode,
                            deadlines,
                            coalesce: true,
                        },
                        2,
                    );
                    assert_eq!(outcome.server.exchanges, WARMUP + 2);
                    assert_eq!(outcome.server.bytes, (WARMUP + 2) * bytes as u64);
                    assert_eq!(outcome.client.bytes, outcome.server.bytes);
                }
            }
        }
    }
}

#[test]
fn shared_bodies_preserve_payload_framing_and_reuse_on_both_transports() {
    for backend in [Backend::Native, Backend::Stream] {
        for coalesce in [false, true] {
            for (bytes, chunked, chunk_bytes) in [
                (0, false, IO_BYTES),
                (128, false, IO_BYTES),
                (65_537, true, 4096),
            ] {
                let fixture = Fixture::new(bytes, chunked, chunk_bytes).shared();
                let result = run::<true>(
                    &fixture,
                    Options {
                        coalesce,
                        ..Options::new(backend)
                    },
                    3,
                );
                assert_eq!(result.server.bytes, (WARMUP + 3) * bytes as u64);
                assert_eq!(result.client.bytes, result.server.bytes);
                assert_eq!(result.diagnostics.unwrap().retired, 2 * (WARMUP + 3));
            }
        }
    }
}

#[test]
fn qualification_diagnostics_identify_coalescing_and_deadline_controls() {
    let fixture = Fixture::new(128, false, IO_BYTES);
    for (coalesce, deadlines) in [(false, true), (true, true), (false, false)] {
        let outcome = run::<true>(
            &fixture,
            Options {
                coalesce,
                deadlines,
                ..Options::new(Backend::Native)
            },
            32,
        );
        let diagnostics = outcome.diagnostics.unwrap();
        // Small writes fit in the socket buffers. These are core operation
        // counts, not a count of io_uring_enter system calls.
        assert_eq!(
            diagnostics.writes,
            (WARMUP + 32) * if coalesce { 2 } else { 4 }
        );
        assert_eq!(diagnostics.deadlines == 0, !deadlines);
        eprintln!("coalesce={coalesce} deadlines={deadlines}: {diagnostics:?}");
    }
}

#[test]
fn session_outlives_the_default_core_request_limit_without_reconnect() {
    for backend in [Backend::Native, Backend::Stream] {
        let result = run::<true>(
            &Fixture::new(0, false, IO_BYTES),
            Options::new(backend),
            1025,
        );
        assert_eq!(result.server.exchanges, WARMUP + 1025);
        assert_eq!(result.client.exchanges, result.server.exchanges);
    }
}

#[test]
fn validation_rejects_corruption_and_length_errors() {
    let mut at = 0;
    check_chunk::<true>(b"abcde", b"abc", &mut at).unwrap();
    assert_eq!(at, 3);
    assert!(check_chunk::<true>(b"abcde", b"xx", &mut at).is_err());
    assert_eq!(at, 3);
    assert!(check_chunk::<false>(b"abcde", b"def", &mut at).is_err());
    assert_eq!(at, 3);
    check_chunk::<true>(b"abcde", b"de", &mut at).unwrap();
    assert_eq!(at, 5);
    let mut overflow = usize::MAX;
    assert!(check_chunk::<false>(b"", b"x", &mut overflow).is_err());
}

#[test]
#[should_panic(expected = "invalid wrapper benchmark")]
fn zero_iteration_batch_is_not_a_timing_result() {
    run::<true>(
        &Fixture::new(0, false, IO_BYTES),
        Options::new(Backend::Native),
        0,
    );
}
