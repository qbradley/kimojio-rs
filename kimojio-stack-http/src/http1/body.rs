// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! HTTP/1.1 body read/write helpers.
//!
//! These helpers are public for low-level protocol tests and custom connection
//! code. Higher-level callers should normally use
//! [`crate::http1::ClientConnection`] or [`crate::http1::ServerConnection`].

use bytes::Bytes;
use kimojio_stack::RuntimeSocket;

use crate::{Body, BodyBuilder, BodyLimits, Error, HttpRuntime, LimitKind, RuntimeStackTransport};

use super::read_buf::{DEFAULT_READ_CHUNK_SIZE, read_into_empty, read_more};

const LINE_READ_CHUNK_SIZE: usize = 1024;

/// HTTP/1.1 body framing strategy for a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyKind {
    /// No body is present.
    Empty,
    /// Body has an explicit content length.
    ContentLength(usize),
    /// Body is transfer-encoding chunked.
    Chunked,
    /// Body continues until EOF.
    Eof,
}

/// Reads and buffers a body according to `kind`.
pub fn read_body<R, S>(
    cx: &R,
    transport: &mut RuntimeStackTransport<S>,
    read_buf: &mut Vec<u8>,
    kind: BodyKind,
    limits: BodyLimits,
) -> Result<Body, Error>
where
    R: HttpRuntime<S>,
    S: RuntimeSocket,
{
    match kind {
        BodyKind::Empty => Ok(Body::empty()),
        BodyKind::ContentLength(len) => read_content_length(cx, transport, read_buf, len, limits),
        BodyKind::Chunked => read_chunked(cx, transport, read_buf, limits),
        BodyKind::Eof => read_to_eof(cx, transport, read_buf, limits),
    }
}

/// Reads a body incrementally according to `kind`.
pub fn read_body_chunks<R, S, F>(
    cx: &R,
    transport: &mut RuntimeStackTransport<S>,
    read_buf: &mut Vec<u8>,
    kind: BodyKind,
    limits: BodyLimits,
    mut on_chunk: F,
) -> Result<(), Error>
where
    R: HttpRuntime<S>,
    S: RuntimeSocket,
    F: FnMut(Bytes) -> Result<(), Error>,
{
    match kind {
        BodyKind::Empty => Ok(()),
        BodyKind::ContentLength(len) => {
            limits.check_body_len(len)?;
            read_content_length_chunks(cx, transport, read_buf, len, &mut on_chunk)
        }
        BodyKind::Chunked => read_chunked_chunks(cx, transport, read_buf, limits, &mut on_chunk),
        BodyKind::Eof => read_to_eof_chunks(cx, transport, read_buf, limits, &mut on_chunk),
    }
}

/// Drains a body without delivering bytes to a caller sink.
pub fn drain_body<R, S>(
    cx: &R,
    transport: &mut RuntimeStackTransport<S>,
    read_buf: &mut Vec<u8>,
    kind: BodyKind,
    limits: BodyLimits,
) -> Result<(), Error>
where
    R: HttpRuntime<S>,
    S: RuntimeSocket,
{
    match kind {
        BodyKind::Empty => Ok(()),
        BodyKind::ContentLength(len) => {
            limits.check_body_len(len)?;
            drain_exact(cx, transport, read_buf, len)
        }
        BodyKind::Chunked => drain_chunked(cx, transport, read_buf, limits),
        BodyKind::Eof => drain_to_eof(cx, transport, read_buf, limits),
    }
}

/// Serializes a buffered body as either raw bytes or chunked transfer coding.
pub fn write_body(buf: &mut Vec<u8>, body: &Body, chunked: bool) {
    if chunked {
        if !body.is_empty() {
            buf.extend_from_slice(format!("{:x}\r\n", body.len()).as_bytes());
            buf.extend_from_slice(body.as_bytes());
            buf.extend_from_slice(b"\r\n");
        }
        buf.extend_from_slice(b"0\r\n\r\n");
    } else {
        buf.extend_from_slice(body.as_bytes());
    }
}

fn read_content_length<R, S>(
    cx: &R,
    transport: &mut RuntimeStackTransport<S>,
    read_buf: &mut Vec<u8>,
    len: usize,
    limits: BodyLimits,
) -> Result<Body, Error>
where
    R: HttpRuntime<S>,
    S: RuntimeSocket,
{
    limits.check_body_len(len)?;
    let bytes = read_exact_bytes(cx, transport, read_buf, len)?;
    Body::from_bytes(bytes, limits)
}

fn read_content_length_chunks<R, S, F>(
    cx: &R,
    transport: &mut RuntimeStackTransport<S>,
    read_buf: &mut Vec<u8>,
    len: usize,
    on_chunk: &mut F,
) -> Result<(), Error>
where
    R: HttpRuntime<S>,
    S: RuntimeSocket,
    F: FnMut(Bytes) -> Result<(), Error>,
{
    let mut remaining = len;
    if !read_buf.is_empty() {
        let amount = read_buf.len().min(remaining);
        let chunk = Bytes::copy_from_slice(&read_buf[..amount]);
        read_buf.drain(..amount);
        remaining -= amount;
        on_chunk(chunk)?;
    }
    while remaining != 0 {
        let amount = read_into_empty(
            cx,
            transport,
            read_buf,
            remaining.min(DEFAULT_READ_CHUNK_SIZE),
        )?;
        if amount == 0 {
            return Err(Error::Eof);
        }
        let chunk = Bytes::copy_from_slice(&read_buf[..amount]);
        read_buf.clear();
        on_chunk(chunk)?;
        remaining -= amount;
    }
    Ok(())
}

fn read_chunked<R, S>(
    cx: &R,
    transport: &mut RuntimeStackTransport<S>,
    read_buf: &mut Vec<u8>,
    limits: BodyLimits,
) -> Result<Body, Error>
where
    R: HttpRuntime<S>,
    S: RuntimeSocket,
{
    let mut body = BodyBuilder::new(limits);
    let mut total_len = 0_usize;
    loop {
        let line = read_line(cx, transport, read_buf, 1024)?;
        let size = parse_chunk_size(&line)?;
        if size == 0 {
            loop {
                let trailer = read_line(cx, transport, read_buf, 8 * 1024)?;
                if trailer.is_empty() {
                    return Ok(body.finish());
                }
            }
        }

        total_len = total_len.saturating_add(size);
        limits.check_body_len(total_len)?;
        let chunk = read_exact_bytes(cx, transport, read_buf, size)?;
        body.append(&chunk)?;
        let crlf = read_exact_bytes(cx, transport, read_buf, 2)?;
        if crlf.as_ref() != b"\r\n" {
            return Err(Error::Protocol("chunk data is not followed by CRLF"));
        }
    }
}

fn read_chunked_chunks<R, S, F>(
    cx: &R,
    transport: &mut RuntimeStackTransport<S>,
    read_buf: &mut Vec<u8>,
    limits: BodyLimits,
    on_chunk: &mut F,
) -> Result<(), Error>
where
    R: HttpRuntime<S>,
    S: RuntimeSocket,
    F: FnMut(Bytes) -> Result<(), Error>,
{
    let mut total_len = 0_usize;
    loop {
        let line = read_line(cx, transport, read_buf, 1024)?;
        let size = parse_chunk_size(&line)?;
        if size == 0 {
            loop {
                let trailer = read_line(cx, transport, read_buf, 8 * 1024)?;
                if trailer.is_empty() {
                    return Ok(());
                }
            }
        }
        total_len = total_len.saturating_add(size);
        limits.check_body_len(total_len)?;
        let chunk = read_exact_bytes(cx, transport, read_buf, size)?;
        on_chunk(chunk)?;
        let crlf = read_exact_bytes(cx, transport, read_buf, 2)?;
        if crlf.as_ref() != b"\r\n" {
            return Err(Error::Protocol("chunk data is not followed by CRLF"));
        }
    }
}

fn read_to_eof<R, S>(
    cx: &R,
    transport: &mut RuntimeStackTransport<S>,
    read_buf: &mut Vec<u8>,
    limits: BodyLimits,
) -> Result<Body, Error>
where
    R: HttpRuntime<S>,
    S: RuntimeSocket,
{
    let mut body = BodyBuilder::new(limits);
    if !read_buf.is_empty() {
        body.append(read_buf)?;
        read_buf.clear();
    }

    loop {
        let amount = read_into_empty(cx, transport, read_buf, DEFAULT_READ_CHUNK_SIZE)?;
        if amount == 0 {
            return Ok(body.finish());
        }
        let append = body.append(&read_buf[..amount]);
        read_buf.clear();
        append?;
    }
}

fn read_to_eof_chunks<R, S, F>(
    cx: &R,
    transport: &mut RuntimeStackTransport<S>,
    read_buf: &mut Vec<u8>,
    limits: BodyLimits,
    on_chunk: &mut F,
) -> Result<(), Error>
where
    R: HttpRuntime<S>,
    S: RuntimeSocket,
    F: FnMut(Bytes) -> Result<(), Error>,
{
    let mut total_len = 0_usize;
    if !read_buf.is_empty() {
        total_len += read_buf.len();
        limits.check_body_len(total_len)?;
        let chunk = Bytes::copy_from_slice(read_buf);
        read_buf.clear();
        on_chunk(chunk)?;
    }
    loop {
        let amount = read_into_empty(cx, transport, read_buf, DEFAULT_READ_CHUNK_SIZE)?;
        if amount == 0 {
            return Ok(());
        }
        total_len = total_len.saturating_add(amount);
        if let Err(error) = limits.check_body_len(total_len) {
            read_buf.clear();
            return Err(error);
        }
        let chunk = Bytes::copy_from_slice(&read_buf[..amount]);
        read_buf.clear();
        on_chunk(chunk)?;
    }
}

fn read_exact_bytes<R, S>(
    cx: &R,
    transport: &mut RuntimeStackTransport<S>,
    read_buf: &mut Vec<u8>,
    len: usize,
) -> Result<Bytes, Error>
where
    R: HttpRuntime<S>,
    S: RuntimeSocket,
{
    let mut bytes = Vec::with_capacity(len);
    let buffered = read_buf.len().min(len);
    bytes.extend_from_slice(&read_buf[..buffered]);
    read_buf.drain(..buffered);

    while bytes.len() < len {
        let old_len = bytes.len();
        bytes.resize(len, 0);
        let amount = transport.read(cx, &mut bytes[old_len..])?;
        if amount == 0 {
            return Err(Error::Eof);
        }
        bytes.truncate(old_len + amount);
    }

    Ok(Bytes::from(bytes))
}

fn drain_chunked<R, S>(
    cx: &R,
    transport: &mut RuntimeStackTransport<S>,
    read_buf: &mut Vec<u8>,
    limits: BodyLimits,
) -> Result<(), Error>
where
    R: HttpRuntime<S>,
    S: RuntimeSocket,
{
    let mut total_len = 0_usize;
    loop {
        let line = read_line(cx, transport, read_buf, 1024)?;
        let size = parse_chunk_size(&line)?;
        if size == 0 {
            loop {
                let trailer = read_line(cx, transport, read_buf, 8 * 1024)?;
                if trailer.is_empty() {
                    return Ok(());
                }
            }
        }
        total_len = total_len.saturating_add(size);
        limits.check_body_len(total_len)?;
        drain_exact(cx, transport, read_buf, size)?;
        let mut crlf = [0_u8; 2];
        read_exact_into(cx, transport, read_buf, &mut crlf)?;
        if crlf.as_ref() != b"\r\n" {
            return Err(Error::Protocol("chunk data is not followed by CRLF"));
        }
    }
}

fn drain_exact<R, S>(
    cx: &R,
    transport: &mut RuntimeStackTransport<S>,
    read_buf: &mut Vec<u8>,
    mut len: usize,
) -> Result<(), Error>
where
    R: HttpRuntime<S>,
    S: RuntimeSocket,
{
    while len != 0 {
        let buffered = read_buf.len().min(len);
        read_buf.drain(..buffered);
        len -= buffered;
        if len == 0 {
            break;
        }

        let amount = read_into_empty(cx, transport, read_buf, len.min(DEFAULT_READ_CHUNK_SIZE))?;
        if amount == 0 {
            return Err(Error::Eof);
        }
        read_buf.clear();
        len -= amount;
    }
    Ok(())
}

fn drain_to_eof<R, S>(
    cx: &R,
    transport: &mut RuntimeStackTransport<S>,
    read_buf: &mut Vec<u8>,
    limits: BodyLimits,
) -> Result<(), Error>
where
    R: HttpRuntime<S>,
    S: RuntimeSocket,
{
    let mut total_len = read_buf.len();
    limits.check_body_len(total_len)?;
    read_buf.clear();
    loop {
        let amount = read_into_empty(cx, transport, read_buf, DEFAULT_READ_CHUNK_SIZE)?;
        if amount == 0 {
            return Ok(());
        }
        total_len = total_len.saturating_add(amount);
        read_buf.clear();
        limits.check_body_len(total_len)?;
    }
}

fn read_exact_into<R, S>(
    cx: &R,
    transport: &mut RuntimeStackTransport<S>,
    read_buf: &mut Vec<u8>,
    mut out: &mut [u8],
) -> Result<(), Error>
where
    R: HttpRuntime<S>,
    S: RuntimeSocket,
{
    let buffered = read_buf.len().min(out.len());
    out[..buffered].copy_from_slice(&read_buf[..buffered]);
    read_buf.drain(..buffered);
    out = &mut out[buffered..];

    while !out.is_empty() {
        let amount = transport.read(cx, out)?;
        if amount == 0 {
            return Err(Error::Eof);
        }
        out = &mut out[amount..];
    }
    Ok(())
}

fn read_line<R, S>(
    cx: &R,
    transport: &mut RuntimeStackTransport<S>,
    read_buf: &mut Vec<u8>,
    limit: usize,
) -> Result<Vec<u8>, Error>
where
    R: HttpRuntime<S>,
    S: RuntimeSocket,
{
    loop {
        if let Some(pos) = find_crlf(read_buf) {
            let mut line = read_buf.drain(..pos + 2).collect::<Vec<_>>();
            line.truncate(pos);
            return Ok(line);
        }
        if read_buf.len() > limit {
            return Err(Error::size_limit(LimitKind::Headers, limit, read_buf.len()));
        }
        let amount = read_more(cx, transport, read_buf, LINE_READ_CHUNK_SIZE)?;
        if amount == 0 {
            return Err(Error::Eof);
        }
    }
}

fn parse_chunk_size(line: &[u8]) -> Result<usize, Error> {
    let size = line.split(|&byte| byte == b';').next().unwrap_or(line);
    let size = std::str::from_utf8(size).map_err(|_| Error::Parse("chunk size is not UTF-8"))?;
    usize::from_str_radix(size.trim(), 16).map_err(|_| Error::Parse("invalid chunk size"))
}

fn find_crlf(bytes: &[u8]) -> Option<usize> {
    bytes.windows(2).position(|window| window == b"\r\n")
}
