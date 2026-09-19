use std::{
    io::{Read, Write},
    rc::Rc,
};

use base64::{Engine, engine::general_purpose::STANDARD};
use kimojio::{OwnedFdStream, socket_helpers::create_client_socket};
use kimojio_http1::{
    Config, ConnectionId, Error, IncomingFrame, OutgoingBody, OutgoingFrame, connect,
    http::{HeaderMap, Request},
};

fn headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| (name.as_str().to_owned(), STANDARD.encode(value.as_bytes())))
        .collect()
}

fn report(file: &mut Option<std::fs::File>, record: serde_json::Value) -> Result<(), Error> {
    if let Some(file) = file {
        serde_json::to_writer(&mut *file, &record)
            .map_err(|e| Error::Application(e.to_string()))?;
        writeln!(file)
            .and_then(|()| file.flush())
            .map_err(|e| Error::Application(e.to_string()))?;
    }
    Ok(())
}

#[kimojio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut address = "127.0.0.1:8080".to_owned();
    let mut method = "GET".to_owned();
    let mut path = "/".to_owned();
    let mut body = Vec::new();
    let mut body_file = None;
    let mut result_file = None;
    let mut chunked = false;
    let mut expect_continue = false;
    let mut repeat = 1usize;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--connect" => address = args.next().ok_or("missing address")?,
            "--method" => method = args.next().ok_or("missing method")?,
            "--path" => path = args.next().ok_or("missing path")?,
            "--body" => body = args.next().ok_or("missing body")?.into_bytes(),
            "--body-file" => body_file = Some(args.next().ok_or("missing body file")?),
            "--result-file" => result_file = Some(args.next().ok_or("missing result file")?),
            "--chunked" => chunked = true,
            "--expect-continue" => expect_continue = true,
            "--repeat" | "--count" => repeat = args.next().ok_or("missing repeat")?.parse()?,
            _ => return Err(format!("unknown argument: {arg}").into()),
        }
    }
    if let Some(path) = body_file {
        body.clear();
        std::fs::File::open(path)?
            .take(16 * 1024 * 1024 + 1)
            .read_to_end(&mut body)?;
    }
    if body.len() > 16 * 1024 * 1024 {
        return Err("body exceeds 16 MiB".into());
    }
    let body = Rc::new(body);
    let mut result_file = result_file.map(std::fs::File::create).transpose()?;
    let socket = create_client_socket(&address.parse()?).await?;
    let config = Config::new(ConnectionId {
        slot: 1,
        generation: 1,
    });
    let (mut client, connection) = connect(OwnedFdStream::new(socket), config);
    let control = client.control();
    let application = async move {
        let result = async {
            for _ in 0..repeat {
                let length = (!chunked).then_some(body.len() as u64);
                let source =
                    futures::stream::unfold((body.clone(), 0), |(bytes, start)| async move {
                        if start == bytes.len() {
                            return None;
                        }
                        let end = (start + 16 * 1024).min(bytes.len());
                        let frame = OutgoingFrame::Data(bytes[start..end].to_vec());
                        Some((Ok(frame), (bytes, end)))
                    });
                let outgoing = OutgoingBody::from_stream(length, source);
                let mut request = Request::builder()
                    .method(method.as_str())
                    .uri(&path)
                    .header("host", &address);
                if expect_continue {
                    request = request.header("expect", "100-continue");
                }
                let request = request.body(outgoing).map_err(|_| Error::InvalidMetadata)?;
                let mut response = client.send(request).await?;
                let status = response.status().as_u16();
                let response_headers = headers(response.headers());
                println!("STATUS {status}");
                let mut received = Vec::new();
                let mut trailers = Vec::new();
                while let Some(frame) = response.body_mut().frame().await? {
                    match frame {
                        IncomingFrame::Data(chunk) => {
                            if chunk.len() > (16 * 1024 * 1024usize).saturating_sub(received.len())
                            {
                                return Err(Error::Limit);
                            }
                            received.extend_from_slice(&chunk);
                            std::io::stdout()
                                .write_all(&chunk)
                                .map_err(|e| Error::Application(e.to_string()))?;
                        }
                        IncomingFrame::Trailers(values) => {
                            trailers = headers(&values);
                            for (name, value) in &values {
                                println!(
                                    "TRAILER {name}: {}",
                                    String::from_utf8_lossy(value.as_bytes())
                                );
                            }
                        }
                    }
                }
                report(
                    &mut result_file,
                    serde_json::json!({
                        "status": status,
                        "body_base64": STANDARD.encode(&received),
                        "headers": response_headers,
                        "trailers": trailers,
                        "error": null,
                    }),
                )?;
            }
            client.shutdown().await
        }
        .await;
        if let Err(error) = &result {
            control.abort();
            let _ = report(
                &mut result_file,
                serde_json::json!({"error": error.to_string()}),
            );
        }
        result
    };
    let (result, driver_result) = futures::join!(application, connection.run());
    result?;
    driver_result?;
    Ok(())
}
