use crate::*;

#[cfg(test)]
#[path = "codec_tests.rs"]
mod tests;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Framing {
    Empty,
    Fixed(u64),
    Chunked,
    Eof,
}

pub(crate) fn token(bytes: &[u8]) -> bool {
    !bytes.is_empty()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(b))
}

pub(crate) fn has_token(headers: Headers<'_>, name: &str, value: &[u8]) -> bool {
    headers.iter().any(|h| {
        h.name.eq_ignore_ascii_case(name)
            && h.value
                .split(|b| *b == b',')
                .any(|part| trim(part).eq_ignore_ascii_case(value))
    })
}

pub(crate) fn trim(mut bytes: &[u8]) -> &[u8] {
    while matches!(bytes.first(), Some(b' ' | b'\t')) {
        bytes = &bytes[1..];
    }
    while matches!(bytes.last(), Some(b' ' | b'\t')) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

pub(crate) fn framing(headers: Headers<'_>, response: bool) -> Result<Framing, Failure> {
    let mut length = None;
    let mut transfer = false;
    let mut codings = 0;
    for h in headers {
        if h.name.eq_ignore_ascii_case("content-length") {
            for part in h.value.split(|b| *b == b',') {
                let part = trim(part);
                if part.is_empty() || !part.iter().all(u8::is_ascii_digit) {
                    return Err(Failure::Protocol);
                }
                let value = part
                    .iter()
                    .try_fold(0u64, |n, b| {
                        n.checked_mul(10)?.checked_add(u64::from(b - b'0'))
                    })
                    .ok_or(Failure::Protocol)?;
                if length.is_some_and(|prior| prior != value) {
                    return Err(Failure::Protocol);
                }
                length = Some(value);
            }
        } else if h.name.eq_ignore_ascii_case("transfer-encoding") {
            transfer = true;
            for part in h.value.split(|b| *b == b',') {
                let part = trim(part);
                if part.is_empty() {
                    continue;
                }
                if !part.eq_ignore_ascii_case(b"chunked") {
                    return Err(Failure::Protocol);
                }
                codings += 1;
            }
        }
    }
    if transfer {
        if length.is_some() || codings != 1 {
            return Err(Failure::Protocol);
        }
        Ok(Framing::Chunked)
    } else if let Some(length) = length {
        Ok(if length == 0 {
            Framing::Empty
        } else {
            Framing::Fixed(length)
        })
    } else {
        Ok(if response {
            Framing::Eof
        } else {
            Framing::Empty
        })
    }
}

pub(crate) fn persistent(version: Version, headers: Headers<'_>) -> bool {
    !has_token(headers, "connection", b"close")
        && (version == Version::Http11 || has_token(headers, "connection", b"keep-alive"))
}

pub(crate) fn host(headers: Headers<'_>) -> bool {
    let mut hosts = headers
        .iter()
        .filter(|h| h.name.eq_ignore_ascii_case("host"));
    hosts.next().is_some_and(|h| {
        !h.value.is_empty()
            && h.value
                .iter()
                .all(|b| b.is_ascii_graphic() && !b"/\\,@?#".contains(b))
    }) && hosts.next().is_none()
}

pub(crate) fn strict_lines(bytes: &[u8]) -> bool {
    bytes.iter().enumerate().all(|(i, b)| {
        (*b != b'\n' || (i > 0 && bytes[i - 1] == b'\r'))
            && (*b != b'\r' || bytes.get(i + 1) == Some(&b'\n'))
    })
}

// Return the serialized field size while performing the existing validation pass.
fn valid_headers(headers: Headers<'_>, config: &Config) -> Result<usize, CommandError> {
    if headers.len() > config.max_headers {
        return Err(CommandError::Limit);
    }
    let mut bytes = 0usize;
    for h in headers {
        bytes = bytes
            .checked_add(h.name.len())
            .and_then(|n| n.checked_add(h.value.len()))
            .and_then(|n| n.checked_add(4))
            .filter(|n| *n <= config.max_head_bytes)
            .ok_or(CommandError::Limit)?;
        if !token(h.name.as_bytes())
            || h.value
                .iter()
                .any(|b| (*b < 32 && *b != b'\t') || *b == 127)
        {
            return Err(CommandError::InvalidHead);
        }
        if h.name.eq_ignore_ascii_case("content-length")
            || h.name.eq_ignore_ascii_case("transfer-encoding")
            || h.name.eq_ignore_ascii_case("expect")
        {
            return Err(CommandError::InvalidFraming);
        }
    }
    Ok(bytes)
}

fn head_length(limit: usize, parts: &[usize]) -> Result<usize, CommandError> {
    parts.iter().try_fold(0usize, |length, &part| {
        length
            .checked_add(part)
            .filter(|&length| length <= limit)
            .ok_or(CommandError::Limit)
    })
}

struct HeadWriter {
    bytes: Vec<u8>,
    limit: usize,
}

impl HeadWriter {
    fn new(limit: usize) -> Self {
        Self::with_capacity(limit, 0)
    }

    fn with_capacity(limit: usize, capacity: usize) -> Self {
        debug_assert!(capacity <= limit);
        Self {
            bytes: Vec::with_capacity(capacity),
            limit,
        }
    }

    fn append(&mut self, bytes: &[u8]) -> Result<(), CommandError> {
        let len = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .filter(|n| *n <= self.limit)
            .ok_or(CommandError::Limit)?;
        if len > self.bytes.capacity() {
            let capacity = len.saturating_mul(2).min(self.limit);
            self.bytes.reserve_exact(capacity - self.bytes.len());
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn decimal(&mut self, mut value: u64) -> Result<(), CommandError> {
        // A u64 needs at most 20 decimal digits, including zero.
        let mut digits = [0; 20];
        let mut start = digits.len();
        loop {
            start -= 1;
            digits[start] = b'0' + (value % 10) as u8;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        self.append(&digits[start..])
    }
}

/// Encode the chunk prefix in the operation's existing inline storage.
/// The payload and trailing CRLF remain separate scatter/gather slices.
pub(crate) fn encode_chunk_size(mut size: usize, prefix: &mut [u8; 24]) -> usize {
    let digits = (size.max(1).ilog2() / 4 + 1) as usize;
    let mut at = digits;
    loop {
        at -= 1;
        prefix[at] = b"0123456789abcdef"[size & 15];
        size >>= 4;
        if at == 0 {
            break;
        }
    }
    prefix[digits..digits + 2].copy_from_slice(b"\r\n");
    digits + 2
}

fn append_headers(out: &mut HeadWriter, headers: Headers<'_>) -> Result<(), CommandError> {
    for h in headers {
        out.append(h.name.as_bytes())?;
        out.append(b": ")?;
        out.append(h.value)?;
        out.append(b"\r\n")?;
    }
    Ok(())
}

fn append_framing(
    out: &mut HeadWriter,
    body: BodyLength,
    framing: Framing,
) -> Result<(), CommandError> {
    match (body, framing) {
        (_, Framing::Chunked) => out.append(b"transfer-encoding: chunked\r\n"),
        (BodyLength::Known(n), _) => {
            out.append(b"content-length: ")?;
            out.decimal(n)?;
            out.append(b"\r\n")
        }
        (BodyLength::Empty, _) => out.append(b"content-length: 0\r\n"),
        _ => Ok(()),
    }
}

fn framing_length(body: BodyLength, framing: Framing) -> usize {
    match (body, framing) {
        (_, Framing::Chunked) => b"transfer-encoding: chunked\r\n".len(),
        (BodyLength::Known(n), _) => {
            let digits = n.checked_ilog10().unwrap_or(0) as usize + 1;
            b"content-length: ".len() + digits + b"\r\n".len()
        }
        (BodyLength::Empty, _) => b"content-length: 0\r\n".len(),
        _ => 0,
    }
}

fn version(version: Version) -> &'static str {
    match version {
        Version::Http10 => "HTTP/1.0",
        Version::Http11 => "HTTP/1.1",
    }
}

pub(crate) fn encode_request(
    request: Request<'_>,
    config: &Config,
) -> Result<(Vec<u8>, Framing), CommandError> {
    let header_bytes = valid_headers(request.head.headers, config)?;
    if !token(request.head.method.as_bytes())
        || request.head.target.is_empty()
        || !request.head.target.bytes().all(|b| b.is_ascii_graphic())
        || (request.head.version == Version::Http11 && !host(request.head.headers))
    {
        return Err(CommandError::InvalidHead);
    }
    let framing = match request.body {
        BodyLength::Empty | BodyLength::Known(0) => Framing::Empty,
        BodyLength::Known(n) if n <= config.max_body_bytes => Framing::Fixed(n),
        BodyLength::Known(_) => return Err(CommandError::Limit),
        BodyLength::Streaming if request.head.version == Version::Http11 => Framing::Chunked,
        _ => return Err(CommandError::InvalidFraming),
    };
    if request
        .head
        .method
        .len()
        .saturating_add(request.head.target.len())
        > config.max_head_bytes
    {
        return Err(CommandError::Limit);
    }
    let expect = request.expect_continue && framing != Framing::Empty;
    let length = head_length(
        config.max_head_bytes,
        &[
            version(request.head.version).len(),
            request.head.method.len(),
            request.head.target.len(),
            6, // Two spaces, the start-line CRLF, and the final empty line.
            header_bytes,
            framing_length(request.body, framing),
            if expect {
                b"expect: 100-continue\r\n".len()
            } else {
                0
            },
        ],
    )?;
    let mut out = HeadWriter::with_capacity(config.max_head_bytes, length);
    out.append(request.head.method.as_bytes())?;
    out.append(b" ")?;
    out.append(request.head.target.as_bytes())?;
    out.append(b" ")?;
    out.append(version(request.head.version).as_bytes())?;
    out.append(b"\r\n")?;
    append_headers(&mut out, request.head.headers)?;
    append_framing(&mut out, request.body, framing)?;
    if expect {
        out.append(b"expect: 100-continue\r\n")?;
    }
    out.append(b"\r\n")?;
    debug_assert_eq!(out.bytes.len(), length);
    Ok((out.bytes, framing))
}

pub(crate) fn encode_response(
    response: Response<'_>,
    head_method: bool,
    close: bool,
    tunnel: bool,
    config: &Config,
) -> Result<(Vec<u8>, Framing), CommandError> {
    let header_bytes = valid_headers(response.head.headers, config)?;
    if !(100..=599).contains(&response.head.status)
        || response
            .head
            .reason
            .bytes()
            .any(|b| (b < 32 && b != b'\t') || b == 127)
    {
        return Err(CommandError::InvalidHead);
    }
    let body_forbidden = response.head.status < 200 || response.head.status == 204 || tunnel;
    let body_suppressed =
        body_forbidden || response.head.status == 304 || head_method || response.head.status == 205;
    if body_forbidden && response.body != BodyLength::Empty {
        return Err(CommandError::InvalidFraming);
    }
    if response.head.status == 205
        && !matches!(response.body, BodyLength::Empty | BodyLength::Known(0))
    {
        return Err(CommandError::InvalidFraming);
    }
    let framing = if body_suppressed {
        Framing::Empty
    } else {
        match response.body {
            BodyLength::Empty | BodyLength::Known(0) => Framing::Empty,
            BodyLength::Known(n) if n <= config.max_body_bytes => Framing::Fixed(n),
            BodyLength::Known(_) => return Err(CommandError::Limit),
            BodyLength::Streaming if response.head.version == Version::Http11 => Framing::Chunked,
            BodyLength::Streaming => Framing::Eof,
        }
    };
    if response.head.reason.len() > config.max_head_bytes {
        return Err(CommandError::Limit);
    }
    let connection: &[u8] = if (close || framing == Framing::Eof)
        && !has_token(response.head.headers, "connection", b"close")
    {
        b"connection: close\r\n"
    } else if !close
        && !tunnel
        && response.head.version == Version::Http10
        && !has_token(response.head.headers, "connection", b"keep-alive")
        && !has_token(response.head.headers, "connection", b"close")
    {
        b"connection: keep-alive\r\n"
    } else {
        b""
    };
    let length = head_length(
        config.max_head_bytes,
        &[
            version(response.head.version).len(),
            3, // Validated status codes are in 100..=599.
            response.head.reason.len(),
            6, // Two spaces, the status-line CRLF, and the final empty line.
            header_bytes,
            if body_forbidden {
                0
            } else {
                framing_length(response.body, framing)
            },
            connection.len(),
        ],
    )?;
    let mut out = HeadWriter::with_capacity(config.max_head_bytes, length);
    out.append(version(response.head.version).as_bytes())?;
    let status = response.head.status;
    out.append(&[
        b' ',
        b'0' + (status / 100) as u8,
        b'0' + (status / 10 % 10) as u8,
        b'0' + (status % 10) as u8,
        b' ',
    ])?;
    out.append(response.head.reason.as_bytes())?;
    out.append(b"\r\n")?;
    append_headers(&mut out, response.head.headers)?;
    if !body_forbidden {
        append_framing(&mut out, response.body, framing)?;
    }
    out.append(connection)?;
    out.append(b"\r\n")?;
    debug_assert_eq!(out.bytes.len(), length);
    Ok((out.bytes, framing))
}

pub(crate) fn chunk_size(line: &[u8]) -> Result<u64, Failure> {
    let digits = line.iter().take_while(|b| b.is_ascii_hexdigit()).count();
    if digits == 0 {
        return Err(Failure::Protocol);
    }
    let value = line[..digits]
        .iter()
        .try_fold(0u64, |n, b| {
            n.checked_mul(16)?
                .checked_add(u64::from((*b as char).to_digit(16)?))
        })
        .ok_or(Failure::Protocol)?;
    let mut cursor = digits;
    while cursor < line.len() {
        skip_whitespace(line, &mut cursor);
        if line.get(cursor) != Some(&b';') {
            return Err(Failure::Protocol);
        }
        cursor += 1;
        skip_whitespace(line, &mut cursor);
        let start = cursor;
        while line
            .get(cursor)
            .is_some_and(|b| token(std::slice::from_ref(b)))
        {
            cursor += 1;
        }
        if cursor == start {
            return Err(Failure::Protocol);
        }
        let after_name = cursor;
        skip_whitespace(line, &mut cursor);
        if line.get(cursor) != Some(&b'=') {
            cursor = after_name;
            continue;
        }
        cursor += 1;
        skip_whitespace(line, &mut cursor);
        if line.get(cursor) == Some(&b'"') {
            cursor += 1;
            loop {
                match line.get(cursor).copied() {
                    Some(b'"') => {
                        cursor += 1;
                        break;
                    }
                    Some(b'\\') => {
                        cursor += 1;
                        if !line
                            .get(cursor)
                            .is_some_and(|b| *b == b'\t' || (*b >= 32 && *b != 127))
                        {
                            return Err(Failure::Protocol);
                        }
                        cursor += 1;
                    }
                    Some(b) if b == b'\t' || (b >= 32 && b != 127) => cursor += 1,
                    _ => return Err(Failure::Protocol),
                }
            }
        } else {
            let start = cursor;
            while line
                .get(cursor)
                .is_some_and(|b| token(std::slice::from_ref(b)))
            {
                cursor += 1;
            }
            if start == cursor {
                return Err(Failure::Protocol);
            }
        }
    }
    Ok(value)
}

fn skip_whitespace(bytes: &[u8], cursor: &mut usize) {
    while matches!(bytes.get(*cursor), Some(b' ' | b'\t')) {
        *cursor += 1;
    }
}

pub(crate) fn field_present(headers: Headers<'_>, name: &str) -> bool {
    headers.iter().any(|h| h.name.eq_ignore_ascii_case(name))
}

pub(crate) fn expect_continue(headers: Headers<'_>, version: Version) -> Result<bool, Failure> {
    if version == Version::Http10 {
        return Ok(false);
    }
    let mut present = false;
    let mut recognized = false;
    for header in headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case("expect"))
    {
        present = true;
        for value in header.value.split(|byte| *byte == b',') {
            let value = trim(value);
            if value.is_empty() {
                continue;
            }
            if !value.eq_ignore_ascii_case(b"100-continue") {
                return Err(Failure::ExpectationFailed);
            }
            recognized = true;
        }
    }
    if present && !recognized {
        Err(Failure::ExpectationFailed)
    } else {
        Ok(recognized)
    }
}

pub(crate) fn connection_fields(headers: Headers<'_>) -> Result<Vec<u8>, Failure> {
    let mut fields = Vec::new();
    for h in headers
        .iter()
        .filter(|h| h.name.eq_ignore_ascii_case("connection"))
    {
        for value in h.value.split(|b| *b == b',') {
            let value = trim(value);
            if value.is_empty() {
                continue;
            }
            if !token(value) {
                return Err(Failure::Protocol);
            }
            if !fields.is_empty() {
                fields.push(b',');
            }
            fields.extend_from_slice(value);
        }
    }
    Ok(fields)
}

fn protocol(value: &[u8]) -> bool {
    let mut pieces = value.split(|b| *b == b'/');
    token(pieces.next().unwrap_or_default())
        && pieces.next().is_none_or(token)
        && pieces.next().is_none()
}

pub(crate) fn upgrade_protocols(headers: Headers<'_>) -> Result<Option<Vec<u8>>, Failure> {
    if !has_token(headers, "connection", b"upgrade") {
        return Ok(None);
    }
    let mut protocols = Vec::new();
    for h in headers
        .iter()
        .filter(|h| h.name.eq_ignore_ascii_case("upgrade"))
    {
        for value in h.value.split(|b| *b == b',') {
            let value = trim(value);
            if !protocol(value) {
                return Err(Failure::Protocol);
            }
            if !protocols.is_empty() {
                protocols.push(b',');
            }
            protocols.extend_from_slice(value);
        }
    }
    if protocols.is_empty() {
        return Err(Failure::Protocol);
    }
    Ok(Some(protocols))
}

pub(crate) fn valid_upgrade(requested: &[u8], headers: Headers<'_>) -> bool {
    if has_token(headers, "connection", b"close") {
        return false;
    }
    let Ok(Some(selected)) = upgrade_protocols(headers) else {
        return false;
    };
    !selected.contains(&b',')
        && requested
            .split(|b| *b == b',')
            .any(|value| value.eq_ignore_ascii_case(&selected))
}

pub(crate) fn valid_trailer(header: &Header<'_>, connection_fields: &[u8]) -> bool {
    token(header.name.as_bytes())
        && ![
            "content-length",
            "transfer-encoding",
            "host",
            "connection",
            "trailer",
            "upgrade",
            "te",
            "expect",
            "authorization",
            "proxy-authorization",
            "www-authenticate",
            "proxy-authenticate",
            "content-encoding",
            "content-type",
            "content-range",
        ]
        .iter()
        .any(|name| header.name.eq_ignore_ascii_case(name))
        && !connection_fields
            .split(|b| *b == b',')
            .any(|name| header.name.as_bytes().eq_ignore_ascii_case(name))
        && !header
            .value
            .iter()
            .any(|b| (*b < 32 && *b != b'\t') || *b == 127)
}

pub(crate) fn encode_trailers(
    headers: Headers<'_>,
    connection_fields: &[u8],
    config: &Config,
) -> Result<Vec<u8>, CommandError> {
    if headers.len() > config.max_headers {
        return Err(CommandError::Limit);
    }
    if headers.iter().any(|h| !valid_trailer(h, connection_fields)) {
        return Err(CommandError::InvalidHead);
    }
    let mut out = HeadWriter::new(config.max_head_bytes);
    out.append(b"0\r\n")?;
    append_headers(&mut out, headers)?;
    out.append(b"\r\n")?;
    Ok(out.bytes)
}
