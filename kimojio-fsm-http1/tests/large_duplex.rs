mod support;

use kimojio_fsm_http1::*;
use support::*;

const CHUNK: usize = 16 * 1024;
const UPLOAD: usize = 8 * 1024 * 1024;
const PREFIX: &[u8] = b"response before upload completion\0\xff";
const TAIL: &[u8] = b"response after exact full upload";

fn payload_byte(offset: usize) -> u8 {
    let mixed = (offset as u32).wrapping_mul(0x9e37_79b9);
    (mixed ^ (mixed >> 13) ^ (mixed >> 23)) as u8
}

#[derive(Default)]
struct Pipe {
    read: Option<ReadOp<B>>,
    write: Option<WriteOp<B>>,
    reads: usize,
    writes: usize,
    completed_reads: usize,
    completed_writes: usize,
}

impl Pipe {
    fn read(&mut self, mut op: ReadOp<B>) {
        assert!(op.bytes_mut().len() <= CHUNK);
        assert!(self.read.replace(op).is_none());
        self.reads += 1;
    }

    fn write(&mut self, op: WriteOp<B>) {
        assert!(self.write.replace(op).is_none());
        self.writes += 1;
    }

    fn transfer(&mut self) -> Option<(WriteCompletion<B>, ReadCompletion<B>)> {
        if self.read.is_none() || self.write.is_none() {
            return None;
        }
        let mut read = self.read.take().unwrap();
        let write = self.write.take().unwrap();
        let bytes: Vec<_> = write
            .slices()
            .into_iter()
            .flatten()
            .copied()
            .take(CHUNK.min(read.bytes_mut().len()))
            .collect();
        assert!(!bytes.is_empty());
        self.completed_reads += 1;
        self.completed_writes += 1;
        Some((write.complete(Ok(bytes.len())), fill(read, &bytes)))
    }

    fn assert_settled(&self) {
        assert!(self.read.is_none() && self.write.is_none());
        assert_eq!(self.reads, self.completed_reads);
        assert_eq!(self.writes, self.completed_writes);
        assert!(self.reads > 0 && self.writes > 0);
    }
}

struct Receipt {
    id: BodyId,
    address: usize,
    len: usize,
}

impl Receipt {
    fn check(self, sent: BodySent<B>, exchange: ExchangeId) {
        assert_eq!(sent.exchange, exchange);
        assert_eq!(sent.id, self.id);
        assert_eq!(sent.buffer.as_ptr() as usize, self.address);
        assert_eq!(sent.buffer.len(), self.len);
        assert_eq!(sent.accepted, self.len);
        assert_eq!(sent.acceptance, Acceptance::Exact);
        assert_eq!(sent.result, Ok(()));
    }
}

fn large_upload_with_early_response(chunked: bool) {
    let config = Config {
        max_buffer_bytes: CHUNK,
        max_body_bytes: UPLOAD as u64,
        ..config()
    };
    let mut client = Client::new(
        ConnectionId {
            slot: 1,
            generation: 1,
        },
        config.clone(),
        vec![0; CHUNK],
        Tick(0),
    )
    .unwrap();
    let mut server = Server::new(
        ConnectionId {
            slot: 2,
            generation: 1,
        },
        config,
        vec![0; CHUNK],
        Tick(0),
    )
    .unwrap();
    let request = client
        .request(get(
            "POST",
            if chunked {
                BodyLength::Streaming
            } else {
                BodyLength::Known(UPLOAD as u64)
            },
            false,
            &[Header {
                name: "host",
                value: b"a",
            }],
        ))
        .unwrap();
    let mut upload_pipe = Pipe::default();
    let mut response_pipe = Pipe::default();
    let mut server_exchange = None;
    let mut client_demand = None;
    let mut server_demand = None;
    let mut client_receipt: Option<Receipt> = None;
    let mut server_receipt: Option<Receipt> = None;
    let mut upload_sent = 0;
    let mut upload_received = 0;
    let mut upload_receipts = 0;
    let mut response_receipts = 0;
    let mut request_leases = 0;
    let mut response_leases = 0;
    let mut response_head_seen = false;
    let mut prefix_seen = false;
    let mut request_done = false;
    let mut response_done = false;
    let mut request_trailers_seen = false;
    let mut response_trailers_seen = false;
    let mut response_parts = 0;
    let mut response_bytes = Vec::new();
    let mut finished = [false; 2];
    let mut closed = [false; 2];

    for _ in 0..50_000 {
        if let Some(event) = client.next(&mut Capture) {
            match event {
                Event::Read(op) => response_pipe.read(op),
                Event::Write(op) => upload_pipe.write(op),
                Event::Demand(id, capacity) => {
                    assert_eq!(id, request);
                    assert!(client_demand.replace(capacity).is_none());
                }
                Event::Response(id, 200, false) => {
                    assert_eq!(id, request);
                    assert!(!response_head_seen);
                    assert!(upload_sent <= CHUNK && upload_received < UPLOAD);
                    response_head_seen = true;
                    client.grant_body_credit(id, CHUNK).unwrap();
                }
                Event::Body(op) => {
                    assert!(response_head_seen);
                    let n = op.bytes().len();
                    response_bytes.extend_from_slice(op.bytes());
                    if !prefix_seen && response_bytes.len() >= PREFIX.len() {
                        assert_eq!(response_bytes, PREFIX);
                        assert_eq!(upload_sent, CHUNK);
                        assert!(upload_received < UPLOAD);
                        prefix_seen = true;
                    }
                    client.release_body(op.release(n)).unwrap();
                    client.grant_body_credit(request, n).unwrap();
                    response_leases += 1;
                }
                Event::Sent(sent) => {
                    client_receipt.take().unwrap().check(sent, request);
                    upload_receipts += 1;
                }
                Event::Trailers(headers) => {
                    assert_eq!(headers, [("x-end".into(), b"done".to_vec())]);
                    assert!(!response_trailers_seen);
                    response_trailers_seen = true;
                }
                Event::Incoming(id) => {
                    assert_eq!(id, request);
                    assert!(request_done && response_trailers_seen);
                    assert!(!response_done);
                    response_done = true;
                }
                Event::Finished(result) => {
                    assert_eq!(result.result, Ok(()));
                    assert!(!result.reusable && !finished[0]);
                    finished[0] = true;
                }
                Event::Close(op) => client.complete_close(op.complete(Ok(()))).unwrap(),
                Event::Closed(result) => {
                    assert_eq!(result, Ok(()));
                    closed[0] = true;
                }
                Event::Deadline(_) => {}
                other => panic!("client {other:?}"),
            }
        }
        if let Some(event) = server.next(&mut Capture) {
            match event {
                Event::Read(op) => upload_pipe.read(op),
                Event::Write(op) => response_pipe.write(op),
                Event::Request(id, _) => {
                    assert!(server_exchange.replace(id).is_none());
                    server.grant_body_credit(id, CHUNK).unwrap();
                    server.respond(id, response(BodyLength::Streaming)).unwrap();
                }
                Event::Demand(id, capacity) => {
                    assert_eq!(Some(id), server_exchange);
                    assert!(server_demand.replace(capacity).is_none());
                }
                Event::Body(op) => {
                    let n = op.bytes().len();
                    assert!(upload_received + n <= UPLOAD);
                    for (offset, &byte) in op.bytes().iter().enumerate() {
                        assert_eq!(byte, payload_byte(upload_received + offset));
                    }
                    upload_received += n;
                    server.release_body(op.release(n)).unwrap();
                    server
                        .grant_body_credit(server_exchange.unwrap(), n)
                        .unwrap();
                    request_leases += 1;
                }
                Event::Sent(sent) => {
                    server_receipt
                        .take()
                        .unwrap()
                        .check(sent, server_exchange.unwrap());
                    response_receipts += 1;
                }
                Event::Trailers(headers) => {
                    assert!(chunked && !request_trailers_seen);
                    assert_eq!(headers, [("upload".into(), b"complete".to_vec())]);
                    request_trailers_seen = true;
                }
                Event::Incoming(id) => {
                    assert_eq!(Some(id), server_exchange);
                    assert_eq!(upload_received, UPLOAD);
                    assert_eq!(request_trailers_seen, chunked);
                    assert!(prefix_seen && !request_done);
                    request_done = true;
                }
                Event::Finished(result) => {
                    assert_eq!(result.result, Ok(()));
                    assert!(!result.reusable && !finished[1]);
                    finished[1] = true;
                }
                Event::Close(op) => server.complete_close(op.complete(Ok(()))).unwrap(),
                Event::Closed(result) => {
                    assert_eq!(result, Ok(()));
                    closed[1] = true;
                }
                Event::Deadline(_) => {}
                other => panic!("server {other:?}"),
            }
        }

        // Only the first payload can precede the observed response prefix.
        if client_demand.is_some() && (upload_sent == 0 || prefix_seen) {
            let capacity = client_demand.take().unwrap();
            if upload_sent < UPLOAD {
                let len = CHUNK.min(capacity).min(UPLOAD - upload_sent);
                assert_eq!(len, CHUNK);
                assert!(client_receipt.is_none());
                let buffer: Vec<_> = (upload_sent..upload_sent + len).map(payload_byte).collect();
                let address = buffer.as_ptr() as usize;
                upload_sent += len;
                let id = client
                    .send_body(SendBody {
                        exchange: request,
                        buffer,
                        range: 0..len,
                        end: !chunked && upload_sent == UPLOAD,
                    })
                    .unwrap();
                client_receipt = Some(Receipt { id, address, len });
            } else {
                assert!(chunked);
                client
                    .finish_body(
                        request,
                        &[Header {
                            name: "upload",
                            value: b"complete",
                        }],
                    )
                    .unwrap();
            }
        }
        // Neither the tail nor response termination exists before full upload.
        if server_demand.is_some() && (response_parts == 0 || request_done) {
            let capacity = server_demand.take().unwrap();
            let exchange = server_exchange.unwrap();
            if response_parts < 2 {
                let buffer = if response_parts == 0 { PREFIX } else { TAIL }.to_vec();
                let len = buffer.len();
                let address = buffer.as_ptr() as usize;
                assert!(len <= capacity && server_receipt.is_none());
                let id = server
                    .send_body(SendBody {
                        exchange,
                        buffer,
                        range: 0..len,
                        end: false,
                    })
                    .unwrap();
                server_receipt = Some(Receipt { id, address, len });
                response_parts += 1;
            } else {
                server
                    .finish_body(
                        exchange,
                        &[Header {
                            name: "x-end",
                            value: b"done",
                        }],
                    )
                    .unwrap();
            }
        }
        if let Some((write, read)) = upload_pipe.transfer() {
            client.complete_write(write).unwrap();
            server.complete_read(read).unwrap();
        }
        if let Some((write, read)) = response_pipe.transfer() {
            server.complete_write(write).unwrap();
            client.complete_read(read).unwrap();
        }
        if closed == [true; 2] {
            break;
        }
    }
    assert_eq!(
        closed, [true; 2],
        "stalled: chunked={chunked}, sent={upload_sent}, received={upload_received}, prefix={prefix_seen}"
    );
    assert_eq!(finished, [true; 2]);
    assert!(response_head_seen && prefix_seen && request_done && response_done);
    assert_eq!(upload_sent, UPLOAD);
    assert_eq!(upload_received, UPLOAD);
    assert_eq!(upload_receipts, UPLOAD / CHUNK);
    assert_eq!(response_receipts, 2);
    assert!(request_leases >= UPLOAD / CHUNK && response_leases >= 2);
    assert_eq!(response_bytes, [PREFIX, TAIL].concat());
    assert!(client_receipt.is_none() && server_receipt.is_none());
    assert!(client_demand.is_none() && server_demand.is_none());
    upload_pipe.assert_settled();
    response_pipe.assert_settled();
    assert!(client.next(&mut Capture).is_none());
    assert!(server.next(&mut Capture).is_none());
}

#[test]
fn fixed_eight_mib_upload_continues_after_early_response_head_and_prefix() {
    large_upload_with_early_response(false);
}

#[test]
fn chunked_eight_mib_upload_continues_after_early_response_head_and_prefix() {
    large_upload_with_early_response(true);
}
