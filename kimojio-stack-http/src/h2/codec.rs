// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use bytes::{Bytes, BytesMut};
use http::{
    HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode, Uri, Version,
    header::{CONNECTION, HOST, TE, TRANSFER_ENCODING, UPGRADE},
};
use kimojio_stack::RuntimeContext;

use crate::{Body, Error, HttpConfig, StackTransport, Trailers};

use super::{
    CLIENT_PREFACE, ConnectionState, Frame, FrameFlags, FramePayload, FrameType, Header, Setting,
    SettingId, Settings, StreamId, validate_client_preface,
};

pub(super) fn settings_from_config(config: HttpConfig) -> Settings {
    Settings {
        enable_push: false,
        max_header_list_size: config.max_header_bytes.min(u32::MAX as usize) as u32,
        ..Settings::default()
    }
}

pub(super) fn settings_frame(settings: Settings) -> Frame {
    Frame::settings(vec![
        Setting::new(SettingId::HeaderTableSize, settings.header_table_size),
        Setting::new(SettingId::EnablePush, u32::from(settings.enable_push)),
        Setting::new(SettingId::InitialWindowSize, settings.initial_window_size),
        Setting::new(SettingId::MaxFrameSize, settings.max_frame_size),
        Setting::new(SettingId::MaxHeaderListSize, settings.max_header_list_size),
    ])
}

pub(super) fn read_frame(
    cx: &RuntimeContext<'_>,
    transport: &mut StackTransport,
    max_frame_size: u32,
) -> Result<Frame, Error> {
    let mut header = [0_u8; super::frame::FRAME_HEADER_LEN];
    if transport.read_exact_or_eof(cx, &mut header)? != header.len() {
        return Err(Error::Eof);
    }
    let len = ((header[0] as usize) << 16) | ((header[1] as usize) << 8) | header[2] as usize;
    let mut bytes = Vec::with_capacity(header.len() + len);
    bytes.extend_from_slice(&header);
    bytes.resize(header.len() + len, 0);
    if len > 0 && transport.read_exact_or_eof(cx, &mut bytes[header.len()..])? != len {
        return Err(Error::Eof);
    }
    Frame::decode(&bytes, max_frame_size).map(|(frame, _)| frame)
}

pub(super) fn write_frame(
    cx: &RuntimeContext<'_>,
    transport: &mut StackTransport,
    frame: &Frame,
) -> Result<(), Error> {
    let bytes = frame.encode()?;
    transport.write_all(cx, &bytes)
}

pub(super) fn flush_pending(
    cx: &RuntimeContext<'_>,
    transport: &mut StackTransport,
    state: &mut ConnectionState,
) -> Result<(), Error> {
    while let Some(frame) = state.pop_outbound() {
        write_frame(cx, transport, &frame)?;
    }
    Ok(())
}

pub(super) fn write_client_preface(
    cx: &RuntimeContext<'_>,
    transport: &mut StackTransport,
) -> Result<(), Error> {
    transport.write_all(cx, CLIENT_PREFACE)
}

pub(super) fn read_client_preface(
    cx: &RuntimeContext<'_>,
    transport: &mut StackTransport,
    state: &mut ConnectionState,
) -> Result<(), Error> {
    let mut preface = [0_u8; CLIENT_PREFACE.len()];
    if transport.read_exact_or_eof(cx, &mut preface)? != preface.len() {
        return Err(Error::Eof);
    }
    validate_client_preface(&preface)?;
    state.receive_preface(&preface)
}

pub(super) fn collect_header_block(
    cx: &RuntimeContext<'_>,
    transport: &mut StackTransport,
    state: &mut ConnectionState,
    first: Frame,
) -> Result<(StreamId, bool, Bytes), Error> {
    let stream_id = StreamId::new(first.stream_id)?;
    let end_stream = first.flags.contains(FrameFlags::END_STREAM);
    let mut end_headers = first.flags.contains(FrameFlags::END_HEADERS);
    let mut block = BytesMut::new();

    state.track_inbound_frame(&first)?;
    let FramePayload::Headers(bytes) = first.payload else {
        return Err(Error::Protocol("expected HEADERS frame"));
    };
    block.extend_from_slice(&bytes);

    while !end_headers {
        let frame = read_frame(cx, transport, state.local_settings().max_frame_size)?;
        state.track_inbound_frame(&frame)?;
        if frame.frame_type != FrameType::Continuation || frame.stream_id != stream_id.get() {
            return Err(Error::Protocol("invalid CONTINUATION frame"));
        }
        end_headers = frame.flags.contains(FrameFlags::END_HEADERS);
        let FramePayload::Continuation(bytes) = frame.payload else {
            return Err(Error::Protocol("expected CONTINUATION payload"));
        };
        block.extend_from_slice(&bytes);
    }

    Ok((stream_id, end_stream, block.freeze()))
}

pub(super) fn request_headers(request: &Request<Body>) -> Result<Vec<Header>, Error> {
    let mut headers = Vec::with_capacity(request.headers().len() + 4);
    headers.push(Header::new(
        ":method",
        request.method().as_str().as_bytes().to_vec(),
    ));
    headers.push(Header::new(
        ":scheme",
        request
            .uri()
            .scheme_str()
            .unwrap_or("http")
            .as_bytes()
            .to_vec(),
    ));
    if let Some(authority) = request
        .uri()
        .authority()
        .map(|authority| authority.as_str())
        .or_else(|| {
            request
                .headers()
                .get(HOST)
                .and_then(|value| value.to_str().ok())
        })
    {
        headers.push(Header::new(":authority", authority.as_bytes().to_vec()));
    }
    let path = request
        .uri()
        .path_and_query()
        .map(|path| path.as_str())
        .unwrap_or("/");
    headers.push(Header::new(":path", path.as_bytes().to_vec()));
    extend_regular_headers(&mut headers, request.headers(), true)?;
    Ok(headers)
}

pub(super) fn response_headers(response: &Response<Body>) -> Result<Vec<Header>, Error> {
    let mut headers = Vec::with_capacity(response.headers().len() + 1);
    headers.push(Header::new(
        ":status",
        response.status().as_str().as_bytes().to_vec(),
    ));
    extend_regular_headers(&mut headers, response.headers(), false)?;
    Ok(headers)
}

pub(super) fn trailers_headers(trailers: &Trailers) -> Result<Vec<Header>, Error> {
    let mut headers = Vec::with_capacity(trailers.len());
    extend_header_map(&mut headers, trailers.as_map())?;
    Ok(headers)
}

pub(super) fn request_from_headers(
    headers: Vec<Header>,
    body: Body,
) -> Result<Request<Body>, Error> {
    let mut method = None;
    let mut path = None;
    let mut regular = HeaderMap::new();
    let mut saw_regular = false;

    for header in headers {
        let name = header.name.as_ref();
        if name.starts_with(b":") {
            if saw_regular {
                return Err(Error::Protocol("pseudo-header after regular header"));
            }
            match name {
                b":method" => assign_pseudo(&mut method, &header.value)?,
                b":path" => assign_pseudo(&mut path, &header.value)?,
                b":scheme" | b":authority" => {}
                _ => return Err(Error::Protocol("invalid request pseudo-header")),
            }
            continue;
        }
        saw_regular = true;
        append_regular_header(&mut regular, name, &header.value)?;
    }

    let method = method
        .ok_or(Error::Protocol("missing :method"))?
        .parse::<Method>()
        .map_err(|_| Error::Protocol("invalid :method"))?;
    let uri = path
        .ok_or(Error::Protocol("missing :path"))?
        .parse::<Uri>()
        .map_err(|_| Error::Protocol("invalid :path"))?;
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .version(Version::HTTP_2);
    *builder.headers_mut().expect("request builder has headers") = regular;
    builder
        .body(body)
        .map_err(|_| Error::Protocol("failed to build HTTP/2 request"))
}

pub(super) fn response_from_headers(
    headers: Vec<Header>,
    body: Body,
) -> Result<Response<Body>, Error> {
    let mut status = None;
    let mut regular = HeaderMap::new();
    let mut saw_regular = false;

    for header in headers {
        let name = header.name.as_ref();
        if name.starts_with(b":") {
            if saw_regular {
                return Err(Error::Protocol("pseudo-header after regular header"));
            }
            match name {
                b":status" => assign_pseudo(&mut status, &header.value)?,
                _ => return Err(Error::Protocol("invalid response pseudo-header")),
            }
            continue;
        }
        saw_regular = true;
        append_regular_header(&mut regular, name, &header.value)?;
    }

    let status = status
        .ok_or(Error::Protocol("missing :status"))?
        .parse::<u16>()
        .map_err(|_| Error::Protocol("invalid :status"))?;
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(status).map_err(|_| Error::Protocol("invalid :status"))?)
        .version(Version::HTTP_2);
    *builder.headers_mut().expect("response builder has headers") = regular;
    builder
        .body(body)
        .map_err(|_| Error::Protocol("failed to build HTTP/2 response"))
}

pub(super) fn trailers_from_headers(headers: Vec<Header>) -> Result<Trailers, Error> {
    let mut trailers = Trailers::new();
    for header in headers {
        let name = header.name.as_ref();
        if name.starts_with(b":") {
            return Err(Error::Protocol("trailers cannot contain pseudo-headers"));
        }
        let name =
            HeaderName::from_bytes(name).map_err(|_| Error::Protocol("invalid trailer name"))?;
        let value = HeaderValue::from_bytes(&header.value)
            .map_err(|_| Error::Protocol("invalid trailer value"))?;
        trailers.insert(name, value);
    }
    Ok(trailers)
}

fn extend_regular_headers(
    headers: &mut Vec<Header>,
    map: &HeaderMap,
    request: bool,
) -> Result<(), Error> {
    for (name, value) in map {
        if *name == HOST || *name == CONNECTION || *name == UPGRADE || *name == TRANSFER_ENCODING {
            continue;
        }
        if *name == TE && (!request || !value.as_bytes().eq_ignore_ascii_case(b"trailers")) {
            return Err(Error::Protocol("invalid HTTP/2 TE header"));
        }
        headers.push(Header::new(
            name.as_str().as_bytes().to_vec(),
            value.as_bytes().to_vec(),
        ));
    }
    Ok(())
}

fn extend_header_map(headers: &mut Vec<Header>, map: &HeaderMap) -> Result<(), Error> {
    for (name, value) in map {
        if *name == CONNECTION || *name == UPGRADE || *name == TRANSFER_ENCODING {
            return Err(Error::Protocol("connection-specific trailer"));
        }
        headers.push(Header::new(
            name.as_str().as_bytes().to_vec(),
            value.as_bytes().to_vec(),
        ));
    }
    Ok(())
}

fn append_regular_header(map: &mut HeaderMap, name: &[u8], value: &[u8]) -> Result<(), Error> {
    let name = HeaderName::from_bytes(name).map_err(|_| Error::Protocol("invalid header name"))?;
    if name == CONNECTION || name == UPGRADE || name == TRANSFER_ENCODING {
        return Err(Error::Protocol("connection-specific HTTP/2 header"));
    }
    let value =
        HeaderValue::from_bytes(value).map_err(|_| Error::Protocol("invalid header value"))?;
    map.append(name, value);
    Ok(())
}

fn assign_pseudo(slot: &mut Option<String>, value: &[u8]) -> Result<(), Error> {
    if slot.is_some() {
        return Err(Error::Protocol("duplicate pseudo-header"));
    }
    let value = std::str::from_utf8(value).map_err(|_| Error::Protocol("invalid pseudo-header"))?;
    *slot = Some(value.to_owned());
    Ok(())
}
