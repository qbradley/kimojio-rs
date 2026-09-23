use super::*;

fn observe(observer: &mut Observer, id: ExchangeId, parsed: ParsedHead<'_>) -> Option<()> {
    observer.exchange = Some(id);
    observer.record(match parsed {
        ParsedHead::Request(head) => format!("request {head:?}"),
        ParsedHead::Response(head, info) => format!("response {head:?} {info}"),
    })
}

fn metadata_fixture<const SERVER: bool>(
    rx: Rx,
    limit: usize,
    mode: Drive,
) -> (Core<Vec<u8>, Vec<u8>, SERVER>, Observer) {
    let mut core = Core::new(
        ConnectionId {
            slot: 12,
            generation: 3,
        },
        Config {
            head_timeout_ns: None,
            body_timeout_ns: None,
            idle_timeout_ns: None,
            continue_timeout_ns: None,
            ..Config::default()
        },
        vec![0; 256],
        Tick(0),
    )
    .unwrap();
    let mut observer = Observer::new(mode);
    if SERVER {
        core.metadata(
            b"POST / HTTP/1.1\r\nhost: a\r\ntransfer-encoding: chunked\r\n\r\n",
            &mut observer,
            observe,
        )
        .unwrap();
    } else {
        core.request(Request {
            head: RequestHead {
                method: "POST",
                target: "/",
                version: Version::Http11,
                headers: &[Header {
                    name: "host",
                    value: b"a",
                }],
            },
            body: BodyLength::Streaming,
            expect_continue: false,
        })
        .unwrap();
    }
    core.rx = rx;
    core.config.max_head_bytes = limit;
    core.config.max_chunk_line_bytes = limit;
    core.metadata_bytes = 0;
    core.metadata_fields = 0;
    core.chunk_metadata_bytes = 0;
    (core, observer)
}

fn compare<const SERVER: bool>(rx: Rx, wire: &[u8]) {
    for limit in [2, 16, 32, 128] {
        for split in 0..=wire.len() {
            for mode in [Drive::Continue, Drive::Yield, Drive::Mixed] {
                let (mut direct, mut dp) = metadata_fixture::<SERVER>(rx, limit, mode);
                let (mut buffered, mut bp) = metadata_fixture::<SERVER>(rx, limit, mode);
                for fragment in [&wire[..split], &wire[split..]] {
                    if fragment.is_empty() {
                        continue;
                    }
                    for core in [&mut direct, &mut buffered] {
                        let ReceiveStorage::Available(buffer) = &mut core.receive else {
                            unreachable!()
                        };
                        buffer[..fragment.len()].copy_from_slice(fragment);
                        core.start = 0;
                        core.end = fragment.len();
                    }
                    let a = direct.receive_metadata_with::<true, _>(&mut dp, observe);
                    let b = buffered.receive_metadata_with::<false, _>(&mut bp, observe);
                    assert_eq!(a, b);
                    assert_eq!(
                        format!("{direct:?}"),
                        format!("{buffered:?}"),
                        "server={SERVER} rx={rx:?} wire={wire:?} split={split} limit={limit}"
                    );
                    assert_eq!(dp.trace, bp.trace);
                    assert_eq!(dp.logs, bp.logs);
                    direct.assert_invariants();
                    buffered.assert_invariants();
                    if direct.failure.is_some()
                        || direct.metadata_bytes != 0
                        || direct.chunk_metadata_bytes != 0
                    {
                        break; // Exactly one section; following bytes must stay unread.
                    }
                }
            }
        }
    }
}

#[test]
fn contiguous_and_fragmented_metadata_match_buffered_reference_at_every_split() {
    for wire in [
        b"GET /a HTTP/1.1\r\nhost: a\r\n\r\nunread".as_slice(),
        b"GET / HTTP/1.1\r\nhost: bad host\r\n\r\nunread",
        b"GET / HTTP/1.1\nhost: a\r\n\r\n",
        b"GET / HTTP/1.1\r\nhost: a\rX",
    ] {
        compare::<true>(Rx::Head, wire);
    }
    for wire in [
        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nab".as_slice(),
        b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1",
        b"HTTP/1.1 200 OK\r\ncontent-length: 1, 2\r\n\r\nx",
        b"HTTP/1.1 200 OK\n\n",
    ] {
        compare::<false>(Rx::Head, wire);
    }
    for (rx, wire) in [
        (Rx::Size, b"2\r\nunread".as_slice()),
        (Rx::Size, b"aF; x=\"quoted\\\"value\"\r\nunread"),
        (Rx::Size, b"00000000000000000002\r\nunread"),
        (Rx::Size, b"ffffffffffffffff\r\nunread"),
        (Rx::Size, b"10000000000000000\r\nunread"),
        (Rx::Size, b"2;\r\nunread"),
        (Rx::Size, b"2\nunread"),
        (Rx::ChunkCrlf, b"\r\nunread"),
        (Rx::ChunkCrlf, b"x\r\nunread"),
        (Rx::ChunkCrlf, b"\rXunread"),
        (Rx::Trailers, b"\r\nunread"),
        (Rx::Trailers, b"x-value: a\r\n\r\nunread"),
        (Rx::Trailers, b"content-length: 2\r\n\r\nunread"),
    ] {
        compare::<true>(rx, wire);
        compare::<false>(rx, wire);
    }
}

#[test]
fn chunk_continuations_match_full_selection_with_credit_deadlines_and_abort() {
    #[derive(Debug, Eq, PartialEq)]
    struct Outcome {
        state: String,
        callbacks: Vec<String>,
        logs: Vec<(ConnectionId, Tick, LogEvent)>,
        consumed: Vec<u8>,
    }
    fn run<const SERVER: bool>(mode: Drive, limit: usize, abort: bool, timers: bool) -> Outcome {
        let wire = b"2\r\nab\r\n2\r\nab\r\n2\r\nab\r\n0\r\n\r\nTAIL";
        let (mut core, mut observer) = metadata_fixture::<SERVER>(Rx::Size, 128, mode);
        if !SERVER {
            // Model a client whose bodyless upload has already settled; the
            // independently arriving chunked response is now ready to consume.
            core.output = None;
            core.tx = Transmit::begin(Framing::Empty);
            core.tx.settle();
            core.exchange.as_mut().unwrap().source_notification = Notification::Delivered;
            core.close_after = true;
            observer.sources = 1;
        }
        observer.expected_receipts = 0;
        let ReceiveStorage::Available(buffer) = &mut core.receive else {
            unreachable!()
        };
        buffer[..wire.len()].copy_from_slice(wire);
        core.end = wire.len();
        core.config.max_chunk_metadata_bytes = limit;
        core.config.body_timeout_ns = timers.then_some(100);
        core.set_deadline(TimerPhase::Body, core.config.body_timeout_ns)
            .unwrap();
        let exchange = core.exchange.as_ref().unwrap().id;
        core.grant_body_credit(exchange, 2).unwrap();
        let mut consumed = Vec::new();
        let mut withheld = false;
        for step in 0..8 {
            drive(&mut core, &mut observer);
            let Some(body) = observer.body.take() else {
                break;
            };
            if !withheld {
                // Returning zero must suspend delivery even with buffered data.
                core.release_body(body.release(0)).unwrap();
                drive(&mut core, &mut observer);
                assert!(observer.body.is_none());
                core.grant_body_credit(exchange, 2).unwrap();
                withheld = true;
                continue;
            }
            consumed.extend_from_slice(body.bytes());
            if abort {
                core.shutdown(ShutdownMode::Abort);
                drive(&mut core, &mut observer);
                assert!(observer.close.is_none(), "held body still owns storage");
            }
            core.observe_time(Tick(step + 1)).unwrap();
            core.release_body(body.release(2)).unwrap();
            if core.failure.is_none() {
                core.grant_body_credit(exchange, 2).unwrap();
            }
        }
        drive(&mut core, &mut observer);
        assert!(observer.body.is_none());
        if !abort && limit == usize::MAX {
            assert_eq!(consumed, b"ababab");
            assert_eq!(
                &core.receive.buffer().unwrap()[core.start..core.end],
                b"TAIL"
            );
        } else {
            assert!(core.failure.is_some());
        }
        Outcome {
            state: format!("{core:?}"),
            callbacks: observer.trace,
            logs: observer.logs,
            consumed,
        }
    }
    fn check<const SERVER: bool>() {
        for limit in [4, 10, usize::MAX] {
            for abort in [false, true] {
                for timers in [false, true] {
                    let expected = run::<SERVER>(Drive::Steps, limit, abort, timers);
                    for mode in [Drive::Continue, Drive::Yield, Drive::Mixed] {
                        assert_eq!(run::<SERVER>(mode, limit, abort, timers), expected);
                    }
                }
            }
        }
    }
    check::<true>();
    check::<false>();
}

#[test]
fn contiguous_head_borrows_input_and_restores_storage_before_yield() {
    let wire = b"GET /borrowed HTTP/1.1\r\nhost: a\r\n\r\nab";
    let (mut core, mut observer) = metadata_fixture::<true>(Rx::Head, 128, Drive::Yield);
    let ReceiveStorage::Available(buffer) = &mut core.receive else {
        unreachable!()
    };
    buffer[..wire.len()].copy_from_slice(wire);
    let expected = buffer[4..].as_ptr() as usize;
    core.end = wire.len();
    fn pointer(observer: &mut Observer, _: ExchangeId, parsed: ParsedHead<'_>) -> Option<()> {
        let ParsedHead::Request(head) = parsed else {
            unreachable!()
        };
        observer.record((head.target.as_ptr() as usize).to_string())
    }
    assert_eq!(core.receive_metadata(&mut observer, pointer), Some(()));
    assert_eq!(observer.trace.last().unwrap(), &expected.to_string());
    assert_eq!(
        core.head.capacity(),
        0,
        "complete metadata must not allocate scratch"
    );
    assert_eq!(&core.receive.buffer().unwrap()[core.start..core.end], b"ab");
    core.assert_invariants();
}
