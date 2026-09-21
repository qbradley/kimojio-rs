#![cfg(feature = "virtual-clock")]

use std::{cell::Cell, future::Future, rc::Rc, time::Duration};

use futures::{FutureExt, future::LocalBoxFuture};
use kimojio::{OwnedFdStream, operations};
use kimojio_http1::{
    Config, ConnectionId, Error, OutgoingBody, http::Response, serve_connection,
    serve_connection_native,
};

const HEAD_TIMEOUT: Duration = Duration::from_secs(1);
const FIRST: &[u8] = b"GET /first HTTP/1.1\r\nHost: test\r\n\r\n";
const PARTIAL: &[u8] = b"GET /second HTTP/1.1\r\nHost:";
const OK: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n";
const TIMEOUT: &[u8] =
    b"HTTP/1.1 408 Request Timeout\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";

type Server = LocalBoxFuture<'static, Result<(), Error>>;

fn server(native: bool, fd: std::os::fd::OwnedFd, handled: Rc<Cell<usize>>) -> Server {
    let mut config = Config::new(ConnectionId {
        slot: 1,
        generation: 1,
    });
    config.protocol.idle_timeout_ns = None;
    config.protocol.head_timeout_ns = Some(1_000_000_000);
    let handler = move |request: kimojio_http1::http::Request<_>| {
        assert_eq!(request.uri().path(), "/first");
        assert_eq!(handled.replace(handled.get() + 1), 0);
        async { Ok(Response::new(OutgoingBody::empty())) }
    };
    if native {
        serve_connection_native(fd, config, handler).boxed_local()
    } else {
        serve_connection(OwnedFdStream::new(fd), config, handler).boxed_local()
    }
}

async fn while_serving<T>(server: &mut Server, operation: impl Future<Output = T>) -> T {
    futures::pin_mut!(operation);
    futures::future::poll_fn(|cx| {
        assert!(server.as_mut().poll(cx).is_pending(), "server closed early");
        operation.as_mut().poll(cx)
    })
    .await
}

async fn wait_for_deadline(server: &mut Server, expected: Option<std::time::Instant>) {
    for _ in 0..1000 {
        assert!(
            operations::poll_once(std::pin::Pin::new(&mut *server))
                .await
                .is_none()
        );
        if operations::virtual_clock_next_deadline() == expected {
            assert_eq!(
                operations::virtual_clock_pending_timers(),
                usize::from(expected.is_some())
            );
            return;
        }
        operations::yield_io().await;
    }
    panic!("receive phase did not publish deadline {expected:?}");
}

async fn first_response(server: &mut Server, peer: &std::os::fd::OwnedFd) {
    let mut wire = vec![0; OK.len()];
    let mut offset = 0;
    while offset < wire.len() {
        let n = while_serving(server, operations::read(peer, &mut wire[offset..]))
            .await
            .unwrap();
        assert_ne!(n, 0);
        offset += n;
    }
    assert_eq!(wire, OK);
}

async fn finish(server: Server, peer: &std::os::fd::OwnedFd, timeout: bool, expected: &[u8]) {
    let read_to_eof = async {
        let mut wire = Vec::new();
        let mut buffer = [0; 256];
        loop {
            let n = operations::read(peer, &mut buffer).await.unwrap();
            if n == 0 {
                return wire;
            }
            wire.extend_from_slice(&buffer[..n]);
        }
    };
    let (result, wire) = futures::join!(server, read_to_eof);
    if timeout {
        assert!(matches!(
            result,
            Err(Error::Protocol(kimojio_fsm_http1::Failure::Timeout))
        ));
    } else {
        result.unwrap();
    }
    assert_eq!(wire, expected);
    assert_eq!(operations::virtual_clock_pending_timers(), 0);
}

#[kimojio::test]
async fn reused_partial_head_times_out_with_idle_disabled() {
    operations::virtual_clock_enable(true);
    for native in [false, true] {
        let (fd, peer) = kimojio::pipe::bipipe();
        let handled = Rc::new(Cell::new(0));
        let mut server = server(native, fd, handled.clone());
        assert_eq!(operations::write(&peer, FIRST).await.unwrap(), FIRST.len());
        first_response(&mut server, &peer).await;
        wait_for_deadline(&mut server, None).await;
        assert_eq!(handled.get(), 1);

        operations::virtual_clock_advance(Duration::from_secs(20));
        wait_for_deadline(&mut server, None).await;
        assert_eq!(
            while_serving(&mut server, operations::write(&peer, PARTIAL))
                .await
                .unwrap(),
            PARTIAL.len()
        );
        let deadline = kimojio::clock_now() + HEAD_TIMEOUT;
        wait_for_deadline(&mut server, Some(deadline)).await;
        operations::virtual_clock_advance(HEAD_TIMEOUT - Duration::from_nanos(1));
        wait_for_deadline(&mut server, Some(deadline)).await;
        operations::virtual_clock_advance(Duration::from_nanos(1));
        finish(server, &peer, true, TIMEOUT).await;
        assert_eq!(handled.get(), 1);
    }
}

#[kimojio::test]
async fn pipelined_partial_head_times_out_after_first_response() {
    operations::virtual_clock_enable(true);
    for native in [false, true] {
        let (fd, peer) = kimojio::pipe::bipipe();
        let handled = Rc::new(Cell::new(0));
        let mut server = server(native, fd, handled.clone());
        let input = [FIRST, PARTIAL].concat();
        assert_eq!(operations::write(&peer, &input).await.unwrap(), input.len());
        first_response(&mut server, &peer).await;
        let deadline = kimojio::clock_now() + HEAD_TIMEOUT;
        wait_for_deadline(&mut server, Some(deadline)).await;
        assert_eq!(handled.get(), 1);
        operations::virtual_clock_advance(HEAD_TIMEOUT - Duration::from_nanos(1));
        wait_for_deadline(&mut server, Some(deadline)).await;
        operations::virtual_clock_advance(Duration::from_nanos(1));
        finish(server, &peer, true, TIMEOUT).await;
        assert_eq!(handled.get(), 1);
    }
}

#[kimojio::test]
async fn reused_idle_eof_does_not_start_a_head_timeout() {
    operations::virtual_clock_enable(true);
    for native in [false, true] {
        let (fd, peer) = kimojio::pipe::bipipe();
        let handled = Rc::new(Cell::new(0));
        let mut server = server(native, fd, handled.clone());
        assert_eq!(operations::write(&peer, FIRST).await.unwrap(), FIRST.len());
        first_response(&mut server, &peer).await;
        wait_for_deadline(&mut server, None).await;
        operations::virtual_clock_advance(Duration::from_secs(20));
        wait_for_deadline(&mut server, None).await;
        rustix::net::shutdown(&peer, rustix::net::Shutdown::Write).unwrap();
        finish(server, &peer, false, b"").await;
        assert_eq!(handled.get(), 1);
    }
}

#[kimojio::test]
async fn initial_connection_still_starts_its_head_timeout_without_input() {
    operations::virtual_clock_enable(true);
    for native in [false, true] {
        let (fd, peer) = kimojio::pipe::bipipe();
        let handled = Rc::new(Cell::new(0));
        let mut server = server(native, fd, handled.clone());
        let deadline = kimojio::clock_now() + HEAD_TIMEOUT;
        wait_for_deadline(&mut server, Some(deadline)).await;
        operations::virtual_clock_advance(HEAD_TIMEOUT);
        finish(server, &peer, true, b"").await;
        assert_eq!(handled.get(), 0);
    }
}
