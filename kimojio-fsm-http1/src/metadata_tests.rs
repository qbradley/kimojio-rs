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
