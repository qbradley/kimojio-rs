// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use bytes::Bytes;
use kimojio_stack::RuntimeContext;

use crate::{Body, BodyBuilder, BodyLimits, Error, LimitKind, StackTransport};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyKind {
    Empty,
    ContentLength(usize),
    Chunked,
    Eof,
}

pub fn read_body(
    cx: &RuntimeContext<'_>,
    transport: &mut StackTransport,
    read_buf: &mut Vec<u8>,
    kind: BodyKind,
    limits: BodyLimits,
) -> Result<Body, Error> {
    match kind {
        BodyKind::Empty => Ok(Body::empty()),
        BodyKind::ContentLength(len) => read_content_length(cx, transport, read_buf, len, limits),
        BodyKind::Chunked => read_chunked(cx, transport, read_buf, limits),
        BodyKind::Eof => read_to_eof(cx, transport, read_buf, limits),
    }
}

pub fn read_body_chunks<F>(
    cx: &RuntimeContext<'_>,
    transport: &mut StackTransport,
    read_buf: &mut Vec<u8>,
    kind: BodyKind,
    limits: BodyLimits,
    mut on_chunk: F,
) -> Result<(), Error>
where
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

pub fn drain_body(
    cx: &RuntimeContext<'_>,
    transport: &mut StackTransport,
    read_buf: &mut Vec<u8>,
    kind: BodyKind,
    limits: BodyLimits,
) -> Result<(), Error> {
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

fn read_content_length(
    cx: &RuntimeContext<'_>,
    transport: &mut StackTransport,
    read_buf: &mut Vec<u8>,
    len: usize,
    limits: BodyLimits,
) -> Result<Body, Error> {
    limits.check_body_len(len)?;
    let bytes = read_exact_bytes(cx, transport, read_buf, len)?;
    Body::from_bytes(bytes, limits)
}

fn read_content_length_chunks<F>(
    cx: &RuntimeContext<'_>,
    transport: &mut StackTransport,
    read_buf: &mut Vec<u8>,
    len: usize,
    on_chunk: &mut F,
) -> Result<(), Error>
where
    F: FnMut(Bytes) -> Result<(), Error>,
{
    let mut remaining = len;
    if !read_buf.is_empty() {
        let amount = read_buf.len().min(remaining);
        on_chunk(Bytes::copy_from_slice(&read_buf[..amount]))?;
        read_buf.drain(..amount);
        remaining -= amount;
    }
    let mut buf = [0_u8; 16 * 1024];
    while remaining != 0 {
        let amount = transport.read(cx, &mut buf[..remaining.min(16 * 1024)])?;
        if amount == 0 {
            return Err(Error::Eof);
        }
        on_chunk(Bytes::copy_from_slice(&buf[..amount]))?;
        remaining -= amount;
    }
    Ok(())
}

fn read_chunked(
    cx: &RuntimeContext<'_>,
    transport: &mut StackTransport,
    read_buf: &mut Vec<u8>,
    limits: BodyLimits,
) -> Result<Body, Error> {
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

fn read_chunked_chunks<F>(
    cx: &RuntimeContext<'_>,
    transport: &mut StackTransport,
    read_buf: &mut Vec<u8>,
    limits: BodyLimits,
    on_chunk: &mut F,
) -> Result<(), Error>
where
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

fn read_to_eof(
    cx: &RuntimeContext<'_>,
    transport: &mut StackTransport,
    read_buf: &mut Vec<u8>,
    limits: BodyLimits,
) -> Result<Body, Error> {
    let mut body = BodyBuilder::new(limits);
    if !read_buf.is_empty() {
        body.append(read_buf)?;
        read_buf.clear();
    }

    let mut buf = [0_u8; 16 * 1024];
    loop {
        let amount = transport.read(cx, &mut buf)?;
        if amount == 0 {
            return Ok(body.finish());
        }
        body.append(&buf[..amount])?;
    }
}

fn read_to_eof_chunks<F>(
    cx: &RuntimeContext<'_>,
    transport: &mut StackTransport,
    read_buf: &mut Vec<u8>,
    limits: BodyLimits,
    on_chunk: &mut F,
) -> Result<(), Error>
where
    F: FnMut(Bytes) -> Result<(), Error>,
{
    let mut total_len = 0_usize;
    if !read_buf.is_empty() {
        total_len += read_buf.len();
        limits.check_body_len(total_len)?;
        on_chunk(Bytes::copy_from_slice(read_buf))?;
        read_buf.clear();
    }
    let mut buf = [0_u8; 16 * 1024];
    loop {
        let amount = transport.read(cx, &mut buf)?;
        if amount == 0 {
            return Ok(());
        }
        total_len = total_len.saturating_add(amount);
        limits.check_body_len(total_len)?;
        on_chunk(Bytes::copy_from_slice(&buf[..amount]))?;
    }
}

fn read_exact_bytes(
    cx: &RuntimeContext<'_>,
    transport: &mut StackTransport,
    read_buf: &mut Vec<u8>,
    len: usize,
) -> Result<Bytes, Error> {
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

fn drain_chunked(
    cx: &RuntimeContext<'_>,
    transport: &mut StackTransport,
    read_buf: &mut Vec<u8>,
    limits: BodyLimits,
) -> Result<(), Error> {
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

fn drain_exact(
    cx: &RuntimeContext<'_>,
    transport: &mut StackTransport,
    read_buf: &mut Vec<u8>,
    mut len: usize,
) -> Result<(), Error> {
    let mut buf = [0_u8; 16 * 1024];
    while len != 0 {
        let amount = len.min(buf.len());
        read_exact_into(cx, transport, read_buf, &mut buf[..amount])?;
        len -= amount;
    }
    Ok(())
}

fn drain_to_eof(
    cx: &RuntimeContext<'_>,
    transport: &mut StackTransport,
    read_buf: &mut Vec<u8>,
    limits: BodyLimits,
) -> Result<(), Error> {
    let mut total_len = read_buf.len();
    limits.check_body_len(total_len)?;
    read_buf.clear();
    let mut buf = [0_u8; 16 * 1024];
    loop {
        let amount = transport.read(cx, &mut buf)?;
        if amount == 0 {
            return Ok(());
        }
        total_len = total_len.saturating_add(amount);
        limits.check_body_len(total_len)?;
    }
}

fn read_exact_into(
    cx: &RuntimeContext<'_>,
    transport: &mut StackTransport,
    read_buf: &mut Vec<u8>,
    mut out: &mut [u8],
) -> Result<(), Error> {
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

fn read_line(
    cx: &RuntimeContext<'_>,
    transport: &mut StackTransport,
    read_buf: &mut Vec<u8>,
    limit: usize,
) -> Result<Vec<u8>, Error> {
    loop {
        if let Some(pos) = find_crlf(read_buf) {
            let mut line = read_buf.drain(..pos + 2).collect::<Vec<_>>();
            line.truncate(pos);
            return Ok(line);
        }
        if read_buf.len() > limit {
            return Err(Error::size_limit(LimitKind::Headers, limit, read_buf.len()));
        }
        let mut buf = [0_u8; 1024];
        let amount = transport.read(cx, &mut buf)?;
        if amount == 0 {
            return Err(Error::Eof);
        }
        read_buf.extend_from_slice(&buf[..amount]);
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
