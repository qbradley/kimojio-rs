use kimojio_fsm_http::{
    ConnectionResponse, ExchangeId, H2Client, H2ErrorCode, H2Frame, H2FrameType, H2OutboundCommit,
    HttpLimits, HttpProtocol, ServerConnection, ServerError, ServerEvent, Step,
};

#[derive(Debug, Eq, PartialEq)]
enum Observed {
    NeedInput,
    Write,
    Head {
        exchange_id: ExchangeId,
        target: Vec<u8>,
    },
    Complete(ExchangeId),
    Done,
}

fn step(connection: &mut ServerConnection, input: &[u8]) -> Result<Observed, ServerError> {
    connection.step(input, |step| match step {
        Step::NeedInput => Observed::NeedInput,
        Step::Write(_) => Observed::Write,
        Step::Event(ServerEvent::RequestHead {
            exchange_id,
            target,
            ..
        }) => Observed::Head {
            exchange_id,
            target: target.to_vec(),
        },
        Step::Event(ServerEvent::RequestComplete { exchange_id }) => {
            Observed::Complete(exchange_id)
        }
        Step::Done => Observed::Done,
        _ => panic!("unexpected server event"),
    })
}

fn take_client_block(client: &mut H2Client, commit: H2OutboundCommit) -> Vec<u8> {
    let block = client.next_outbound_block().expect("queued request");
    assert_eq!(block.commit(), commit);
    let bytes = block.bytes().to_vec();
    client.acknowledge_outbound_block(commit).unwrap();
    bytes
}

fn drive_input(connection: &mut ServerConnection, input: &[u8]) -> Vec<Observed> {
    let mut offset = 0;
    let mut observed = Vec::new();
    for _ in 0..64 {
        let event = step(connection, &input[offset..]).unwrap();
        let consumed = connection.consumed();
        connection.consume(consumed).unwrap();
        offset += consumed;
        let idle = matches!(event, Observed::NeedInput) && consumed == 0;
        observed.push(event);
        if idle {
            assert_eq!(offset, input.len());
            return observed;
        }
    }
    panic!("driver did not consume the supplied input");
}

fn request(client: &mut H2Client, target: &str, end_stream: bool) -> (u32, Vec<u8>) {
    let (stream_id, commit) = client
        .open_stream("GET", "http", "example.test", target, &[], end_stream)
        .unwrap();
    (stream_id, take_client_block(client, commit))
}

#[test]
fn peer_reset_reports_exact_frame_consumption_and_preserves_following_frame() {
    let mut client = H2Client::default();
    let mut connection =
        ServerConnection::new_with_protocol(HttpProtocol::Http2, HttpLimits::new());
    drive_input(&mut connection, &client.connection_preface());

    let (reset_stream, reset_request) = request(&mut client, "/reset", false);
    let reset_exchange = drive_input(&mut connection, &reset_request)
        .into_iter()
        .find_map(|event| match event {
            Observed::Head {
                exchange_id,
                target,
            } if target == b"/reset" => Some(exchange_id),
            _ => None,
        })
        .expect("reset stream request head");

    let (_, sibling_request) = request(&mut client, "/sibling", true);
    let sibling_exchange = drive_input(&mut connection, &sibling_request)
        .into_iter()
        .find_map(|event| match event {
            Observed::Head {
                exchange_id,
                target,
            } if target == b"/sibling" => Some(exchange_id),
            _ => None,
        })
        .expect("sibling request head");

    let (_, after_request) = request(&mut client, "/after", true);
    let mut input = Vec::new();
    H2Frame {
        frame_type: H2FrameType::RstStream,
        flags: 0,
        stream_id: reset_stream,
        payload: H2ErrorCode::Cancel.as_u32().to_be_bytes().to_vec(),
    }
    .encode(&mut input);
    let reset_len = input.len();
    input.extend_from_slice(&after_request);

    assert_eq!(
        step(&mut connection, &input),
        Err(ServerError::PeerReset {
            stream_id: reset_stream,
            error_code: H2ErrorCode::Cancel.as_u32(),
        })
    );
    assert_eq!(connection.consumed(), reset_len);
    assert_eq!(connection.cancel_exchange().unwrap(), Some(reset_exchange));
    connection.consume(reset_len).unwrap();
    assert!(connection.begin_next_exchange(reset_exchange).unwrap());

    let after = drive_input(&mut connection, &input[reset_len..]);
    assert!(after.iter().any(|event| {
        matches!(
            event,
            Observed::Head { target, .. } if target == b"/after"
        )
    }));

    assert!(
        !connection
            .prepare_response(
                sibling_exchange,
                ConnectionResponse {
                    status: 204,
                    reason: "No Content",
                    headers: &[],
                    body_len: Some(0),
                },
            )
            .unwrap()
    );
    assert!(matches!(
        step(&mut connection, &[]).unwrap(),
        Observed::Write
    ));
}
