use std::{io::Write, net::TcpListener};

use kimojio::{OwnedFd, OwnedFdStream, operations, task_pool::TaskPool};
use kimojio_http1::{
    Config, ConnectionId, Error, IncomingBody, OutgoingBody, OutgoingFrame,
    http::{HeaderMap, Request, Response, StatusCode},
    serve_connection,
};

async fn handle(request: Request<IncomingBody>) -> Result<Response<OutgoingBody>, Error> {
    match request.uri().path() {
        "/echo" => {
            let mut incoming = request.into_body();
            incoming.accept().await?;
            Ok(Response::new(OutgoingBody::from_incoming(incoming)))
        }
        "/trailers" => {
            let mut trailers = HeaderMap::new();
            trailers.insert("x-finished", "yes".parse().unwrap());
            let frames = [
                Ok(OutgoingFrame::Data(b"first\n".to_vec())),
                Ok(OutgoingFrame::Data(b"second\n".to_vec())),
                Ok(OutgoingFrame::Trailers(trailers)),
            ];
            let mut response = Response::new(OutgoingBody::from_stream(
                None,
                futures::stream::iter(frames),
            ));
            response
                .headers_mut()
                .insert("trailer", "x-finished".parse().unwrap());
            Ok(response)
        }
        "/early" => {
            let mut response = Response::new(OutgoingBody::empty());
            *response.status_mut() = StatusCode::PAYLOAD_TOO_LARGE;
            Ok(response)
        }
        path if path.starts_with("/bytes/") => {
            let amount = path[7..]
                .parse::<usize>()
                .ok()
                .filter(|n| *n <= 16 * 1024 * 1024);
            let Some(amount) = amount else {
                let mut response = Response::new(OutgoingBody::empty());
                *response.status_mut() = StatusCode::BAD_REQUEST;
                return Ok(response);
            };
            let frames = futures::stream::unfold(amount, |remaining| async move {
                let len = remaining.min(16 * 1024);
                (len != 0).then(|| (Ok(OutgoingFrame::Data(vec![b'x'; len])), remaining - len))
            });
            Ok(Response::new(OutgoingBody::from_stream(
                Some(amount as u64),
                frames,
            )))
        }
        _ => Ok(Response::new(OutgoingBody::full(
            b"hello from kimojio-http1\n",
        ))),
    }
}

async fn serve(socket: OwnedFd, slot: u64) {
    if let Err(error) = kimojio::socket_helpers::update_accept_socket(&socket) {
        eprintln!("connection {slot}: {error}");
        return;
    }
    let config = Config::new(ConnectionId {
        slot,
        generation: 1,
    });
    if let Err(error) = serve_connection(OwnedFdStream::new(socket), config, handle).await {
        eprintln!("connection {slot}: {error}");
    }
}

/// Socket creation and binding are synchronous setup; accepted I/O is native async.
#[kimojio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut bind = "127.0.0.1:0".to_owned();
    let mut connections = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bind" => bind = args.next().ok_or("missing --bind value")?,
            "--connections" => {
                connections = Some(
                    args.next()
                        .ok_or("missing connection count")?
                        .parse::<u64>()?,
                )
            }
            _ => return Err(format!("unknown argument: {arg}").into()),
        }
    }
    let listener = TcpListener::bind(bind)?;
    println!("LISTEN {}", listener.local_addr()?);
    std::io::stdout().flush()?;
    let listener: OwnedFd = listener.into();
    let pool = TaskPool::new(32);
    let mut live = Vec::new();
    let mut slot = 0;
    while connections.is_none_or(|limit| slot < limit) {
        let socket = operations::accept(&listener).await?;
        slot = slot.checked_add(1).ok_or("connection identity exhausted")?;
        let task = pool
            .spawn_task(serve(socket, slot))
            .await
            .map_err(|_| "connection admission cancelled")?;
        live.retain(|handle: &operations::TaskHandle<()>| !handle.is_complete());
        live.push(task);
    }
    for task in live {
        task.await.map_err(|_| "connection task failed")?;
    }
    operations::close(listener).await?;
    Ok(())
}
