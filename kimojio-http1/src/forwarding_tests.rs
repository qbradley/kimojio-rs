use super::*;

#[path = "slot_tests.rs"]
mod slot_tests;

type ReceiveMachine = core::Server<Vec<u8>, OutgoingData>;

fn lease() -> (
    ReceiveMachine,
    BodyChunk,
    Receiver<core::BodyCompletion<Vec<u8>>>,
) {
    let config = core::Config::default();
    let buffer = vec![0; config.max_buffer_bytes];
    let allocation = buffer.as_ptr() as usize;
    let retained_capacity = buffer.capacity();
    let mut server = core::Server::with_output_type(
        core::ConnectionId {
            slot: 100,
            generation: 1,
        },
        config,
        buffer,
        core::Tick(0),
    )
    .unwrap();
    let request = b"POST / HTTP/1.1\r\nHost: test\r\nContent-Length: 7\r\n\r\npayload";
    for _ in 0..32 {
        match server.next(&mut Ports) {
            Some(Event::Read(mut op)) => {
                op.bytes_mut()[..request.len()].copy_from_slice(request);
                server
                    .complete_read(op.complete(Ok(request.len())))
                    .unwrap();
            }
            Some(Event::Request(id, request)) => {
                request.unwrap();
                server.grant_body_credit(id, 7).unwrap();
            }
            Some(Event::Body(op)) => {
                assert_eq!(op.bytes(), b"payload");
                assert!(op.bytes().as_ptr() as usize > allocation);
                assert!((op.bytes().as_ptr() as usize) + 7 <= allocation + retained_capacity);
                let (release, returned) = async_channel();
                return (
                    server,
                    BodyChunk {
                        op: Some(op),
                        release,
                        retained_capacity,
                    },
                    returned,
                );
            }
            Some(Event::Deadline(_)) | None => {}
            _ => panic!("unexpected receive event"),
        }
    }
    panic!("no receive lease");
}

fn destination() -> (core::Client<Vec<u8>, OutgoingData>, core::ExchangeId) {
    let config = core::Config::default();
    let buffer = vec![0; config.max_buffer_bytes];
    let mut client = core::Client::with_output_type(
        core::ConnectionId {
            slot: 101,
            generation: 1,
        },
        config,
        buffer,
        core::Tick(0),
    )
    .unwrap();
    let id = client
        .request(core::Request {
            head: core::RequestHead {
                method: "POST",
                target: "/",
                version: core::Version::Http11,
                headers: &[core::Header {
                    name: "host",
                    value: b"test",
                }],
            },
            body: core::BodyLength::Known(7),
            expect_continue: false,
        })
        .unwrap();
    for _ in 0..32 {
        match client.next(&mut Ports) {
            Some(Event::Write(op)) => {
                let n = op.slices().iter().map(|bytes| bytes.len()).sum();
                client.complete_write(op.complete(Ok(n))).unwrap();
            }
            Some(Event::SendReady(_, _)) => return (client, id),
            Some(Event::Read(_) | Event::Deadline(_)) | None => {}
            _ => panic!("unexpected destination event"),
        }
    }
    panic!("no destination capacity");
}

fn assert_not_returned(returned: &Receiver<core::BodyCompletion<Vec<u8>>>) {
    assert!(
        returned.try_recv().unwrap().is_none(),
        "lease returned before receipt"
    );
}

fn assert_returned(
    mut server: ReceiveMachine,
    returned: Receiver<core::BodyCompletion<Vec<u8>>>,
    pointer: *const u8,
) {
    let (op, consumed) = returned
        .try_recv()
        .unwrap()
        .expect("returned once")
        .into_parts();
    assert_eq!(op.bytes().as_ptr(), pointer);
    assert_eq!(op.bytes(), b"payload");
    assert_eq!(consumed, 7);
    server.release_body(op.release(consumed)).unwrap();
    assert!(returned.try_recv().ok().flatten().is_none());
}

#[test]
fn forwarded_lease_survives_partial_writes_and_cancellation_until_receipt() {
    for result in [
        Ok(5),
        Err(core::IoError {
            kind: core::IoErrorKind::UnknownProgress,
            code: None,
        }),
        Err(core::IoError {
            kind: core::IoErrorKind::CancelledUnknownProgress,
            code: None,
        }),
    ] {
        let (server, chunk, returned) = lease();
        let pointer = chunk.as_ptr();
        let (mut client, id) = destination();
        client
            .send_body(core::SendBody {
                exchange: id,
                buffer: OutgoingData::Forward(chunk),
                range: 0..7,
                end: false,
            })
            .unwrap();
        let mut partial = false;
        let mut source_finished = false;
        let mut receipt = None;
        let mut writes = 0;
        for _ in 0..64 {
            assert_not_returned(&returned);
            match client.next(&mut Ports) {
                Some(Event::Write(op)) => {
                    writes += 1;
                    let payload = op.slices()[1];
                    assert_eq!(
                        payload,
                        if partial {
                            &b"yload"[..]
                        } else {
                            &b"payload"[..]
                        }
                    );
                    assert_eq!(
                        payload.as_ptr() as usize,
                        pointer as usize + if partial { 2 } else { 0 }
                    );
                    if !partial {
                        client.complete_write(op.complete(Ok(2))).unwrap();
                        partial = true;
                    } else {
                        if result.is_err_and(|error| {
                            error.kind == core::IoErrorKind::CancelledUnknownProgress
                        }) {
                            client.cancel_exchange(id).unwrap();
                            let mut cancelled_original = false;
                            for _ in 0..32 {
                                match client.next(&mut Ports) {
                                    Some(Event::Cancel(cancel)) => {
                                        cancelled_original |= cancel.target == op.id()
                                    }
                                    Some(Event::SourceFinished(_)) => source_finished = true,
                                    Some(Event::Deadline(_)) | None => {}
                                    Some(Event::BodySent(_)) => {
                                        panic!("receipt before original completion")
                                    }
                                    _ => panic!("unexpected cancellation event"),
                                }
                                assert_not_returned(&returned);
                                if cancelled_original {
                                    break;
                                }
                            }
                            assert!(cancelled_original);
                        }
                        client.complete_write(op.complete(result)).unwrap();
                    }
                }
                Some(Event::SourceFinished(_)) => source_finished = true,
                Some(Event::BodySent(sent)) => {
                    receipt = Some(sent);
                    break;
                }
                Some(Event::Deadline(_) | Event::Cancel(_)) | None => {}
                _ => panic!("unexpected outgoing event"),
            }
        }
        let receipt = receipt.expect("outgoing receipt");
        assert_eq!(writes, 2);
        assert!(partial && source_finished);
        assert_eq!(receipt.buffer.as_ref().as_ptr(), pointer);
        assert_eq!(receipt.accepted, if result.is_ok() { 7 } else { 2 });
        assert_eq!(
            receipt.acceptance,
            if result.is_ok() {
                core::Acceptance::Exact
            } else {
                core::Acceptance::LowerBound
            }
        );
        assert_eq!(receipt.result.is_ok(), result.is_ok());
        assert_not_returned(&returned);
        drop(receipt);
        assert_returned(server, returned, pointer);
    }
}

#[test]
fn rejected_forwarded_command_returns_the_original_lease_once() {
    let (server, chunk, returned) = lease();
    let pointer = chunk.as_ptr();
    let (mut client, id) = destination();
    client.cancel_exchange(id).unwrap();
    let rejection = client
        .send_body(core::SendBody {
            exchange: id,
            buffer: OutgoingData::Forward(chunk),
            range: 0..7,
            end: false,
        })
        .unwrap_err();
    assert_eq!(rejection.reason, core::RejectReason::InvalidState);
    assert_not_returned(&returned);
    assert!(!source_admitted(Err(rejection)).unwrap());
    assert_returned(server, returned, pointer);
}

#[test]
fn dropping_unadmitted_sources_and_frames_returns_the_lease_once() {
    for poll in [false, true] {
        let (server, chunk, returned) = lease();
        let pointer = chunk.as_ptr();
        assert!(chunk.retained_capacity() > chunk.len());
        let mut source = OutgoingBody::from_stream(
            Some(7),
            futures::stream::iter([Ok(OutgoingFrame::Forward(chunk))]),
        );
        assert_not_returned(&returned);
        if poll {
            let frame = futures::executor::block_on(source.source.next())
                .unwrap()
                .unwrap();
            drop(source);
            assert_not_returned(&returned);
            drop(frame);
        } else {
            drop(source);
        }
        assert_returned(server, returned, pointer);
    }
    let (server, chunk, returned) = lease();
    drop(returned);
    drop(server);
    drop(chunk);
}

struct DelayedWriter {
    started: Option<SenderOneshot<()>>,
    cancelled: Option<SenderOneshot<()>>,
    settle: Option<futures::channel::oneshot::Receiver<()>>,
    closed: Rc<Cell<bool>>,
    success: bool,
}

impl kimojio::AsyncStreamWrite for DelayedWriter {
    async fn write(&mut self, bytes: &[u8], _: Option<Instant>) -> Result<(), kimojio::Errno> {
        if bytes.is_empty() {
            return Ok(());
        }
        assert_eq!(bytes, b"payload");
        self.started.take().unwrap().send(()).unwrap();
        assert_eq!(
            operations::sleep(Duration::from_secs(10)).await,
            Err(kimojio::Errno::CANCELED)
        );
        self.cancelled.take().unwrap().send(()).unwrap();
        self.settle.take().unwrap().await.unwrap();
        assert_eq!(bytes, b"payload");
        if self.success {
            Ok(())
        } else {
            Err(kimojio::Errno::CANCELED)
        }
    }

    async fn shutdown(&mut self) -> Result<(), kimojio::Errno> {
        Ok(())
    }
    async fn close(&mut self) -> Result<(), kimojio::Errno> {
        assert!(!self.closed.replace(true));
        Ok(())
    }
}

#[kimojio::test]
async fn native_cancellation_keeps_forwarded_lease_until_original_settlement_and_receipt() {
    for success in [false, true] {
        native_cancellation_case(success).await;
    }
}

async fn native_cancellation_case(success: bool) {
    let (server, chunk, returned) = lease();
    let pointer = chunk.as_ptr();
    let (mut client, id) = destination();
    client
        .send_body(core::SendBody {
            exchange: id,
            buffer: OutgoingData::Forward(chunk),
            range: 0..7,
            end: false,
        })
        .unwrap();
    let mut write = None;
    for _ in 0..32 {
        match client.next(&mut Ports) {
            Some(Event::Write(op)) => {
                write = Some(op);
                break;
            }
            Some(Event::SourceFinished(_) | Event::Deadline(_)) | None => {}
            _ => panic!("unexpected event"),
        }
    }
    let (send, requests) = async_channel();
    let (complete, completions) = async_channel();
    let (started, start) = oneshot();
    let (cancelled, cancel_done) = oneshot();
    let (settle, settled) = futures::channel::oneshot::channel();
    let cancel = Rc::new(CancellationToken::new());
    let closed = Rc::new(Cell::new(false));
    let worker = io::write_worker(
        DelayedWriter {
            started: Some(started),
            cancelled: Some(cancelled),
            settle: Some(settled),
            closed: closed.clone(),
            success,
        },
        requests,
        complete,
    );
    send.try_send(WriteAction::Write(Pending {
        op: write.unwrap(),
        cancel: cancel.clone(),
    }))
    .ok()
    .unwrap();
    let app = async {
        start.recv().await.unwrap();
        cancel.cancel();
        cancel_done.recv().await.unwrap();
        assert_not_returned(&returned);
        assert!(!closed.get());
        settle.send(()).unwrap();
        let WriteResult::Write(completion) = completions.recv().await.unwrap() else {
            panic!("missing write completion");
        };
        assert_not_returned(&returned);
        client.complete_write(completion).unwrap();
        assert_not_returned(&returned);
        let mut received = false;
        for _ in 0..32 {
            match client.next(&mut Ports) {
                Some(Event::BodySent(receipt)) => {
                    assert_eq!(
                        receipt.acceptance,
                        if success {
                            core::Acceptance::Exact
                        } else {
                            core::Acceptance::LowerBound
                        }
                    );
                    assert_eq!(receipt.accepted, if success { 7 } else { 0 });
                    assert_eq!(receipt.result.is_ok(), success);
                    assert_not_returned(&returned);
                    drop(receipt);
                    received = true;
                    break;
                }
                Some(Event::SourceFinished(_) | Event::Deadline(_) | Event::Cancel(_)) | None => {}
                _ => panic!("unexpected receipt event"),
            }
        }
        assert!(received);
        assert_returned(server, returned, pointer);
        drop(send);
    };
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
        futures::join!(app, worker);
    })
    .await
    .unwrap();
    assert!(closed.get());
}

#[kimojio::test]
async fn raw_native_worker_preserves_forwarded_lease_and_exact_cancel_or_success_receipt() {
    use crate::transport::{NativeTransport, Transport};

    for success in [false, true] {
        let (server, chunk, returned) = lease();
        let pointer = chunk.as_ptr();
        let (mut client, id) = destination();
        client
            .send_body(core::SendBody {
                exchange: id,
                buffer: OutgoingData::Forward(chunk),
                range: 0..7,
                end: false,
            })
            .unwrap();
        let write = (0..32)
            .find_map(|_| match client.next(&mut Ports) {
                Some(Event::Write(op)) => Some(op),
                Some(Event::SourceFinished(_) | Event::Deadline(_)) | None => None,
                _ => panic!("unexpected write event"),
            })
            .expect("missing forwarded write");
        assert_eq!(write.slices()[1].as_ptr(), pointer);
        let (fd, peer) = kimojio::pipe::bipipe();
        if !success {
            rustix::net::sockopt::set_socket_send_buffer_size(&fd, 4096).unwrap();
            loop {
                match rustix::net::send(&fd, &[0x33; 4096], rustix::net::SendFlags::DONTWAIT) {
                    Ok(count) => assert!(count > 0),
                    Err(kimojio::Errno::AGAIN) => break,
                    result => panic!("socket fill failed: {result:?}"),
                }
            }
        }
        let (reader, writer) = NativeTransport(fd).split().await.unwrap();
        drop(reader);
        let (send, requests) = async_channel();
        let (complete, completions) = async_channel();
        let cancel = Rc::new(CancellationToken::new());
        send.try_send(WriteAction::Write(Pending {
            op: write,
            cancel: cancel.clone(),
        }))
        .ok()
        .unwrap();
        let mut worker = Box::pin(io::write_worker(writer, requests, complete));
        assert!(futures::poll!(worker.as_mut()).is_pending());
        operations::yield_io().await;
        if success {
            let mut bytes = [0; 16];
            assert_eq!(operations::read(&peer, &mut bytes).await, Ok(7));
            assert_eq!(&bytes[..7], b"payload");
        }
        assert_not_returned(&returned);
        cancel.cancel();
        let app = async {
            let WriteResult::Write(completion) = completions.recv().await.unwrap() else {
                panic!("missing write completion");
            };
            assert_not_returned(&returned);
            client.complete_write(completion).unwrap();
            let receipt = (0..32)
                .find_map(|_| match client.next(&mut Ports) {
                    Some(Event::BodySent(receipt)) => Some(receipt),
                    Some(Event::SourceFinished(_) | Event::Deadline(_) | Event::Cancel(_))
                    | None => None,
                    _ => panic!("unexpected receipt event"),
                })
                .expect("missing forwarded receipt");
            assert_eq!(receipt.acceptance, core::Acceptance::Exact);
            assert_eq!(receipt.accepted, if success { 7 } else { 0 });
            assert_eq!(receipt.result.is_ok(), success);
            assert_not_returned(&returned);
            drop(receipt);
            assert_returned(server, returned, pointer);
            drop(send);
        };
        operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
            futures::join!(app, worker);
        })
        .await
        .unwrap();
    }
}
