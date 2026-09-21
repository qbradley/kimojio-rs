use super::{Transport, outgoing, schema::*};
use futures::{FutureExt, future::Either};
use kimojio::{OwnedFdStream, operations, socket_helpers};
use kimojio_http2::{
    Config, ConnectionResult, Error, IncomingBody, IncomingFrame, OutgoingBody, OutgoingFrame,
    Shutdown,
    http::{HeaderMap, Method, Request, Response, header},
    serve_connection_native_with_shutdown, serve_connection_with_shutdown,
};
use std::{io::Write, net::SocketAddr, time::Duration};

pub(super) async fn run(input: ServerInput, transport: Transport) -> Result<(), String> {
    operations::io_scope(async move || {
        let config = input.config.config()?;
        let listener = operations::socket(
            operations::AddressFamily::INET,
            operations::SocketType::STREAM,
            Some(operations::ipproto::TCP),
        )
        .await
        .map_err(|error| format!("native listen socket: {error}"))?;
        operations::bind(&listener, &SocketAddr::from(([127, 0, 0, 1], 0)))
            .map_err(|error| format!("bind: {error}"))?;
        operations::listen(&listener, 128).map_err(|error| format!("listen: {error}"))?;
        let address = rustix::net::getsockname(&listener)
            .map_err(|error| format!("listen address: {error}"))?;
        let port = std::net::SocketAddrV4::try_from(address)
            .map_err(|error| format!("IPv4 listen address: {error}"))?
            .port();
        println!("LISTEN 127.0.0.1:{port}");
        std::io::stdout()
            .flush()
            .map_err(|error| error.to_string())?;
        loop {
            let socket = operations::accept(&listener)
                .await
                .map_err(|error| format!("native accept: {error}"))?;
            socket_helpers::update_accept_socket(&socket)
                .map_err(|error| format!("accepted socket options: {error}"))?;
            let result = serve(
                socket,
                config.clone(),
                Duration::from_millis(input.timeout_ms),
                transport,
            )
            .await;
            match result {
                Ok(()) | Err(Error::Connection(ConnectionResult::PeerClosed)) => (),
                Err(error @ Error::Connection(_)) => {
                    eprintln!("wrapper interop server connection: {error}");
                }
                Err(error) => return Err(format!("wrapper interop server: {error}")),
            }
        }
    })
    .await
}

async fn serve(
    fd: kimojio::OwnedFd,
    config: Config,
    timeout: Duration,
    transport: Transport,
) -> Result<(), Error> {
    operations::io_scope(async move || {
        let shutdown = Shutdown::default();
        let connection = match transport {
            Transport::Native => {
                serve_connection_native_with_shutdown(fd, config, shutdown.clone(), handle)
                    .boxed_local()
            }
            Transport::Generic => serve_connection_with_shutdown(
                OwnedFdStream::new(fd),
                config,
                shutdown.clone(),
                handle,
            )
            .boxed_local(),
        };
        let alarm = operations::sleep(timeout);
        match futures::future::select(connection, alarm).await {
            Either::Left((result, _alarm)) => result,
            Either::Right((alarm, connection)) => {
                shutdown.abort();
                let result = operations::timeout_at(
                    kimojio::clock_now() + Duration::from_secs(3),
                    connection,
                )
                .await
                .map_err(|error| {
                    Error::Application(format!(
                        "server settlement watchdog: {error:?}; descriptor closure is unconfirmed"
                    ))
                })?;
                Err(Error::Application(format!(
                    "server connection watchdog: alarm={alarm:?}, terminal={result:?}"
                )))
            }
        }
    })
    .await
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    Bytes(u64),
    Trailers(u64),
    Informational(u64),
    NoContent,
    Early,
}

fn route(path: &str) -> Option<Route> {
    if path == "/no-content" {
        return Some(Route::NoContent);
    }
    if path == "/early" {
        return Some(Route::Early);
    }
    let (kind, count) = path.strip_prefix('/')?.split_once('/')?;
    if count.is_empty() || !count.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let count = count.parse::<u64>().ok().filter(|n| *n <= MAX_BODY)?;
    match kind {
        "bytes" => Some(Route::Bytes(count)),
        "trailers" => Some(Route::Trailers(count)),
        "informational" => Some(Route::Informational(count)),
        _ => None,
    }
}

fn response(
    status: u16,
    length: Option<u64>,
    body: OutgoingBody,
) -> Result<Response<OutgoingBody>, Error> {
    let mut builder = Response::builder().status(status);
    if let Some(length) = length {
        builder = builder.header(header::CONTENT_LENGTH, length.to_string());
    }
    builder.body(body).map_err(|_| Error::InvalidMetadata)
}

fn echo(body: IncomingBody) -> OutgoingBody {
    OutgoingBody::from_stream(futures::stream::try_unfold(body, |mut body| async move {
        loop {
            match body.frame().await? {
                Some(IncomingFrame::Data(chunk)) => {
                    return Ok(Some((OutgoingFrame::Forward(chunk), body)));
                }
                Some(IncomingFrame::Trailers(_)) => (),
                None => return Ok(None),
            }
        }
    }))
}

pub(super) async fn handle(
    request: Request<IncomingBody>,
) -> Result<Response<OutgoingBody>, Error> {
    let id = request.body().stream_id().get();
    let head = request.method() == Method::HEAD;
    if request.method() == Method::CONNECT || request.uri().path() == "/echo" {
        if head {
            let length = request
                .headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|value| kimojio_fsm_http2::parse_content_length(value.as_bytes()))
                .unwrap_or(0);
            return response(200, Some(length as u64), OutgoingBody::empty());
        }
        return response(200, None, echo(request.into_body()));
    }
    let Some(route) = route(request.uri().path()) else {
        return response(404, Some(0), OutgoingBody::empty());
    };
    let mut trailers = HeaderMap::new();
    let bytes = match route {
        Route::NoContent => return response(204, None, OutgoingBody::empty()),
        Route::Early => return response(413, Some(0), OutgoingBody::empty()),
        Route::Bytes(bytes) => bytes,
        Route::Trailers(bytes) => {
            trailers.append("x-end", "done".parse().expect("static header"));
            bytes
        }
        Route::Informational(bytes) => {
            let hints = Response::builder()
                .status(103)
                .body(())
                .map_err(|_| Error::InvalidMetadata)?;
            request
                .body()
                .informational_sender()
                .ok_or(Error::Closed)?
                .send(hints)
                .await?;
            bytes
        }
    };
    // The wrapper drains and refunds unread uploads after this handle drops.
    drop(request);
    let body = if head {
        OutgoingBody::empty()
    } else {
        outgoing(bytes, id, trailers)
    };
    response(200, Some(bytes), body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[kimojio::test]
    async fn inline_watchdog_aborts_and_settles_both_transports() {
        for transport in [Transport::Native, Transport::Generic] {
            let (fd, peer) = kimojio::pipe::bipipe();
            operations::timeout_at(kimojio::clock_now() + Duration::from_secs(5), async {
                let error = serve(fd, Config::default(), Duration::ZERO, transport)
                    .await
                    .unwrap_err();
                assert!(
                    matches!(error, Error::Application(message) if message.starts_with("server connection watchdog:"))
                );
                let mut bytes = [0; 1024];
                while operations::read(&peer, &mut bytes).await.unwrap() != 0 {}
                operations::close(peer).await.unwrap();
            })
            .await
            .unwrap();
        }
    }

    #[test]
    fn route_lengths_are_strict_and_bounded() {
        assert_eq!(route("/bytes/16777233"), Some(Route::Bytes(16777233)));
        assert_eq!(route("/trailers/0"), Some(Route::Trailers(0)));
        assert_eq!(route("/informational/37"), Some(Route::Informational(37)));
        for path in [
            "/bytes/-1",
            "/bytes/+1",
            "/bytes/1/2",
            "/bytes/",
            "/bytes/1073741825",
        ] {
            assert_eq!(route(path), None, "{path}");
        }
    }

    #[test]
    fn head_and_no_content_metadata_do_not_imply_payload() {
        let head = response(200, Some(131087), OutgoingBody::empty()).unwrap();
        assert_eq!(head.headers()[header::CONTENT_LENGTH], "131087");
        let no_content = response(204, None, OutgoingBody::empty()).unwrap();
        assert!(!no_content.headers().contains_key(header::CONTENT_LENGTH));
        assert!(
            super::super::trailer_fields(no_content.headers())
                .unwrap()
                .is_empty()
        );
    }
}
