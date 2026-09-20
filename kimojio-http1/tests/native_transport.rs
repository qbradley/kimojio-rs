use std::{cell::Cell, rc::Rc, time::Duration};

use futures::FutureExt;
use kimojio::{OwnedFdStream, operations};
use kimojio_http1::{
    Config, ConnectionId, IncomingBody, IncomingFrame, OutgoingBody, OutgoingFrame, Shutdown,
    connect, connect_native,
    http::{HeaderMap, Request, Response},
    serve_connection, serve_connection_native, serve_connection_native_with_shutdown,
};

fn config(slot: u64) -> Config {
    let mut config = Config::new(ConnectionId {
        slot,
        generation: 1,
    });
    config.protocol.max_buffer_bytes = 16 * 1024;
    config.protocol.max_head_bytes = 8 * 1024;
    config
}

fn request(body: OutgoingBody) -> Request<OutgoingBody> {
    Request::builder()
        .method("POST")
        .uri("/")
        .header("host", "test")
        .body(body)
        .unwrap()
}

fn response_body() -> OutgoingBody {
    let mut trailers = HeaderMap::new();
    trailers.insert("x-complete", "yes".parse().unwrap());
    let frames = (0..4)
        .map(|_| Ok(OutgoingFrame::Data(vec![0xfe; 16 * 1024])))
        .chain([Ok(OutgoingFrame::Trailers(trailers))]);
    OutgoingBody::from_stream(None, futures::stream::iter(frames))
}

#[kimojio::test]
async fn native_and_generic_backends_interoperate_and_reuse_one_connection() {
    for native_client in [false, true] {
        for native_server in [false, true] {
            let (client_fd, server_fd) = kimojio::pipe::bipipe();
            rustix::net::sockopt::set_socket_send_buffer_size(&server_fd, 4096).unwrap();
            let (mut client, driver) = if native_client {
                let (client, driver) = connect_native(client_fd, config(1));
                (client, driver.run().boxed_local())
            } else {
                let (client, driver) = connect(OwnedFdStream::new(client_fd), config(1));
                (client, driver.run().boxed_local())
            };
            let handled = Rc::new(Cell::new(0));
            let handler_count = handled.clone();
            let handler = move |mut request: Request<IncomingBody>| {
                handler_count.set(handler_count.get() + 1);
                async move {
                    assert_eq!(request.body_mut().collect(64).await?, b"request");
                    Ok(Response::new(response_body()))
                }
            };
            let server = if native_server {
                serve_connection_native(server_fd, config(2), handler).boxed_local()
            } else {
                serve_connection(OwnedFdStream::new(server_fd), config(2), handler).boxed_local()
            };
            let app = async {
                for _ in 0..3 {
                    let mut response = client
                        .send(request(OutgoingBody::full(b"request")))
                        .await
                        .unwrap();
                    let mut count = 0;
                    let mut trailers = false;
                    while let Some(frame) = response.body_mut().frame().await.unwrap() {
                        match frame {
                            IncomingFrame::Data(chunk) => {
                                assert!(chunk.iter().all(|byte| *byte == 0xfe));
                                count += chunk.len();
                            }
                            IncomingFrame::Trailers(headers) => {
                                assert_eq!(headers["x-complete"], "yes");
                                trailers = true;
                            }
                        }
                    }
                    assert_eq!(count, 4 * 16 * 1024);
                    assert!(trailers);
                }
                client.shutdown().await.unwrap();
            };
            operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
                let ((), client, server) = futures::join!(app, driver, server);
                client.unwrap();
                server.unwrap();
            })
            .await
            .unwrap();
            assert_eq!(handled.get(), 3);
        }
    }
}

#[kimojio::test]
async fn native_cross_connection_forwarding_preserves_payload_and_trailers() {
    let (source, origin) = kimojio::pipe::bipipe();
    let (destination, sink) = kimojio::pipe::bipipe();
    rustix::net::sockopt::set_socket_send_buffer_size(&destination, 4096).unwrap();
    let (mut source, source_driver) = connect_native(source, config(10));
    let (mut destination, destination_driver) = connect_native(destination, config(11));
    let origin = serve_connection_native(origin, config(12), |_| async {
        Ok(Response::new(response_body()))
    });
    let sink = serve_connection_native(
        sink,
        config(13),
        |mut request: Request<IncomingBody>| async move {
            let mut count = 0;
            let mut trailers = false;
            while let Some(frame) = request.body_mut().frame().await? {
                match frame {
                    IncomingFrame::Data(chunk) => {
                        assert!(chunk.iter().all(|byte| *byte == 0xfe));
                        count += chunk.len();
                    }
                    IncomingFrame::Trailers(headers) => {
                        assert_eq!(headers["x-complete"], "yes");
                        trailers = true;
                    }
                }
            }
            assert_eq!(count, 4 * 16 * 1024);
            assert!(trailers);
            Ok(Response::new(OutgoingBody::full(b"forwarded")))
        },
    );
    let app = async {
        let response = source.send(request(OutgoingBody::empty())).await.unwrap();
        let mut response = destination
            .send(request(OutgoingBody::from_incoming(response.into_body())))
            .await
            .unwrap();
        assert_eq!(response.body_mut().collect(32).await.unwrap(), b"forwarded");
        source.shutdown().await.unwrap();
        destination.shutdown().await.unwrap();
    };
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
        let ((), source, destination, origin, sink) = futures::join!(
            app,
            source_driver.run(),
            destination_driver.run(),
            origin,
            sink
        );
        source.unwrap();
        destination.unwrap();
        origin.unwrap();
        sink.unwrap();
    })
    .await
    .unwrap();
}

#[kimojio::test]
async fn native_abort_settles_pending_read_before_peer_eof() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let shutdown = Shutdown::default();
    let mut server = Box::pin(serve_connection_native_with_shutdown(
        fd,
        config(20),
        shutdown.clone(),
        |_| async { panic!("no request was sent") },
    ));
    assert!(futures::poll!(server.as_mut()).is_pending());
    shutdown.abort();
    assert!(server.await.is_err());
    assert_eq!(operations::read(&peer, &mut [0; 1]).await, Ok(0));
}
