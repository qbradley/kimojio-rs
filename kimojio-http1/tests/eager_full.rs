use std::{cell::Cell, rc::Rc, time::Duration};

use futures::FutureExt;
use kimojio::{AsyncStreamRead, AsyncStreamWrite, Errno, OwnedFdStream, operations};
use kimojio_http1::{
    Config, ConnectionId, Error, IncomingBody, IncomingFrame, OutgoingBody, OutgoingFrame, connect,
    connect_native,
    http::{Method, Request, Response},
    serve_connection, serve_connection_native,
};

fn config(slot: u64) -> Config {
    let mut config = Config::new(ConnectionId {
        slot,
        generation: 1,
    });
    config.protocol.max_buffer_bytes = 16 * 1024;
    config.coalesce_full_bodies = true;
    config
}

#[kimojio::test]
async fn ready_full_response_remains_fair_with_a_runnable_source_and_small_budget() {
    struct SourceLifetime(Rc<Cell<bool>>);
    impl Drop for SourceLifetime {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }

    for native in [false, true] {
        for coalesce in [false, true] {
            let (client_fd, server_fd) = kimojio::pipe::bipipe();
            rustix::net::sockopt::set_socket_send_buffer_size(&server_fd, 4096).unwrap();
            let mut client_config = config(20);
            client_config.turn_budget = 1;
            client_config.coalesce_full_bodies = coalesce;
            let mut server_config = client_config.clone();
            server_config.connection_id.slot = 21;
            let (mut client, driver) = if native {
                let (client, driver) = connect_native(client_fd, client_config);
                (client, driver.run().boxed_local())
            } else {
                let (client, driver) = connect(OwnedFdStream::new(client_fd), client_config);
                (client, driver.run().boxed_local())
            };
            let polls = Rc::new(Cell::new(0));
            let source_polls = polls.clone();
            let dropped = Rc::new(Cell::new(false));
            let lifetime = SourceLifetime(dropped.clone());
            let source = futures::stream::repeat_with(move || {
                let _lifetime = &lifetime;
                source_polls.set(source_polls.get() + 1);
                Ok(OutgoingFrame::Data(Vec::new()))
            });
            let handler = |_| async {
                operations::yield_io().await;
                Ok(Response::builder()
                    .status(413)
                    .body(OutgoingBody::full(vec![0x57; 8192]))
                    .unwrap())
            };
            let server = if native {
                serve_connection_native(server_fd, server_config, handler).boxed_local()
            } else {
                serve_connection(OwnedFdStream::new(server_fd), server_config, handler)
                    .boxed_local()
            };
            let app = async {
                let mut response = client
                    .send(
                        Request::builder()
                            .method(Method::POST)
                            .uri("/")
                            .header("host", "test")
                            .body(OutgoingBody::from_stream(None, source))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), 413);
                assert_eq!(
                    response.body_mut().collect(8192).await.unwrap(),
                    [0x57; 8192]
                );
                client.shutdown().await.unwrap();
            };
            operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
                let ((), client, server) = futures::join!(app, driver, server);
                client.unwrap();
                server.unwrap();
            })
            .await
            .unwrap();
            assert!(polls.get() > 0);
            assert!(dropped.get());
        }
    }
}

async fn response_head(peer: &mut OwnedFdStream) {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        assert_eq!(peer.try_read(&mut byte, None).await.unwrap(), 1);
        head.push(byte[0]);
        assert!(head.len() < 1024);
    }
    let head = String::from_utf8(head).unwrap().to_ascii_lowercase();
    assert!(head.starts_with("http/1.1 200 "));
    assert!(head.contains("content-length: 2\r\n"));
    assert!(!head.contains("connection: close"));
    let mut body = [0; 2];
    peer.read(&mut body, None).await.unwrap();
    assert_eq!(&body, b"ok");
}

#[kimojio::test]
async fn opted_in_full_duplex_response_waits_for_an_independent_consumer_lease() {
    for native in [false, true] {
        for abandon in [false, true] {
            let (server_fd, peer_fd) = kimojio::pipe::bipipe();
            let mut peer = OwnedFdStream::new(peer_fd);
            let (send_lease, receive_lease) = kimojio::oneshot();
            let mut send_lease = Some(send_lease);
            let calls = Rc::new(Cell::new(0));
            let called = calls.clone();
            let handler = move |mut request: Request<IncomingBody>| {
                called.set(called.get() + 1);
                let send_lease = send_lease.take();
                async move {
                    if let Some(send) = send_lease {
                        let mut incoming = request.into_body();
                        let Some(IncomingFrame::Data(chunk)) = incoming.frame().await? else {
                            panic!()
                        };
                        assert_eq!(&*chunk, b"abc");
                        send.send((incoming, chunk)).unwrap();
                    } else {
                        assert_eq!(request.method(), Method::GET);
                        assert!(request.body_mut().collect(0).await?.is_empty());
                    }
                    Ok(Response::new(
                        OutgoingBody::full(b"ok").continue_request_body(),
                    ))
                }
            };
            let server = if native {
                serve_connection_native(server_fd, config(10), handler).boxed_local()
            } else {
                serve_connection(OwnedFdStream::new(server_fd), config(10), handler).boxed_local()
            };
            let app = async {
                peer.write(
                    b"POST /first HTTP/1.1\r\nHost: test\r\nContent-Length: 6\r\n\r\nabc",
                    None,
                )
                .await
                .unwrap();
                response_head(&mut peer).await;
                let (mut incoming, chunk) = receive_lease.recv().await.unwrap();
                peer.write(b"defGET /second HTTP/1.1\r\nHost: test\r\n\r\n", None)
                    .await
                    .unwrap();
                assert_eq!(calls.get(), 1);
                if abandon {
                    drop(incoming);
                    operations::yield_io().await;
                    assert_eq!(calls.get(), 1);
                    drop(chunk);
                    let mut byte = [0];
                    assert!(matches!(
                        peer.try_read(&mut byte, None).await,
                        Ok(0) | Err(Errno::CONNRESET)
                    ));
                    assert_eq!(calls.get(), 1);
                } else {
                    operations::yield_io().await;
                    assert_eq!(calls.get(), 1, "retained lease must block reuse");
                    drop(chunk);
                    assert_eq!(incoming.collect(3).await.unwrap(), b"def");
                    response_head(&mut peer).await;
                    assert_eq!(calls.get(), 2);
                }
                peer.close().await.unwrap();
            };
            let ((), result) =
                operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
                    futures::join!(app, server)
                })
                .await
                .unwrap();
            if abandon {
                assert!(matches!(
                    result,
                    Err(Error::Protocol(kimojio_fsm_http1::Failure::Cancelled))
                ));
            } else {
                result.unwrap();
            }
        }
    }
}

#[kimojio::test]
async fn full_body_fast_path_and_expect_head_fallback_reuse_both_backends() {
    for native in [false, true] {
        let (client_fd, server_fd) = kimojio::pipe::bipipe();
        rustix::net::sockopt::set_socket_send_buffer_size(&server_fd, 4096).unwrap();
        let (mut client, driver) = if native {
            let (client, driver) = connect_native(client_fd, config(1));
            (client, driver.run().boxed_local())
        } else {
            let (client, driver) = connect(OwnedFdStream::new(client_fd), config(1));
            (client, driver.run().boxed_local())
        };
        let handled = Rc::new(Cell::new(0));
        let count = handled.clone();
        let handler = move |mut request: Request<IncomingBody>| {
            count.set(count.get() + 1);
            async move {
                let expected = if request.method() == Method::HEAD {
                    0
                } else {
                    8192
                };
                let bytes = request.body_mut().collect(8192).await?;
                assert_eq!(bytes.len(), expected);
                assert!(bytes.iter().all(|byte| *byte == 0x12));
                Ok(Response::new(OutgoingBody::full(vec![0x34; 8192])))
            }
        };
        let server = if native {
            serve_connection_native(server_fd, config(2), handler).boxed_local()
        } else {
            serve_connection(OwnedFdStream::new(server_fd), config(2), handler).boxed_local()
        };
        let app = async {
            for (method, expect) in [
                (Method::POST, false),
                (Method::HEAD, false),
                (Method::POST, true),
            ] {
                let head = method == Method::HEAD;
                let mut request = Request::builder()
                    .method(method)
                    .uri("/")
                    .header("host", "test");
                if expect {
                    request = request.header("expect", "100-continue");
                }
                let body = if head {
                    OutgoingBody::empty()
                } else {
                    OutgoingBody::full(vec![0x12; 8192])
                };
                let mut response = client.send(request.body(body).unwrap()).await.unwrap();
                assert_eq!(response.headers()["content-length"], "8192");
                assert!(!response.headers().contains_key("connection"));
                let bytes = response.body_mut().collect(8192).await.unwrap();
                assert_eq!(bytes.len(), if head { 0 } else { 8192 });
                assert!(bytes.iter().all(|byte| *byte == 0x34));
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
