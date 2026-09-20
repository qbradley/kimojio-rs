mod support;
use kimojio_fsm_http1::*;
use std::collections::VecDeque;
use support::*;

fn receive(op: ReadOp<B>, wire: &mut VecDeque<u8>, fragment: usize) -> ReadCompletion<B> {
    let n = fragment.min(wire.len());
    let bytes: Vec<_> = wire.drain(..n).collect();
    fill(op, &bytes)
}

#[test]
fn successful_response_streams_while_request_source_waits_for_response_head() {
    for (fragment, expect_continue) in [1, 2, 7, 1024]
        .into_iter()
        .flat_map(|fragment| [false, true].map(move |expect| (fragment, expect)))
    {
        let mut client = client(config());
        let mut server = server(config());
        let mut client_read = None;
        let mut server_read = None;
        for _ in 0..2 {
            let request = client
                .request(get(
                    "POST",
                    BodyLength::Streaming,
                    expect_continue,
                    &[Header {
                        name: "host",
                        value: b"a",
                    }],
                ))
                .unwrap();
            let mut to_client = VecDeque::new();
            let mut to_server = VecDeque::new();
            let mut client_demand = false;
            let mut server_demand = None;
            let mut held_request_body = None;
            let mut response_seen = false;
            let mut request_incoming_done = false;
            let mut request_parts_sent = 0;
            let mut response_body = Vec::new();
            let mut client_finished = false;
            let mut server_finished = false;
            let mut response_source_finished = false;

            for _ in 0..20_000 {
                if let Some(event) = client.next(&mut Capture) {
                    match event {
                        Event::Write(op) => {
                            let bytes = op.slices().concat();
                            let n = fragment.min(bytes.len());
                            to_server.extend(&bytes[..n]);
                            client.complete_write(op.complete(Ok(n))).unwrap();
                        }
                        Event::Read(op) => assert!(client_read.replace(op).is_none()),
                        Event::Demand(id, _) => {
                            assert_eq!(id, request);
                            client_demand = true;
                        }
                        Event::Response(id, 200, false) => {
                            assert_eq!(id, request);
                            assert_eq!(
                                request_parts_sent, 0,
                                "producer deliberately waits for response headers"
                            );
                            response_seen = true;
                            client.grant_body_credit(id, 1024).unwrap();
                        }
                        Event::Response(_, 100, true) => {}
                        Event::Body(op) => {
                            let n = op.bytes().len();
                            response_body.extend_from_slice(op.bytes());
                            client.release_body(op.release(n)).unwrap();
                            client.grant_body_credit(request, n).unwrap();
                        }
                        Event::Sent(sent) => assert_eq!(
                            sent.result,
                            Ok(()),
                            "successful duplex response must not reject upload"
                        ),
                        Event::Finished(result) => {
                            assert_eq!(result.result, Ok(()));
                            assert!(result.reusable);
                            client_finished = true;
                        }
                        Event::Close(op) => client.complete_close(op.complete(Ok(()))).unwrap(),
                        Event::Closed(result) => assert_eq!(result, Ok(())),
                        Event::Incoming(_) | Event::Trailers(_) | Event::Deadline(_) => {}
                        other => panic!("client {other:?}"),
                    }
                }
                if let Some(event) = server.next(&mut Capture) {
                    match event {
                        Event::Read(op) => assert!(server_read.replace(op).is_none()),
                        Event::Request(id, _) => {
                            server.grant_body_credit(id, 1024).unwrap();
                            server
                                .respond_duplex(id, response(BodyLength::Streaming))
                                .unwrap();
                        }
                        Event::Write(op) => {
                            let bytes = op.slices().concat();
                            let n = fragment.min(bytes.len());
                            to_client.extend(&bytes[..n]);
                            server.complete_write(op.complete(Ok(n))).unwrap();
                        }
                        Event::Demand(id, _) => {
                            assert!(server_demand.replace(id).is_none());
                        }
                        Event::Body(op) => assert!(held_request_body.replace(op).is_none()),
                        Event::Incoming(_) => request_incoming_done = true,
                        Event::Sent(sent) => assert_eq!(sent.result, Ok(())),
                        Event::Finished(result) => {
                            assert_eq!(result.result, Ok(()));
                            assert!(result.reusable);
                            server_finished = true;
                        }
                        Event::Close(op) => server.complete_close(op.complete(Ok(()))).unwrap(),
                        Event::Closed(result) => assert_eq!(result, Ok(())),
                        Event::Trailers(_) | Event::Deadline(_) => {}
                        other => panic!("server {other:?}"),
                    }
                }
                if response_seen && client_demand && request_parts_sent < 2 {
                    client_demand = false;
                    let bytes = if request_parts_sent == 0 {
                        b"one"
                    } else {
                        b"two"
                    };
                    request_parts_sent += 1;
                    client
                        .send_body(SendBody {
                            exchange: request,
                            buffer: bytes.to_vec(),
                            range: 0..3,
                            end: request_parts_sent == 2,
                        })
                        .unwrap();
                }
                if server_demand.is_some() && held_request_body.is_some() {
                    let id = server_demand.take().unwrap();
                    let op = held_request_body.take().unwrap();
                    let bytes = op.bytes().to_vec();
                    let n = bytes.len();
                    server.release_body(op.release(n)).unwrap();
                    server.grant_body_credit(id, n).unwrap();
                    server
                        .send_body(SendBody {
                            exchange: id,
                            buffer: bytes,
                            range: 0..n,
                            end: false,
                        })
                        .unwrap();
                }
                if request_incoming_done
                    && !response_source_finished
                    && held_request_body.is_none()
                    && let Some(id) = server_demand.take()
                {
                    response_source_finished = true;
                    server.finish_body(id, &[]).unwrap();
                }
                if !to_server.is_empty()
                    && let Some(op) = server_read.take()
                {
                    server
                        .complete_read(receive(op, &mut to_server, fragment))
                        .unwrap();
                }
                if !to_client.is_empty()
                    && let Some(op) = client_read.take()
                {
                    client
                        .complete_read(receive(op, &mut to_client, fragment))
                        .unwrap();
                }
                if client_finished && server_finished {
                    break;
                }
            }
            assert!(
                client_finished && server_finished,
                "duplex exchange stalled at fragment={fragment}"
            );
            assert!(request_incoming_done);
            assert_eq!(request_parts_sent, 2);
            assert_eq!(response_body, b"onetwo");
            assert!(held_request_body.is_none());
            assert!(to_client.is_empty() && to_server.is_empty());
        }
    }
}
