// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use http::{
    HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode, Uri, Version,
    header::{CONNECTION, CONTENT_LENGTH, TRANSFER_ENCODING, UPGRADE},
};
use kimojio_stack::RuntimeContext;

use crate::{Body, BodyLimits, Error, HttpConfig, LimitKind, StackTransport};

use super::body::{BodyKind, read_body, write_body};

#[derive(Debug)]
pub struct IncomingRequest {
    pub request: Request<Body>,
    pub close_after_response: bool,
}

#[derive(Debug)]
pub struct IncomingResponse {
    pub response: Response<Body>,
    pub close_after_response: bool,
}

pub fn read_request(
    cx: &RuntimeContext<'_>,
    transport: &mut StackTransport,
    read_buf: &mut Vec<u8>,
    config: HttpConfig,
) -> Result<Option<IncomingRequest>, Error> {
    let Some(head) = read_head(cx, transport, read_buf, config)? else {
        return Ok(None);
    };
    let mut headers = vec![httparse::EMPTY_HEADER; config.max_header_count];
    let mut parsed = httparse::Request::new(&mut headers);
    match parsed.parse(&head) {
        Ok(httparse::Status::Complete(_)) => {}
        Ok(httparse::Status::Partial) => return Err(Error::Parse("partial HTTP request head")),
        Err(httparse::Error::TooManyHeaders) => {
            return Err(Error::size_limit(
                LimitKind::Headers,
                config.max_header_count,
                config.max_header_count + 1,
            ));
        }
        Err(_) => return Err(Error::Parse("invalid HTTP request head")),
    }

    let method = parsed
        .method
        .ok_or(Error::Parse("missing HTTP request method"))?
        .parse::<Method>()
        .map_err(|_| Error::Parse("invalid HTTP request method"))?;
    let uri = parsed
        .path
        .ok_or(Error::Parse("missing HTTP request target"))?
        .parse::<Uri>()
        .map_err(|_| Error::Parse("invalid HTTP request target"))?;
    let version = parse_version(parsed.version)?;
    let mut header_map = headers_to_map(parsed.headers)?;
    reject_upgrade(&header_map)?;
    let close_after_response = wants_close(version, &header_map);
    let body_kind = request_body_kind(&header_map)?;
    let body = read_body(
        cx,
        transport,
        read_buf,
        body_kind,
        BodyLimits::new(config.max_body_bytes),
    )?;
    if !read_buf.is_empty() {
        return Err(Error::Unsupported("HTTP/1.1 pipelining"));
    }

    let mut builder = Request::builder().method(method).uri(uri).version(version);
    *builder.headers_mut().expect("request builder has headers") = std::mem::take(&mut header_map);
    let request = builder
        .body(body)
        .map_err(|_| Error::Protocol("failed to build HTTP request"))?;
    Ok(Some(IncomingRequest {
        request,
        close_after_response,
    }))
}

pub fn read_response(
    cx: &RuntimeContext<'_>,
    transport: &mut StackTransport,
    read_buf: &mut Vec<u8>,
    config: HttpConfig,
) -> Result<IncomingResponse, Error> {
    let Some(head) = read_head(cx, transport, read_buf, config)? else {
        return Err(Error::Eof);
    };
    let mut headers = vec![httparse::EMPTY_HEADER; config.max_header_count];
    let mut parsed = httparse::Response::new(&mut headers);
    match parsed.parse(&head) {
        Ok(httparse::Status::Complete(_)) => {}
        Ok(httparse::Status::Partial) => return Err(Error::Parse("partial HTTP response head")),
        Err(httparse::Error::TooManyHeaders) => {
            return Err(Error::size_limit(
                LimitKind::Headers,
                config.max_header_count,
                config.max_header_count + 1,
            ));
        }
        Err(_) => return Err(Error::Parse("invalid HTTP response head")),
    }

    let version = parse_version(parsed.version)?;
    let status = StatusCode::from_u16(
        parsed
            .code
            .ok_or(Error::Parse("missing HTTP response status"))?,
    )
    .map_err(|_| Error::Parse("invalid HTTP response status"))?;
    let mut header_map = headers_to_map(parsed.headers)?;
    let mut close_after_response = wants_close(version, &header_map);
    let body_kind = response_body_kind(status, &header_map)?;
    close_after_response |= body_kind == BodyKind::Eof;
    let body = read_body(
        cx,
        transport,
        read_buf,
        body_kind,
        BodyLimits::new(config.max_body_bytes),
    )?;

    let mut builder = Response::builder().status(status).version(version);
    *builder.headers_mut().expect("response builder has headers") = std::mem::take(&mut header_map);
    let response = builder
        .body(body)
        .map_err(|_| Error::Protocol("failed to build HTTP response"))?;
    Ok(IncomingResponse {
        response,
        close_after_response,
    })
}

pub fn write_request(
    cx: &RuntimeContext<'_>,
    transport: &mut StackTransport,
    request: &Request<Body>,
) -> Result<(), Error> {
    let mut bytes = Vec::new();
    write_request_to_vec(&mut bytes, request)?;
    transport.write_all(cx, &bytes)
}

pub fn write_request_to_vec(bytes: &mut Vec<u8>, request: &Request<Body>) -> Result<(), Error> {
    let target = request
        .uri()
        .path_and_query()
        .map(|target| target.as_str())
        .unwrap_or("/");
    bytes.extend_from_slice(request.method().as_str().as_bytes());
    bytes.extend_from_slice(b" ");
    bytes.extend_from_slice(target.as_bytes());
    bytes.extend_from_slice(b" ");
    write_version(bytes, request.version())?;
    bytes.extend_from_slice(b"\r\n");
    write_headers_and_body(bytes, request.headers(), request.body())
}

pub fn write_response(
    cx: &RuntimeContext<'_>,
    transport: &mut StackTransport,
    response: &Response<Body>,
) -> Result<(), Error> {
    let mut bytes = Vec::new();
    write_response_to_vec(&mut bytes, response)?;
    transport.write_all(cx, &bytes)
}

pub fn write_response_to_vec(bytes: &mut Vec<u8>, response: &Response<Body>) -> Result<(), Error> {
    write_version(bytes, response.version())?;
    bytes.extend_from_slice(b" ");
    bytes.extend_from_slice(response.status().as_str().as_bytes());
    if let Some(reason) = response.status().canonical_reason() {
        bytes.extend_from_slice(b" ");
        bytes.extend_from_slice(reason.as_bytes());
    }
    bytes.extend_from_slice(b"\r\n");
    write_headers_and_body(bytes, response.headers(), response.body())
}

fn read_head(
    cx: &RuntimeContext<'_>,
    transport: &mut StackTransport,
    read_buf: &mut Vec<u8>,
    config: HttpConfig,
) -> Result<Option<Vec<u8>>, Error> {
    let limit = config
        .max_start_line_len
        .saturating_add(config.max_header_bytes);
    loop {
        if let Some(end) = find_header_end(read_buf) {
            validate_head_limits(&read_buf[..end], config)?;
            return Ok(Some(read_buf.drain(..end).collect()));
        }
        if read_buf.len() > limit {
            return Err(Error::size_limit(LimitKind::Headers, limit, read_buf.len()));
        }

        fn validate_head_limits(head: &[u8], config: HttpConfig) -> Result<(), Error> {
            let line_end = head
                .windows(2)
                .position(|window| window == b"\r\n")
                .ok_or(Error::Parse("HTTP head is missing start line terminator"))?;
            if line_end > config.max_start_line_len {
                return Err(Error::size_limit(
                    LimitKind::StartLine,
                    config.max_start_line_len,
                    line_end,
                ));
            }

            let header_bytes = head.len().saturating_sub(line_end + 2);
            if header_bytes > config.max_header_bytes {
                return Err(Error::size_limit(
                    LimitKind::Headers,
                    config.max_header_bytes,
                    header_bytes,
                ));
            }

            Ok(())
        }

        let mut buf = vec![0_u8; config.read_buffer_size.max(1)];
        let amount = transport.read(cx, &mut buf)?;
        if amount == 0 {
            if read_buf.is_empty() {
                return Ok(None);
            }
            return Err(Error::Eof);
        }
        read_buf.extend_from_slice(&buf[..amount]);
    }
}

fn headers_to_map(headers: &[httparse::Header<'_>]) -> Result<HeaderMap, Error> {
    let mut map = HeaderMap::new();
    for header in headers {
        let name = HeaderName::from_bytes(header.name.as_bytes())
            .map_err(|_| Error::Parse("invalid HTTP header name"))?;
        let value = HeaderValue::from_bytes(header.value)
            .map_err(|_| Error::Parse("invalid HTTP header value"))?;
        map.append(name, value);
    }
    Ok(map)
}

fn parse_version(version: Option<u8>) -> Result<Version, Error> {
    match version {
        Some(0) => Ok(Version::HTTP_10),
        Some(1) => Ok(Version::HTTP_11),
        Some(_) => Err(Error::Unsupported("unsupported HTTP version")),
        None => Err(Error::Parse("missing HTTP version")),
    }
}

fn write_version(bytes: &mut Vec<u8>, version: Version) -> Result<(), Error> {
    match version {
        Version::HTTP_10 => bytes.extend_from_slice(b"HTTP/1.0"),
        Version::HTTP_11 => bytes.extend_from_slice(b"HTTP/1.1"),
        _ => return Err(Error::Unsupported("unsupported HTTP version")),
    }
    Ok(())
}

fn write_headers_and_body(
    bytes: &mut Vec<u8>,
    headers: &HeaderMap,
    body: &Body,
) -> Result<(), Error> {
    let chunked = match transfer_encoding(headers)? {
        Some(BodyKind::Chunked) => true,
        Some(_) => return Err(Error::Protocol("invalid transfer encoding body kind")),
        None => false,
    };
    for (name, value) in headers {
        bytes.extend_from_slice(name.as_str().as_bytes());
        bytes.extend_from_slice(b": ");
        bytes.extend_from_slice(value.as_bytes());
        bytes.extend_from_slice(b"\r\n");
    }
    if !body.is_empty() && !headers.contains_key(CONTENT_LENGTH) && !chunked {
        bytes.extend_from_slice(b"content-length: ");
        bytes.extend_from_slice(body.len().to_string().as_bytes());
        bytes.extend_from_slice(b"\r\n");
    }
    bytes.extend_from_slice(b"\r\n");
    write_body(bytes, body, chunked);
    Ok(())
}

fn request_body_kind(headers: &HeaderMap) -> Result<BodyKind, Error> {
    if let Some(kind) = transfer_encoding(headers)? {
        ensure_no_content_length(headers)?;
        return Ok(kind);
    }
    content_length(headers).map(|len| len.map_or(BodyKind::Empty, BodyKind::ContentLength))
}

fn response_body_kind(status: StatusCode, headers: &HeaderMap) -> Result<BodyKind, Error> {
    if status.is_informational()
        || status == StatusCode::NO_CONTENT
        || status == StatusCode::NOT_MODIFIED
    {
        return Ok(BodyKind::Empty);
    }
    if let Some(kind) = transfer_encoding(headers)? {
        ensure_no_content_length(headers)?;
        return Ok(kind);
    }
    if let Some(len) = content_length(headers)? {
        return Ok(BodyKind::ContentLength(len));
    }
    Ok(BodyKind::Eof)
}

fn content_length(headers: &HeaderMap) -> Result<Option<usize>, Error> {
    let mut parsed = None;
    for value in headers.get_all(CONTENT_LENGTH) {
        let value = value
            .to_str()
            .map_err(|_| Error::Parse("Content-Length is not ASCII"))?;
        let len = value
            .parse::<usize>()
            .map_err(|_| Error::Parse("invalid Content-Length"))?;
        if let Some(previous) = parsed
            && previous != len
        {
            return Err(Error::Protocol("conflicting Content-Length headers"));
        }
        parsed = Some(len);
    }
    Ok(parsed)
}

fn ensure_no_content_length(headers: &HeaderMap) -> Result<(), Error> {
    if headers.contains_key(CONTENT_LENGTH) {
        Err(Error::Protocol(
            "Content-Length cannot accompany Transfer-Encoding",
        ))
    } else {
        Ok(())
    }
}

fn reject_upgrade(headers: &HeaderMap) -> Result<(), Error> {
    if headers.contains_key(UPGRADE) {
        Err(Error::Unsupported("HTTP/1.1 upgrade"))
    } else {
        Ok(())
    }
}

fn wants_close(version: Version, headers: &HeaderMap) -> bool {
    let mut has_close = false;
    let mut has_keep_alive = false;
    for value in headers.get_all(CONNECTION) {
        for token in value.as_bytes().split(|&byte| byte == b',') {
            let token = trim_ascii(token);
            if token.eq_ignore_ascii_case(b"close") {
                has_close = true;
            }
            if token.eq_ignore_ascii_case(b"keep-alive") {
                has_keep_alive = true;
            }
        }
    }
    has_close || (version == Version::HTTP_10 && !has_keep_alive)
}

fn transfer_encoding(headers: &HeaderMap) -> Result<Option<BodyKind>, Error> {
    let mut saw_encoding = false;
    let mut saw_chunked = false;
    for value in headers.get_all(TRANSFER_ENCODING) {
        for token in value.as_bytes().split(|&byte| byte == b',') {
            let token = trim_ascii(token);
            if token.is_empty() {
                continue;
            }
            saw_encoding = true;
            if token.eq_ignore_ascii_case(b"chunked") {
                saw_chunked = true;
            } else {
                return Err(Error::Unsupported("unsupported Transfer-Encoding"));
            }
        }
    }
    if saw_chunked {
        Ok(Some(BodyKind::Chunked))
    } else if saw_encoding {
        Err(Error::Unsupported("unsupported Transfer-Encoding"))
    } else {
        Ok(None)
    }
}

fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
    while let Some((first, rest)) = bytes.split_first()
        && first.is_ascii_whitespace()
    {
        bytes = rest;
    }
    while let Some((last, rest)) = bytes.split_last()
        && last.is_ascii_whitespace()
    {
        bytes = rest;
    }
    bytes
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|pos| pos + 4)
}
