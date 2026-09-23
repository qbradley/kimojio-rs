use kimojio_fsm_http1::*;
use std::{convert::Infallible, sync::Arc};

#[derive(Default)]
struct Capture {
    read: Option<ReadOp<Vec<u8>>>,
    write: Option<WriteOp<Arc<[u8]>>>,
    exchange: Option<ExchangeId>,
    sent: Option<BodySent<Arc<[u8]>>>,
    demand: Option<usize>,
}

impl Ports<Vec<u8>, Arc<[u8]>> for Capture {
    type Output = Infallible;
    fn read(&mut self, op: ReadOp<Vec<u8>>) -> Option<Infallible> {
        assert!(self.read.replace(op).is_none());
        None
    }
    fn write(&mut self, op: WriteOp<Arc<[u8]>>) -> Option<Infallible> {
        assert!(self.write.replace(op).is_none());
        None
    }
    fn body_sent(&mut self, result: BodySent<Arc<[u8]>>) -> Option<Infallible> {
        assert!(self.sent.replace(result).is_none());
        None
    }
    fn send_ready(&mut self, _: ExchangeId, capacity: usize) -> Option<Infallible> {
        assert!(self.demand.replace(capacity).is_none());
        None
    }
    fn readiness(&mut self, _: ReadinessOp) -> Option<Infallible> {
        panic!("unexpected readiness")
    }
    fn cancel(&mut self, _: CancelOp) -> Option<Infallible> {
        panic!("unexpected cancellation")
    }
    fn close(&mut self, _: CloseOp) -> Option<Infallible> {
        panic!("unexpected close")
    }
    fn body(&mut self, _: BodyOp<Vec<u8>>) -> Option<Infallible> {
        panic!("unexpected incoming body")
    }
    fn trailers(&mut self, _: ExchangeId, _: Headers<'_>) -> Option<Infallible> {
        panic!("unexpected trailers")
    }
    fn incoming_finished(&mut self, _: ExchangeId) -> Option<Infallible> {
        None
    }
    fn exchange_finished(&mut self, result: ExchangeFinished) -> Option<Infallible> {
        assert_eq!(result.result, Ok(()));
        None
    }
    fn deadline_changed(&mut self, _: Option<Deadline>) -> Option<Infallible> {
        None
    }
    fn upgrade_ready(&mut self, _: ExchangeId) -> Option<Infallible> {
        panic!("unexpected upgrade")
    }
    fn closed(&mut self, _: ConnectionResult) -> Option<Infallible> {
        panic!("unexpected closed connection")
    }
}

impl ServerPorts<Vec<u8>, Arc<[u8]>> for Capture {
    fn request(&mut self, exchange: ExchangeId, _: RequestHead<'_>) -> Option<Infallible> {
        self.exchange = Some(exchange);
        None
    }
}

#[test]
fn readonly_shared_payload_keeps_its_address_across_partial_writes() {
    let mut server = Server::<Vec<u8>, Arc<[u8]>>::with_output_type(
        ConnectionId {
            slot: 1,
            generation: 1,
        },
        Config::default(),
        vec![0; 1024],
        Tick(0),
    )
    .unwrap();
    let mut ports = Capture::default();
    assert!(server.next(&mut ports).is_none());
    let mut read = ports.read.take().unwrap();
    let request = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n";
    read.bytes_mut()[..request.len()].copy_from_slice(request);
    server
        .complete_read(read.complete(Ok(request.len())))
        .unwrap();
    server.next(&mut ports);
    let exchange = ports.exchange.unwrap();
    server
        .respond(
            exchange,
            Response {
                head: ResponseHead {
                    version: Version::Http11,
                    status: 200,
                    reason: "OK",
                    headers: &[],
                },
                body: BodyLength::Known(5),
            },
        )
        .unwrap();
    server.next(&mut ports);
    let write = ports.write.take().unwrap();
    let len = write.slices().iter().map(|s| s.len()).sum();
    server.complete_write(write.complete(Ok(len))).unwrap();
    server.next(&mut ports);
    assert_eq!(ports.demand.take(), Some(5));

    let payload: Arc<[u8]> = Arc::from(b"hello".as_slice());
    server
        .send_body(SendBody {
            exchange,
            buffer: payload.clone(),
            range: 0..5,
            end: true,
        })
        .unwrap();
    server.next(&mut ports);
    let write = ports.write.take().unwrap();
    assert_eq!(write.slices()[1].as_ptr(), payload.as_ptr());
    assert_eq!(Arc::strong_count(&payload), 2);
    server.complete_write(write.complete(Ok(2))).unwrap();

    server.next(&mut ports);
    let write = ports.write.take().unwrap();
    assert_eq!(write.slices()[1], b"llo");
    assert_eq!(write.slices()[1].as_ptr(), payload.as_ptr().wrapping_add(2));
    server.complete_write(write.complete(Ok(3))).unwrap();
    server.next(&mut ports);
    let returned = ports.sent.take().unwrap();
    assert_eq!(returned.accepted, 5);
    assert_eq!(returned.result, Ok(()));
    assert!(Arc::ptr_eq(&returned.buffer, &payload));
    assert_eq!(Arc::strong_count(&payload), 2);
}
