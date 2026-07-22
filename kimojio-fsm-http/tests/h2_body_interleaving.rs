use std::collections::BTreeSet;

use kimojio_fsm_http::{
    ConnectionResponse, ExchangeId, H2Client, H2Frame, H2FrameType, H2OutboundCommit, HttpLimits,
    ServerConnection, ServerError, ServerEvent, Step,
};

const DEFAULT_MAX_FRAME_SIZE: usize = 16_384;
const BODY_LEN: usize = 32_000;
const _: () = assert!(BODY_LEN > DEFAULT_MAX_FRAME_SIZE);

#[derive(Debug)]
enum Observed {
    NeedInput,
    Write(Vec<u8>),
    Head(ExchangeId),
    Complete(ExchangeId),
    Done,
}

fn server_step(connection: &mut ServerConnection, input: &[u8]) -> Result<Observed, ServerError> {
    connection.step(input, |step| match step {
        Step::NeedInput => Observed::NeedInput,
        Step::Write(bytes) => Observed::Write(bytes.to_vec()),
        Step::Event(ServerEvent::RequestHead { exchange_id, .. }) => Observed::Head(exchange_id),
        Step::Event(ServerEvent::RequestComplete { exchange_id }) => {
            Observed::Complete(exchange_id)
        }
        Step::Done => Observed::Done,
        _ => panic!("unexpected server step"),
    })
}

fn take_client_block(client: &mut H2Client, commit: H2OutboundCommit) -> Vec<u8> {
    let block = client
        .next_outbound_block()
        .expect("client must queue request headers");
    assert_eq!(block.commit(), commit);
    let bytes = block.bytes().to_vec();
    client.acknowledge_outbound_block(commit).unwrap();
    bytes
}

fn open_request(client: &mut H2Client, target: &str) -> (u32, Vec<u8>) {
    let (stream_id, commit) = client
        .open_stream_with_raw_headers("GET", "http", "example.test", target, &[], true)
        .unwrap();
    (stream_id, take_client_block(client, commit))
}

fn collect_completed_requests(connection: &mut ServerConnection, input: &[u8]) -> Vec<ExchangeId> {
    let mut offset = 0;
    let mut heads = Vec::new();
    let mut completed = BTreeSet::new();

    for _ in 0..100 {
        let observed = server_step(connection, &input[offset..]).unwrap();
        let consumed = connection.consumed();
        match observed {
            Observed::Head(exchange_id) => heads.push(exchange_id),
            Observed::Complete(exchange_id) => {
                completed.insert(exchange_id);
            }
            Observed::Write(bytes) => assert!(!bytes.is_empty()),
            Observed::NeedInput => {
                panic!("driver requested input before both requests completed")
            }
            Observed::Done => panic!("driver finished before responses were prepared"),
        }
        connection.consume(consumed).unwrap();
        offset += consumed;
        assert!(offset <= input.len());

        if offset == input.len() && heads.len() == 2 && completed.len() == 2 {
            assert_eq!(heads.iter().copied().collect::<BTreeSet<_>>(), completed);
            return heads;
        }
    }

    panic!("request event collection did not converge")
}

fn take_response_headers(connection: &mut ServerConnection, expected_stream: u32) {
    let Observed::Write(bytes) = server_step(connection, &[]).unwrap() else {
        panic!("prepared response headers must be written");
    };
    assert_eq!(connection.consumed(), 0);
    connection.consume(0).unwrap();

    let (frame, consumed) = H2Frame::decode(&bytes).unwrap();
    assert_eq!(consumed, bytes.len());
    assert_eq!(frame.frame_type, H2FrameType::Headers);
    assert_eq!(frame.stream_id, expected_stream);
    assert_eq!(frame.flags & 0x1, 0);
}

struct OutboundBody {
    exchange_id: ExchangeId,
    stream_id: u32,
    bytes: Vec<u8>,
    sent: usize,
}

fn pump_response_bodies(
    connection: &mut ServerConnection,
    bodies: &mut [OutboundBody],
) -> Vec<H2Frame> {
    let mut frames = Vec::new();

    for _ in 0..100 {
        if bodies.iter().all(|body| body.sent == body.bytes.len()) {
            return frames;
        }

        let mut made_progress = false;
        for body in bodies.iter_mut() {
            let remaining = body.bytes.len() - body.sent;
            if remaining == 0
                || !connection
                    .prepare_body_chunk(body.exchange_id, remaining)
                    .unwrap()
            {
                continue;
            }

            let (mut wire, payload_len) = {
                let chunk = connection
                    .body_chunk(body.exchange_id)
                    .expect("scheduled exchange must expose body framing");
                (chunk.header().to_vec(), chunk.payload_len())
            };
            assert!(payload_len > 0);
            assert!(payload_len <= remaining);
            wire.extend_from_slice(&body.bytes[body.sent..body.sent + payload_len]);
            connection.commit_body_chunk(body.exchange_id).unwrap();

            let (frame, consumed) = H2Frame::decode(&wire).unwrap();
            assert_eq!(consumed, wire.len());
            assert_eq!(frame.frame_type, H2FrameType::Data);
            assert_eq!(frame.stream_id, body.stream_id);
            body.sent += payload_len;
            frames.push(frame);
            made_progress = true;
        }

        assert!(made_progress, "outbound body pump stalled");
    }

    panic!("outbound body pump did not converge")
}

#[test]
fn concurrent_http2_response_bodies_emit_interleaved_data_frames_before_either_stream_drains() {
    let mut client = H2Client::default();
    let mut input = client.connection_preface();
    let (first_stream, first_request) = open_request(&mut client, "/first");
    input.extend_from_slice(&first_request);
    let (second_stream, second_request) = open_request(&mut client, "/second");
    input.extend_from_slice(&second_request);

    let mut connection = ServerConnection::new(HttpLimits::new());
    let exchanges = collect_completed_requests(&mut connection, &input);
    let first_exchange = exchanges
        .iter()
        .copied()
        .find(|exchange_id| exchange_id.as_u64() == u64::from(first_stream))
        .unwrap();
    let second_exchange = exchanges
        .iter()
        .copied()
        .find(|exchange_id| exchange_id.as_u64() == u64::from(second_stream))
        .unwrap();

    for exchange_id in [first_exchange, second_exchange] {
        assert!(
            connection
                .prepare_response(
                    exchange_id,
                    ConnectionResponse {
                        status: 200,
                        reason: "OK",
                        headers: &[],
                        body_len: Some(BODY_LEN),
                    },
                )
                .unwrap()
        );
    }
    take_response_headers(&mut connection, first_stream);
    take_response_headers(&mut connection, second_stream);

    let first_body = vec![b'a'; BODY_LEN];
    let second_body = vec![b'b'; BODY_LEN];
    let mut bodies = [
        OutboundBody {
            exchange_id: first_exchange,
            stream_id: first_stream,
            bytes: first_body.clone(),
            sent: 0,
        },
        OutboundBody {
            exchange_id: second_exchange,
            stream_id: second_stream,
            bytes: second_body.clone(),
            sent: 0,
        },
    ];
    let frames = pump_response_bodies(&mut connection, &mut bodies);

    let chunk_order = frames
        .iter()
        .map(|frame| frame.stream_id)
        .collect::<Vec<_>>();
    assert!(
        frames
            .iter()
            .filter(|frame| frame.stream_id == first_stream)
            .count()
            > 1
    );
    assert!(
        frames
            .iter()
            .filter(|frame| frame.stream_id == second_stream)
            .count()
            > 1
    );

    let first_emitted_stream = chunk_order[0];
    let sibling_start = chunk_order
        .iter()
        .position(|stream_id| *stream_id != first_emitted_stream)
        .expect("both streams must emit DATA");
    let first_stream_finish = chunk_order
        .iter()
        .rposition(|stream_id| *stream_id == first_emitted_stream)
        .unwrap();
    assert!(
        sibling_start < first_stream_finish,
        "sibling response started only after the first response drained: {chunk_order:?}"
    );
    assert!(
        chunk_order.windows(2).all(|pair| pair[0] != pair[1]),
        "ready response streams did not alternate scheduler turns: {chunk_order:?}"
    );

    for (stream_id, expected) in [
        (first_stream, first_body.as_slice()),
        (second_stream, second_body.as_slice()),
    ] {
        let delivered = frames
            .iter()
            .filter(|frame| frame.stream_id == stream_id)
            .flat_map(|frame| frame.payload.iter().copied())
            .collect::<Vec<_>>();
        assert_eq!(delivered, expected);
    }

    assert!(matches!(
        server_step(&mut connection, &[]).unwrap(),
        Observed::Done
    ));
}
