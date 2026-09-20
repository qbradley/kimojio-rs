use std::{cell::RefCell, future::Future, rc::Rc, time::Duration};

use futures::FutureExt;
use kimojio::{oneshot, operations};
use kimojio_http2::{
    Config, Error, OutgoingBody, connect_native,
    http::{HeaderValue, Request, Response, Version},
    serve_connection_native,
};

fn request(path: &str) -> Request<OutgoingBody> {
    Request::builder()
        .uri(format!("http://informational.test{path}"))
        .body(OutgoingBody::empty())
        .unwrap()
}

fn information(status: u16) -> Response<()> {
    Response::builder().status(status).body(()).unwrap()
}

async fn bounded(future: impl Future<Output = ()>) {
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(10), future)
        .await
        .unwrap();
}

#[kimojio::test]
async fn actual_103_metadata_is_observable_before_the_final_response() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, connection) = connect_native(fd, Config::default());
    let (final_response, receive) = oneshot();
    let mut receive = Some(receive);
    let server = serve_connection_native(peer, Config::default(), move |request| {
        let wait = if request.uri().path() == "/observe" {
            receive.take()
        } else {
            None
        };
        async move {
            let sender = request.body().informational_sender().unwrap();
            sender.send(information(100)).await?;
            let mut hints = information(103);
            hints
                .headers_mut()
                .append("link", "</first>; rel=preload".parse().unwrap());
            hints
                .headers_mut()
                .append("link", "</second>; rel=preload".parse().unwrap());
            let mut secret = HeaderValue::from_static("sensitive");
            secret.set_sensitive(true);
            hints.headers_mut().insert("x-sensitive", secret);
            sender.send(hints).await?;
            if let Some(wait) = wait {
                wait.recv().await.unwrap();
            }
            Ok(Response::new(OutgoingBody::from_static(b"final")))
        }
    });
    let observed = Rc::new(RefCell::new(Vec::new()));
    let callback = observed.clone();
    let app = async {
        let mut sending = Box::pin(client.send_with_informational(
            request("/observe"),
            move |head| {
                assert_eq!(head.version(), Version::HTTP_2);
                callback.borrow_mut().push(head);
            },
        ));
        assert!(sending.as_mut().now_or_never().is_none());
        while observed.borrow().len() < 2 {
            operations::yield_io().await;
        }
        assert!(sending.as_mut().now_or_never().is_none());
        {
            let heads = observed.borrow();
            assert_eq!(heads[0].status(), 100);
            assert_eq!(heads[1].status(), 103);
            assert_eq!(heads[1].headers().get_all("link").iter().count(), 2);
            assert!(heads[1].headers()["x-sensitive"].is_sensitive());
        }
        final_response.send(()).unwrap();
        let mut response = sending.await.unwrap();
        assert!(response.body().informational_sender().is_none());
        assert_eq!(response.status(), 200);
        assert_eq!(response.body_mut().collect(32).await.unwrap(), b"final");
        response.body_mut().completion().await.unwrap();
        let mut ordinary = client.send(request("/ordinary")).await.unwrap();
        assert_eq!(ordinary.body_mut().collect(32).await.unwrap(), b"final");
        ordinary.body_mut().completion().await.unwrap();
        assert_eq!(observed.borrow().len(), 2);
        client.control().graceful();
    };
    bounded(async {
        let ((), client, server) = futures::join!(app, connection.run(), server);
        client.unwrap();
        server.unwrap();
    })
    .await;
}

#[kimojio::test]
async fn invalid_informational_commands_do_not_commit_or_poison_the_final_response() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, connection) = connect_native(fd, Config::default());
    let server = serve_connection_native(
        peer,
        Config {
            max_response_storage: 1024,
            ..Config::default()
        },
        |request| async move {
            let sender = request.body().informational_sender().unwrap();
            assert_eq!(
                sender.send(information(200)).await,
                Err(Error::InvalidMetadata)
            );
            assert_eq!(
                sender.send(information(101)).await,
                Err(Error::InvalidMetadata)
            );
            let mut invalid = information(103);
            invalid
                .headers_mut()
                .insert("content-length", "0".parse().unwrap());
            assert!(matches!(sender.send(invalid).await, Err(Error::Command(_))));
            let mut large = information(103);
            large
                .headers_mut()
                .insert("x-large", "x".repeat(4096).parse().unwrap());
            assert_eq!(sender.send(large).await, Err(Error::Limit));
            sender.send(information(103)).await?;
            Ok(Response::new(OutgoingBody::empty()))
        },
    );
    let observed = Rc::new(RefCell::new(Vec::new()));
    let callback = observed.clone();
    let app = async {
        let mut response = client
            .send_with_informational(request("/invalid"), move |head| {
                callback.borrow_mut().push(head.status().as_u16());
            })
            .await
            .unwrap();
        response.body_mut().collect(0).await.unwrap();
        response.body_mut().completion().await.unwrap();
        assert_eq!(*observed.borrow(), [103]);
        client.control().graceful();
    };
    bounded(async {
        let ((), client, server) = futures::join!(app, connection.run(), server);
        client.unwrap();
        server.unwrap();
    })
    .await;
}

#[kimojio::test]
async fn a_cancelled_queued_informational_head_never_reaches_the_wire() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, connection) = connect_native(fd, Config::default());
    let server = serve_connection_native(peer, Config::default(), |request| async move {
        let sender = request.body().informational_sender().unwrap();
        let mut first = Box::pin(sender.send(information(103)));
        assert!(first.as_mut().now_or_never().is_none());
        assert_eq!(sender.send(information(100)).await, Err(Error::Limit));
        drop(first);
        Ok(Response::new(OutgoingBody::empty()))
    });
    let app = async {
        let mut response = client
            .send_with_informational(request("/cancel"), |_| {
                panic!("cancelled information reached the peer");
            })
            .await
            .unwrap();
        response.body_mut().collect(0).await.unwrap();
        response.body_mut().completion().await.unwrap();
        client.control().graceful();
    };
    bounded(async {
        let ((), client, server) = futures::join!(app, connection.run(), server);
        client.unwrap();
        server.unwrap();
    })
    .await;
}

#[kimojio::test]
async fn final_metadata_seals_informational_emission() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, connection) = connect_native(fd, Config::default());
    let (send, receive) = oneshot();
    let mut send = Some(send);
    let server = serve_connection_native(peer, Config::default(), move |request| {
        let send = send.take().unwrap();
        async move {
            send.send(request.body().informational_sender().unwrap())
                .ok()
                .unwrap();
            Ok(Response::new(OutgoingBody::from_stream(
                futures::stream::pending(),
            )))
        }
    });
    let app = async {
        let mut response = client.send(request("/final")).await.unwrap();
        let sender = receive.recv().await.unwrap();
        assert!(matches!(
            sender.send(information(103)).await,
            Err(Error::Command(_))
        ));
        response.body().cancel();
        assert!(response.body_mut().completion().await.is_err());
        client.control().graceful();
    };
    bounded(async {
        let ((), client, server) = futures::join!(app, connection.run(), server);
        client.unwrap();
        server.unwrap();
    })
    .await;
}

#[kimojio::test]
async fn informational_callback_failure_is_stream_local() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, connection) = connect_native(fd, Config::default());
    let server = serve_connection_native(peer, Config::default(), |request| async move {
        request
            .body()
            .informational_sender()
            .unwrap()
            .send(information(103))
            .await?;
        Ok(Response::new(OutgoingBody::empty()))
    });
    let app = async {
        assert!(matches!(
            client
                .send_with_informational(request("/callback"), |_| {
                    panic!("callback failure");
                })
                .await,
            Err(Error::Application(_))
        ));
        let mut sibling = client.send(request("/sibling")).await.unwrap();
        sibling.body_mut().collect(0).await.unwrap();
        sibling.body_mut().completion().await.unwrap();
        client.control().graceful();
    };
    bounded(async {
        let ((), client, server) = futures::join!(app, connection.run(), server);
        client.unwrap();
        server.unwrap();
    })
    .await;
}

#[kimojio::test]
async fn queued_information_precedes_final_metadata_under_admission_pressure() {
    let (fd, peer) = kimojio::pipe::bipipe();
    rustix::net::sockopt::set_socket_send_buffer_size(&peer, 1024).unwrap();
    let (client, connection) = connect_native(fd, Config::default());
    let mut config = Config {
        turn_budget: 1,
        ..Config::default()
    };
    config.protocol.max_outbound_items = 4;
    config.protocol.max_outbound_capacity = 65536;
    let information_tasks = Rc::new(RefCell::new(Vec::new()));
    let tasks = information_tasks.clone();
    let server = serve_connection_native(peer, config, move |request| {
        let tasks = tasks.clone();
        async move {
            let sender = request.body().informational_sender().unwrap();
            let (submitted, receive) = oneshot();
            let task = operations::spawn_task(async move {
                let mut head = information(103);
                head.headers_mut()
                    .insert("link", "a".repeat(8192).parse().unwrap());
                let mut sending = Box::pin(sender.send(head));
                assert!(sending.as_mut().now_or_never().is_none());
                submitted.send(()).unwrap();
                sending.await
            });
            tasks.borrow_mut().push(task);
            receive.recv().await.unwrap();
            Ok(Response::builder()
                .header("x-final", "b".repeat(8192))
                .body(OutgoingBody::from_static(b"final"))
                .unwrap())
        }
    });
    let app = async {
        let mut requests = Vec::new();
        for _ in 0..24 {
            let client = client.clone();
            requests.push(operations::spawn_task(async move {
                let observed = Rc::new(RefCell::new(Vec::new()));
                let callback = observed.clone();
                let mut response = client
                    .send_with_informational(request("/pressure"), move |head| {
                        assert_eq!(head.status(), 103);
                        assert_eq!(head.headers()["link"].as_bytes().len(), 8192);
                        callback.borrow_mut().push(head.status().as_u16());
                    })
                    .await
                    .unwrap();
                assert_eq!(*observed.borrow(), [103]);
                assert_eq!(response.headers()["x-final"].as_bytes().len(), 8192);
                assert_eq!(response.body_mut().collect(32).await.unwrap(), b"final");
                response.body_mut().completion().await.unwrap();
            }));
        }
        for request in requests {
            request.await.unwrap();
        }
        let tasks = std::mem::take(&mut *information_tasks.borrow_mut());
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        client.control().graceful();
    };
    bounded(async {
        let ((), client, server) = futures::join!(app, connection.run(), server);
        client.unwrap();
        server.unwrap();
    })
    .await;
}

#[kimojio::test]
async fn dropping_an_already_accepted_send_does_not_cancel_a_newer_head() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, connection) = connect_native(fd, Config::default());
    let (observed_first, receive) = oneshot();
    let mut receive = Some(receive);
    let server = serve_connection_native(peer, Config::default(), move |request| {
        let receive = receive.take().unwrap();
        async move {
            let sender = request.body().informational_sender().unwrap();
            let mut first = Box::pin(sender.send(information(103)));
            assert!(first.as_mut().now_or_never().is_none());
            receive.recv().await.unwrap();
            let mut second = Box::pin(sender.send(information(103)));
            assert!(second.as_mut().now_or_never().is_none());
            drop(first);
            second.await?;
            Ok(Response::new(OutgoingBody::empty()))
        }
    });
    let observed = Rc::new(RefCell::new(Vec::new()));
    let callback = observed.clone();
    let mut observed_first = Some(observed_first);
    let app = async {
        let mut response = client
            .send_with_informational(request("/generation"), move |head| {
                callback.borrow_mut().push(head.status().as_u16());
                if let Some(send) = observed_first.take() {
                    send.send(()).unwrap();
                }
            })
            .await
            .unwrap();
        response.body_mut().collect(0).await.unwrap();
        response.body_mut().completion().await.unwrap();
        assert_eq!(*observed.borrow(), [103, 103]);
        client.control().graceful();
    };
    bounded(async {
        let ((), client, server) = futures::join!(app, connection.run(), server);
        client.unwrap();
        server.unwrap();
    })
    .await;
}

#[kimojio::test]
async fn observed_continue_can_release_a_scoped_upload_source() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, connection) = connect_native(fd, Config::default());
    let (upload, receive) = oneshot();
    let server = serve_connection_native(peer, Config::default(), |mut request| async move {
        request
            .body()
            .informational_sender()
            .unwrap()
            .send(information(100))
            .await?;
        let bytes = request.body_mut().collect(32).await?;
        Ok(Response::new(OutgoingBody::full(bytes)))
    });
    let mut upload = Some(upload);
    let app = async {
        let source = futures::stream::once(async move {
            receive.recv().await.unwrap();
            Ok(kimojio_http2::OutgoingFrame::Static(b"continued"))
        });
        let request = Request::builder()
            .method("POST")
            .uri("http://informational.test/continue")
            .header("expect", "100-continue")
            .body(OutgoingBody::from_stream(source))
            .unwrap();
        let mut response = client
            .send_with_informational(request, move |head| {
                assert_eq!(head.status(), 100);
                upload.take().unwrap().send(()).unwrap();
            })
            .await
            .unwrap();
        assert_eq!(response.body_mut().collect(32).await.unwrap(), b"continued");
        response.body_mut().completion().await.unwrap();
        client.control().graceful();
    };
    bounded(async {
        let ((), client, server) = futures::join!(app, connection.run(), server);
        client.unwrap();
        server.unwrap();
    })
    .await;
}
