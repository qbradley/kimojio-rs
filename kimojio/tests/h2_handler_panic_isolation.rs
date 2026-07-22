#![cfg(feature = "http")]

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpStream};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use kimojio::http::{
    Body, Error as HttpError, HeaderMap, HeaderValue, ProtocolErrorKind, Response, ServeError,
    Server, ServerConfig,
};
use kimojio::{AsyncEvent, CancellationToken, operations};
use kimojio_fsm_http::{
    H2ByteClientEvent, H2Client, H2ErrorCode, H2Frame, H2FrameType, H2OutboundCommit,
};

const WAIT: Duration = Duration::from_secs(10);

#[derive(Default)]
struct ObservedResponse {
    status: Option<u16>,
    body: Vec<u8>,
    complete: bool,
    reset: Option<u32>,
}

fn take_client_block(client: &mut H2Client, commit: H2OutboundCommit) -> Vec<u8> {
    let block = client.next_outbound_block().expect("queued request");
    assert_eq!(block.commit(), commit);
    let bytes = block.bytes().to_vec();
    client.acknowledge_outbound_block(commit).unwrap();
    bytes
}

fn open_request(client: &mut H2Client, target: &str) -> (u32, Vec<u8>) {
    let (stream_id, commit) = client
        .open_stream("GET", "http", "localhost", target, &[], true)
        .unwrap();
    (stream_id, take_client_block(client, commit))
}

fn read_complete_responses(
    stream: &mut TcpStream,
    client: &mut H2Client,
    pending: &mut Vec<u8>,
    responses: &mut HashMap<u32, ObservedResponse>,
    required: &[u32],
) {
    while required.iter().any(|stream_id| {
        !responses
            .get(stream_id)
            .is_some_and(|response| response.complete)
    }) {
        let mut bytes = [0u8; 4096];
        let read = stream.read(&mut bytes).unwrap();
        assert_ne!(read, 0, "server closed before all responses completed");
        pending.extend_from_slice(&bytes[..read]);

        let mut consumed_total = 0;
        loop {
            let (event, consumed, output) =
                client.accept_bytes(&pending[consumed_total..]).unwrap();
            if !output.is_empty() {
                stream.write_all(&output).unwrap();
            }
            if consumed == 0 {
                break;
            }
            consumed_total += consumed;
            match event {
                Some(H2ByteClientEvent::ResponseHeaders {
                    stream_id,
                    headers,
                    end_stream,
                }) => {
                    let response = responses.entry(stream_id).or_default();
                    response.status = headers
                        .iter()
                        .find(|header| header.name == b":status")
                        .map(|header| std::str::from_utf8(&header.value).unwrap().parse().unwrap());
                    response.complete = end_stream;
                }
                Some(H2ByteClientEvent::Data {
                    stream_id,
                    payload,
                    end_stream,
                    ..
                }) => {
                    let response = responses.entry(stream_id).or_default();
                    response.body.extend_from_slice(&payload);
                    response.complete = end_stream;
                }
                Some(H2ByteClientEvent::Reset {
                    stream_id,
                    error_code,
                }) => {
                    let response = responses.entry(stream_id).or_default();
                    response.reset = Some(error_code);
                    response.complete = true;
                }
                _ => {}
            }
        }
        pending.drain(..consumed_total);
    }
}

#[derive(Debug)]
struct ResponseBodyFailure;

impl std::fmt::Display for ResponseBodyFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("intentional response body failure")
    }
}

impl std::error::Error for ResponseBodyFailure {}

#[kimojio::test]
async fn failing_streaming_http2_body_resets_only_its_stream() {
    let server = Server::bind((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
    let address = server.local_addr();
    let peer_finished = Arc::new(AtomicBool::new(false));
    let finished = Arc::clone(&peer_finished);
    let peer = std::thread::spawn(move || {
        let mut client = H2Client::default();
        let mut outbound = client.connection_preface();
        let (failed_stream, failed_request) = open_request(&mut client, "/failed");
        outbound.extend_from_slice(&failed_request);
        let (first_sibling, first_request) = open_request(&mut client, "/sibling-one");
        outbound.extend_from_slice(&first_request);
        let (second_sibling, second_request) = open_request(&mut client, "/sibling-two");
        outbound.extend_from_slice(&second_request);

        let mut stream = TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        stream.set_write_timeout(Some(WAIT)).unwrap();
        stream.write_all(&outbound).unwrap();

        let mut pending = Vec::new();
        let mut responses = HashMap::new();
        read_complete_responses(
            &mut stream,
            &mut client,
            &mut pending,
            &mut responses,
            &[failed_stream, first_sibling, second_sibling],
        );

        let (after_stream, after_request) = open_request(&mut client, "/after-body-error");
        stream.write_all(&after_request).unwrap();
        read_complete_responses(
            &mut stream,
            &mut client,
            &mut pending,
            &mut responses,
            &[after_stream],
        );
        finished.store(true, Ordering::Release);
        (
            failed_stream,
            first_sibling,
            second_sibling,
            after_stream,
            responses,
        )
    });

    let reports = Rc::new(RefCell::new(Vec::new()));
    let observed_reports = Rc::clone(&reports);
    let cancellation = Rc::new(CancellationToken::new());
    let serve_task = operations::spawn_task(server.serve_with_error_handler(
        |request| async move {
            match request.uri().path() {
                "/failed" => Response::new(Body::from_stream(futures::stream::iter([
                    Ok::<_, ResponseBodyFailure>(b"failed-prefix".to_vec()),
                    Err(ResponseBodyFailure),
                ]))),
                "/sibling-one" => Response::new(Body::from("first-sibling-complete")),
                "/sibling-two" => Response::new(Body::from("second-sibling-complete")),
                "/after-body-error" => Response::new(Body::from("connection-still-alive")),
                _ => unreachable!("unexpected request target"),
            }
        },
        Rc::clone(&cancellation),
        move |error| match error {
            ServeError::ResponseBody(HttpError::BodyStream { source }) => {
                observed_reports.borrow_mut().push(source.to_string());
            }
            error => panic!("unexpected server report: {error}"),
        },
    ));

    let peer_result = operations::timeout_at(Instant::now() + WAIT, async {
        while !peer_finished.load(Ordering::Acquire) {
            operations::sleep(Duration::from_millis(1)).await.unwrap();
        }
    })
    .await;
    cancellation.cancel();
    operations::timeout_at(Instant::now() + WAIT, serve_task)
        .await
        .expect("server did not stop after the body-failure isolation test")
        .unwrap()
        .unwrap();
    peer_result.expect("healthy HTTP/2 streams did not survive the body failure");

    let (failed_stream, first_sibling, second_sibling, after_stream, responses) =
        peer.join().unwrap();
    assert_eq!(responses[&failed_stream].status, Some(200));
    assert_eq!(responses[&failed_stream].body, b"failed-prefix");
    assert_eq!(
        responses[&failed_stream].reset,
        Some(H2ErrorCode::InternalError.as_u32())
    );
    assert_eq!(responses[&first_sibling].body, b"first-sibling-complete");
    assert_eq!(responses[&second_sibling].body, b"second-sibling-complete");
    assert_eq!(responses[&after_stream].body, b"connection-still-alive");
    assert_eq!(
        reports.borrow().as_slice(),
        ["intentional response body failure"]
    );
}

#[kimojio::test]
async fn panicking_streaming_http2_handler_fails_only_its_stream_and_connection_survives() {
    let server = Server::bind((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
    let address = server.local_addr();
    let peer_finished = Arc::new(AtomicBool::new(false));
    let finished = Arc::clone(&peer_finished);
    let peer = std::thread::spawn(move || {
        let mut client = H2Client::default();
        let mut outbound = client.connection_preface();
        let (panic_stream, panic_request) = open_request(&mut client, "/panic");
        outbound.extend_from_slice(&panic_request);
        let (sibling_stream, sibling_request) = open_request(&mut client, "/sibling");
        outbound.extend_from_slice(&sibling_request);

        let mut stream = TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        stream.set_write_timeout(Some(WAIT)).unwrap();
        stream.write_all(&outbound).unwrap();

        let mut pending = Vec::new();
        let mut responses = HashMap::new();
        read_complete_responses(
            &mut stream,
            &mut client,
            &mut pending,
            &mut responses,
            &[panic_stream, sibling_stream],
        );

        let (after_stream, after_request) = open_request(&mut client, "/after");
        stream.write_all(&after_request).unwrap();
        read_complete_responses(
            &mut stream,
            &mut client,
            &mut pending,
            &mut responses,
            &[after_stream],
        );
        finished.store(true, Ordering::Release);
        (panic_stream, sibling_stream, after_stream, responses)
    });

    let sibling_started = Rc::new(AsyncEvent::new());
    let trigger_panic = Rc::new(AsyncEvent::new());
    let cancellation = Rc::new(CancellationToken::new());
    let started = Rc::clone(&sibling_started);
    let trigger = Rc::clone(&trigger_panic);
    let reports = Rc::new(RefCell::new(Vec::new()));
    let observed_reports = Rc::clone(&reports);
    let serve_task = operations::spawn_task(server.serve_streaming_with_error_handler(
        move |request| {
            let target = request.uri().path().to_owned();
            let started = Rc::clone(&started);
            let trigger = Rc::clone(&trigger);
            async move {
                match target.as_str() {
                    "/panic" => {
                        started.wait().await.unwrap();
                        trigger.set();
                        panic!("isolated HTTP/2 handler panic");
                    }
                    "/sibling" => {
                        started.set();
                        trigger.wait().await.unwrap();
                        Response::new(Body::from("sibling-ok"))
                    }
                    "/after" => Response::new(Body::from("after-ok")),
                    _ => unreachable!("unexpected request target"),
                }
            }
        },
        Rc::clone(&cancellation),
        move |error| {
            assert!(
                matches!(error, ServeError::HandlerPanicked(_)),
                "unexpected server report: {error}"
            );
            observed_reports.borrow_mut().push(error.to_string());
        },
    ));

    operations::timeout_at(Instant::now() + WAIT, async {
        while !peer_finished.load(Ordering::Acquire) {
            operations::sleep(Duration::from_millis(1)).await.unwrap();
        }
    })
    .await
    .expect("peer did not receive all HTTP/2 responses");
    cancellation.cancel();
    operations::timeout_at(Instant::now() + WAIT, serve_task)
        .await
        .expect("server did not shut down after the isolated panic")
        .unwrap()
        .unwrap();

    let (panic_stream, sibling_stream, after_stream, responses) = peer.join().unwrap();
    assert_eq!(responses[&panic_stream].status, Some(500));
    assert!(responses[&panic_stream].body.is_empty());
    assert_eq!(responses[&sibling_stream].status, Some(200));
    assert_eq!(responses[&sibling_stream].body, b"sibling-ok");
    assert_eq!(responses[&after_stream].status, Some(200));
    assert_eq!(responses[&after_stream].body, b"after-ok");
    assert_eq!(reports.borrow().len(), 1);
    assert!(reports.borrow()[0].contains("isolated HTTP/2 handler panic"));
}

#[kimojio::test]
async fn trailer_rejection_and_callback_panic_reset_only_their_http2_streams() {
    let server = Server::bind((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
    let address = server.local_addr();
    let peer_finished = Arc::new(AtomicBool::new(false));
    let finished = Arc::clone(&peer_finished);
    let peer = std::thread::spawn(move || {
        let mut client = H2Client::default();
        let mut outbound = client.connection_preface();
        let (forbidden_stream, forbidden_request) =
            open_request(&mut client, "/forbidden-trailers");
        outbound.extend_from_slice(&forbidden_request);
        let (panic_stream, panic_request) = open_request(&mut client, "/panic-trailers");
        outbound.extend_from_slice(&panic_request);
        let (sibling_stream, sibling_request) = open_request(&mut client, "/sibling");
        outbound.extend_from_slice(&sibling_request);

        let mut stream = TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        stream.set_write_timeout(Some(WAIT)).unwrap();
        stream.write_all(&outbound).unwrap();

        let mut pending = Vec::new();
        let mut responses = HashMap::new();
        read_complete_responses(
            &mut stream,
            &mut client,
            &mut pending,
            &mut responses,
            &[forbidden_stream, panic_stream, sibling_stream],
        );

        let (after_stream, after_request) = open_request(&mut client, "/after");
        stream.write_all(&after_request).unwrap();
        read_complete_responses(
            &mut stream,
            &mut client,
            &mut pending,
            &mut responses,
            &[after_stream],
        );
        finished.store(true, Ordering::Release);
        (
            forbidden_stream,
            panic_stream,
            sibling_stream,
            after_stream,
            responses,
        )
    });

    let reports = Rc::new(RefCell::new((false, false)));
    let observed_reports = Rc::clone(&reports);
    let cancellation = Rc::new(CancellationToken::new());
    let serve_task = operations::spawn_task(server.serve_with_error_handler(
        |request| async move {
            match request.uri().path() {
                "/forbidden-trailers" => {
                    let mut trailers = HeaderMap::new();
                    trailers.insert("content-length", HeaderValue::from_static("1"));
                    Response::new(Body::from("forbidden-prefix").with_trailers(trailers))
                }
                "/panic-trailers" => Response::new(
                    Body::from_chunks(futures::stream::iter([b"panic-prefix".to_vec()]))
                        .with_trailers_fn(|| panic!("isolated trailer callback panic")),
                ),
                "/sibling" => Response::new(Body::from("sibling-ok")),
                "/after" => Response::new(Body::from("after-ok")),
                _ => unreachable!("unexpected request target"),
            }
        },
        Rc::clone(&cancellation),
        move |error| match error {
            ServeError::ResponseTrailers(HttpError::Protocol(error))
                if error.kind() == ProtocolErrorKind::InvalidHeader =>
            {
                observed_reports.borrow_mut().0 = true;
            }
            ServeError::HandlerPanicked(payload) => {
                let message = payload
                    .downcast_ref::<&'static str>()
                    .copied()
                    .or_else(|| payload.downcast_ref::<String>().map(String::as_str));
                assert_eq!(message, Some("isolated trailer callback panic"));
                observed_reports.borrow_mut().1 = true;
            }
            error => panic!("unexpected server report: {error}"),
        },
    ));

    operations::timeout_at(Instant::now() + WAIT, async {
        while !peer_finished.load(Ordering::Acquire) {
            operations::sleep(Duration::from_millis(1)).await.unwrap();
        }
    })
    .await
    .expect("healthy HTTP/2 streams did not survive trailer failures");
    cancellation.cancel();
    operations::timeout_at(Instant::now() + WAIT, serve_task)
        .await
        .expect("server did not stop after trailer-failure isolation test")
        .unwrap()
        .unwrap();

    let (forbidden_stream, panic_stream, sibling_stream, after_stream, responses) =
        peer.join().unwrap();
    assert_eq!(responses[&forbidden_stream].status, Some(200));
    assert_eq!(responses[&forbidden_stream].body, b"forbidden-prefix");
    assert_eq!(
        responses[&forbidden_stream].reset,
        Some(H2ErrorCode::InternalError.as_u32())
    );
    assert_eq!(responses[&panic_stream].status, Some(200));
    assert_eq!(responses[&panic_stream].body, b"panic-prefix");
    assert_eq!(
        responses[&panic_stream].reset,
        Some(H2ErrorCode::InternalError.as_u32())
    );
    assert_eq!(responses[&sibling_stream].body, b"sibling-ok");
    assert_eq!(responses[&after_stream].body, b"after-ok");
    assert_eq!(*reports.borrow(), (true, true));
}

#[kimojio::test]
async fn peer_reset_keeps_sibling_and_following_http2_streams_alive() {
    let server = Server::bind((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
    let address = server.local_addr();
    let peer_finished = Arc::new(AtomicBool::new(false));
    let finished = Arc::clone(&peer_finished);
    let peer = std::thread::spawn(move || {
        let mut client = H2Client::default();
        let mut outbound = client.connection_preface();
        let (reset_stream, reset_request) = open_request(&mut client, "/reset");
        outbound.extend_from_slice(&reset_request);
        let (sibling_stream, sibling_request) = open_request(&mut client, "/sibling");
        outbound.extend_from_slice(&sibling_request);
        client.close_stream(reset_stream);
        H2Frame {
            frame_type: H2FrameType::RstStream,
            flags: 0,
            stream_id: reset_stream,
            payload: H2ErrorCode::Cancel.as_u32().to_be_bytes().to_vec(),
        }
        .encode(&mut outbound);
        let (after_stream, after_request) = open_request(&mut client, "/after");
        outbound.extend_from_slice(&after_request);

        let mut stream = TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        stream.set_write_timeout(Some(WAIT)).unwrap();
        stream.write_all(&outbound).unwrap();

        let mut pending = Vec::new();
        let mut responses = HashMap::new();
        read_complete_responses(
            &mut stream,
            &mut client,
            &mut pending,
            &mut responses,
            &[sibling_stream, after_stream],
        );
        finished.store(true, Ordering::Release);
        (sibling_stream, after_stream, responses)
    });

    let reset_started = Rc::new(Cell::new(false));
    let observed_reset = Rc::clone(&reset_started);
    let hold_reset = Rc::new(AsyncEvent::new());
    let hold = Rc::clone(&hold_reset);
    let cancellation = Rc::new(CancellationToken::new());
    let serve_task = operations::spawn_task(server.serve_with_error_handler(
        move |request| {
            let target = request.uri().path().to_owned();
            let observed_reset = Rc::clone(&observed_reset);
            let hold = Rc::clone(&hold);
            async move {
                match target.as_str() {
                    "/reset" => {
                        observed_reset.set(true);
                        hold.wait().await.unwrap();
                        unreachable!("reset handler must be canceled by its peer")
                    }
                    "/sibling" => {
                        operations::sleep(Duration::from_millis(5)).await.unwrap();
                        Response::new(Body::from("sibling-ok"))
                    }
                    "/after" => Response::new(Body::from("after-ok")),
                    _ => unreachable!("unexpected request target"),
                }
            }
        },
        Rc::clone(&cancellation),
        |error| panic!("peer reset must not fail its connection: {error}"),
    ));

    operations::timeout_at(Instant::now() + WAIT, async {
        while !peer_finished.load(Ordering::Acquire) {
            operations::sleep(Duration::from_millis(1)).await.unwrap();
        }
    })
    .await
    .expect("peer did not receive the surviving HTTP/2 responses");
    cancellation.cancel();
    operations::timeout_at(Instant::now() + WAIT, serve_task)
        .await
        .expect("server did not shut down after the peer reset")
        .unwrap()
        .unwrap();

    let (sibling_stream, after_stream, responses) = peer.join().unwrap();
    assert!(reset_started.get());
    assert_eq!(responses[&sibling_stream].status, Some(200));
    assert_eq!(responses[&sibling_stream].body, b"sibling-ok");
    assert_eq!(responses[&after_stream].status, Some(200));
    assert_eq!(responses[&after_stream].body, b"after-ok");
}

fn open_request_with_body(client: &mut H2Client, target: &str) -> (u32, Vec<u8>) {
    let (stream_id, commit) = client
        .open_stream("POST", "http", "localhost", target, &[], false)
        .unwrap();
    (stream_id, take_client_block(client, commit))
}

fn data_frame(stream_id: u32, payload: &[u8], end_stream: bool) -> Vec<u8> {
    let mut wire = Vec::new();
    H2Frame {
        frame_type: H2FrameType::Data,
        flags: u8::from(end_stream),
        stream_id,
        payload: payload.to_vec(),
    }
    .encode(&mut wire);
    wire
}

/// SEC-007. A peer may burst body bytes at a stream whose consumer has paused,
/// because the advertised window is far larger than the one-slot handoff. That
/// must retire only the paused exchange: failing the connection would let a peer
/// destroy every other request multiplexed on it just by stalling one consumer
/// while a second stream keeps the pump running. The retired consumer must also
/// be told its body was cut short, since a dropped sender otherwise ends the
/// body stream exactly as a complete request does.
///
/// `/drain` is what keeps the pump running. Its consumer asks for a chunk that
/// never arrives, so the connection always has outstanding demand and never
/// parks waiting for it - which is precisely the condition under which the
/// burst at `/slow` reaches the full handoff.
#[kimojio::test]
async fn bursting_a_paused_streaming_consumer_spares_sibling_streams() {
    // `/drain` below parks forever by design, so shut it down promptly instead
    // of waiting out the default 30-second graceful drain.
    let server = Server::bind_with_config(
        (Ipv4Addr::LOCALHOST, 0).into(),
        ServerConfig::default().set_graceful_shutdown_timeout(Duration::from_millis(50)),
    )
    .await
    .unwrap();
    let address = server.local_addr();
    let peer_finished = Arc::new(AtomicBool::new(false));
    let finished = Arc::clone(&peer_finished);
    let peer_released = Arc::new(AtomicBool::new(false));
    let released = Arc::clone(&peer_released);
    let first_chunk_taken = Arc::new(AtomicBool::new(false));
    let took_first_chunk = Arc::clone(&first_chunk_taken);

    let peer = std::thread::spawn(move || {
        let mut client = H2Client::default();
        let mut outbound = client.connection_preface();
        let (slow, slow_head) = open_request_with_body(&mut client, "/slow");
        outbound.extend_from_slice(&slow_head);
        let (_, drain_head) = open_request_with_body(&mut client, "/drain");
        outbound.extend_from_slice(&drain_head);

        let mut stream = TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        stream.set_write_timeout(Some(WAIT)).unwrap();
        stream.write_all(&outbound).unwrap();

        stream
            .write_all(&data_frame(slow, &[b'a'; 1024], false))
            .unwrap();
        let deadline = Instant::now() + WAIT;
        while !first_chunk_taken.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "the consumer never took a chunk");
            std::thread::sleep(Duration::from_millis(1));
        }

        // Burst past the single-slot handoff now that nothing drains it. Each
        // frame stays inside the initial flow-control window so the peer never
        // blocks on its own accounting.
        for _ in 0..8 {
            stream
                .write_all(&data_frame(slow, &[b'z'; 1024], false))
                .unwrap();
        }

        // The connection must still carry new work after the burst, and reading
        // this response proves the pump consumed the burst ahead of it.
        let (after, after_head) = open_request(&mut client, "/after-burst");
        stream.write_all(&after_head).unwrap();
        let mut pending = Vec::new();
        let mut responses = HashMap::new();
        read_complete_responses(
            &mut stream,
            &mut client,
            &mut pending,
            &mut responses,
            &[after],
        );
        finished.store(true, Ordering::Release);
        // Hold the connection open: dropping it here would end the retired
        // consumer's exchange for an unrelated reason.
        let deadline = Instant::now() + WAIT;
        while !released.load(Ordering::Acquire) {
            assert!(
                Instant::now() < deadline,
                "the test never released the peer"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        (after, responses)
    });

    let cancellation = Rc::new(CancellationToken::new());
    let resume = Rc::new(AsyncEvent::new());
    let drained = Rc::new(AsyncEvent::new());
    let truncation = Rc::new(Cell::new(None));
    let resumed = Rc::clone(&resume);
    let reported = Rc::clone(&drained);
    let observed = Rc::clone(&truncation);
    let serve_task = operations::spawn_task(server.serve_streaming(
        move |mut request| {
            let resumed = Rc::clone(&resumed);
            let reported = Rc::clone(&reported);
            let observed = Rc::clone(&observed);
            let took_first_chunk = Arc::clone(&took_first_chunk);
            async move {
                match request.uri().path() {
                    "/slow" => {
                        request.body_mut().next_chunk().await.unwrap().unwrap();
                        took_first_chunk.store(true, Ordering::Release);
                        resumed.wait().await.unwrap();
                        let truncated = loop {
                            match request.body_mut().next_chunk().await {
                                Ok(Some(_)) => {}
                                Ok(None) => break false,
                                Err(_) => break true,
                            }
                        };
                        observed.set(Some(truncated));
                        reported.set();
                        Response::new(Body::empty())
                    }
                    // Hold demand open so the pump never parks for want of it.
                    "/drain" => {
                        request.body_mut().next_chunk().await.unwrap();
                        unreachable!("no body ever arrives for the draining handler")
                    }
                    _ => Response::new(Body::from("after-ok")),
                }
            }
        },
        Rc::clone(&cancellation),
    ));

    let served_after_burst = operations::timeout_at(Instant::now() + WAIT, async {
        while !peer_finished.load(Ordering::Acquire) {
            operations::sleep(Duration::from_millis(1)).await.unwrap();
        }
    })
    .await;

    resume.set();
    let drained_body = operations::timeout_at(Instant::now() + WAIT, drained.wait()).await;
    peer_released.store(true, Ordering::Release);
    cancellation.cancel();
    let _ = operations::timeout_at(Instant::now() + WAIT, serve_task).await;

    // Assert only once the server and peer are released, so a regression fails
    // the test instead of stranding this thread's tasks.
    served_after_burst.expect("a burst at one paused stream took down its siblings");
    drained_body
        .expect("the retired consumer never finished its body")
        .unwrap();
    assert_eq!(
        truncation.get(),
        Some(true),
        "a truncated request body ended as though it were complete"
    );

    let (after, responses) = peer.join().expect("peer thread panicked");
    assert_eq!(responses[&after].status, Some(200));
    assert_eq!(responses[&after].body, b"after-ok");
}
