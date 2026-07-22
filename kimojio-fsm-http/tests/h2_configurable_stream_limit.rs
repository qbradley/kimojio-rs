use kimojio_fsm_http::{
    H2_DEFAULT_MAX_ACTIVE_STREAMS, H2Client, H2ErrorCode, H2Frame, H2FrameType, H2Limits,
    H2OutboundCommit, H2Server, H2SettingId, H2Settings, HttpLimits, HttpProtocol,
    ServerConnection, ServerError, ServerEvent, Step,
};

#[derive(Debug, Eq, PartialEq)]
enum Observed {
    NeedInput,
    Write(Vec<u8>),
    Head(u64),
    Complete(u64),
}

fn server_step(connection: &mut ServerConnection, input: &[u8]) -> Result<Observed, ServerError> {
    connection.step(input, |step| match step {
        Step::NeedInput => Observed::NeedInput,
        Step::Write(bytes) => Observed::Write(bytes.to_vec()),
        Step::Event(ServerEvent::RequestHead { exchange_id, .. }) => {
            Observed::Head(exchange_id.as_u64())
        }
        Step::Event(ServerEvent::RequestComplete { exchange_id }) => {
            Observed::Complete(exchange_id.as_u64())
        }
        _ => panic!("unexpected server step"),
    })
}

fn acknowledge_step(connection: &mut ServerConnection) {
    let consumed = connection.consumed();
    connection.consume(consumed).unwrap();
}

fn take_client_output(client: &mut H2Client, commit: H2OutboundCommit) -> Vec<u8> {
    let block = client.next_outbound_block().expect("queued request");
    assert_eq!(block.commit(), commit);
    let output = block.bytes().to_vec();
    client.acknowledge_outbound_block(commit).unwrap();
    output
}

fn open_request(client: &mut H2Client, path: &str) -> (u32, Vec<u8>) {
    let (stream_id, commit) = client
        .open_stream_with_raw_headers("GET", "http", "example.test", path, &[], true)
        .unwrap();
    (stream_id, take_client_output(client, commit))
}

fn advertised_stream_limit(output: &[u8]) -> Option<u32> {
    let mut offset = 0;
    while offset < output.len() {
        let (frame, consumed) = H2Frame::decode(&output[offset..]).unwrap();
        offset += consumed;
        if frame.frame_type == H2FrameType::Settings && frame.flags & 0x1 == 0 {
            return H2Settings::decode_payload(&frame.payload)
                .unwrap()
                .into_iter()
                .find(|setting| setting.id == H2SettingId::MaxConcurrentStreams)
                .map(|setting| setting.value);
        }
    }
    None
}

#[test]
fn configured_stream_ceiling_is_advertised_and_refuses_the_extra_stream() {
    let limits = HttpLimits::new().set_max_active_streams(1);
    let mut connection = ServerConnection::new_with_protocol(HttpProtocol::Http2, limits);
    let mut client = H2Client::default();

    let Observed::Write(settings) = server_step(&mut connection, &client.connection_preface())
        .expect("client preface must initialize the connection")
    else {
        panic!("server must write its initial settings");
    };
    assert_eq!(advertised_stream_limit(&settings), Some(1));
    acknowledge_step(&mut connection);

    let (first_stream, first) = open_request(&mut client, "/first");
    assert_eq!(
        server_step(&mut connection, &first).unwrap(),
        Observed::Head(u64::from(first_stream))
    );
    acknowledge_step(&mut connection);
    assert_eq!(
        server_step(&mut connection, &[]).unwrap(),
        Observed::Complete(u64::from(first_stream))
    );
    acknowledge_step(&mut connection);

    let (refused_stream, refused) = open_request(&mut client, "/refused");
    let Observed::Write(reset) = server_step(&mut connection, &refused).unwrap() else {
        panic!("the stream above the configured ceiling must be reset");
    };
    let (reset, consumed) = H2Frame::decode(&reset).unwrap();
    assert_eq!(consumed, 13);
    assert_eq!(reset.frame_type, H2FrameType::RstStream);
    assert_eq!(reset.stream_id, refused_stream);
    assert_eq!(
        reset.payload,
        H2ErrorCode::RefusedStream.as_u32().to_be_bytes()
    );
}

#[test]
fn default_stream_ceiling_remains_one_hundred() {
    let limits = HttpLimits::new();
    assert_eq!(limits.max_active_streams(), H2_DEFAULT_MAX_ACTIVE_STREAMS);
    assert_eq!(
        H2Limits::from_http_limits(limits).max_active_streams,
        H2_DEFAULT_MAX_ACTIVE_STREAMS
    );

    let mut connection = ServerConnection::new_with_protocol(HttpProtocol::Http2, limits);
    let mut client = H2Client::default();
    let Observed::Write(settings) =
        server_step(&mut connection, &client.connection_preface()).unwrap()
    else {
        panic!("server must write its initial settings");
    };
    assert_eq!(
        advertised_stream_limit(&settings),
        Some(H2_DEFAULT_MAX_ACTIVE_STREAMS as u32)
    );
}

#[test]
fn zero_stream_ceiling_is_rejected_by_h2_server_construction() {
    let http_limits = HttpLimits::new().set_max_active_streams(0);
    let h2_limits = H2Limits::from_http_limits(http_limits);
    assert_eq!(h2_limits.max_active_streams, 0);
    assert!(
        H2Server::with_local_flow_control_and_http_limits(65_535, 65_535, h2_limits, http_limits,)
            .is_err()
    );
}
