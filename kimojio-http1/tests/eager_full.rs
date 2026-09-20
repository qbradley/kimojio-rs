use std::{cell::Cell, rc::Rc, time::Duration};

use futures::FutureExt;
use kimojio::{OwnedFdStream, operations};
use kimojio_http1::{
    Config, ConnectionId, IncomingBody, OutgoingBody, connect, connect_native,
    http::{Method, Request, Response},
    serve_connection, serve_connection_native,
};

fn config(slot: u64) -> Config {
    let mut config = Config::new(ConnectionId {
        slot,
        generation: 1,
    });
    config.protocol.max_buffer_bytes = 16 * 1024;
    config
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
