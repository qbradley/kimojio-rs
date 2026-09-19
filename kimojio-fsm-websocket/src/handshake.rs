use base64::{Engine, engine::general_purpose::STANDARD};
use kimojio_fsm_http1::{self as http, Buffer, Header, RequestHead, Version};
use sha1::{Digest, Sha1};

/// Validated protocol handshake, independent of routing and origin policy.
///
/// No extensions or subprotocol are selected. The application must authorize
/// the resource and origin before calling `accept`.
#[derive(Clone, Debug)]
pub struct Handshake {
    accept: [u8; 28],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HandshakeError {
    BadRequest,
    UnsupportedVersion,
}
impl HandshakeError {
    pub fn status(self) -> u16 {
        match self {
            Self::BadRequest => 400,
            Self::UnsupportedVersion => 426,
        }
    }
    /// Emits an empty HTTP rejection. The owner can request graceful shutdown.
    pub fn respond<B: Buffer, W: AsRef<[u8]>>(
        self,
        server: &mut http::Server<B, W>,
        exchange: http::ExchangeId,
    ) -> Result<(), http::CommandError> {
        let headers = [Header {
            name: "sec-websocket-version",
            value: b"13",
        }];
        server.respond(
            exchange,
            http::Response::new(
                self.status(),
                if self == Self::UnsupportedVersion {
                    "Upgrade Required"
                } else {
                    "Bad Request"
                },
                if self == Self::UnsupportedVersion {
                    &headers
                } else {
                    &[]
                },
                http::BodyLength::Empty,
            ),
        )
    }
}

impl Handshake {
    pub fn validate(head: RequestHead<'_>) -> Result<Self, HandshakeError> {
        use HandshakeError::{BadRequest, UnsupportedVersion};
        if head.method != "GET" || head.version != Version::Http11 {
            return Err(BadRequest);
        }
        let host = singleton(head, "host")?.ok_or(BadRequest)?;
        if host.is_empty() {
            return Err(BadRequest);
        }
        if !has_token(head, "upgrade", b"websocket") || !has_token(head, "connection", b"upgrade") {
            return Err(BadRequest);
        }
        let key = singleton(head, "sec-websocket-key")?.ok_or(BadRequest)?;
        let version = singleton(head, "sec-websocket-version")?.ok_or(BadRequest)?;
        if version.is_empty()
            || version.len() > 3
            || !version.iter().all(u8::is_ascii_digit)
            || (version.len() > 1 && version[0] == b'0')
            || std::str::from_utf8(version)
                .ok()
                .and_then(|v| v.parse::<u8>().ok())
                .is_none()
        {
            return Err(BadRequest);
        }
        let mut decoded = [0; 18];
        if STANDARD
            .decode_slice(key, &mut decoded)
            .map_err(|_| BadRequest)?
            != 16
        {
            return Err(BadRequest);
        }
        singleton(head, "origin")?;
        for header in head.headers {
            if header.name.eq_ignore_ascii_case("transfer-encoding")
                || header.name.eq_ignore_ascii_case("expect")
            {
                return Err(BadRequest);
            }
            if header.name.eq_ignore_ascii_case("content-length")
                && trim(header.value).iter().any(|b| *b != b'0')
            {
                return Err(BadRequest);
            }
            if header.name.eq_ignore_ascii_case("sec-websocket-extensions")
                && !extensions(header.value)
            {
                return Err(BadRequest);
            }
        }
        // Requests may split the protocol list across fields, but tokens must
        // remain unique across the whole list. Header limits bound this scan.
        let protocols = head
            .headers
            .iter()
            .filter(|h| h.name.eq_ignore_ascii_case("sec-websocket-protocol"))
            .flat_map(|h| h.value.split(|b| *b == b',').map(trim));
        for (i, protocol) in protocols.clone().enumerate() {
            if protocol.is_empty()
                || !protocol.iter().copied().all(token)
                || protocols
                    .clone()
                    .take(i)
                    .any(|previous| previous == protocol)
            {
                return Err(BadRequest);
            }
        }
        if version != b"13" {
            return Err(UnsupportedVersion);
        }
        let mut hash = Sha1::new();
        hash.update(key);
        hash.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
        let mut accept = [0; 28];
        STANDARD
            .encode_slice(hash.finalize(), &mut accept)
            .expect("fixed SHA-1 base64 length");
        Ok(Self { accept })
    }
    pub fn accept_value(&self) -> &[u8; 28] {
        &self.accept
    }
    /// Call after the request callback returns, never through callback reentry.
    ///
    /// Keep driving HTTP until `upgrade_ready`, then call `take_upgrade`.
    pub fn accept<B: Buffer, W: AsRef<[u8]>>(
        &self,
        server: &mut http::Server<B, W>,
        exchange: http::ExchangeId,
    ) -> Result<(), http::CommandError> {
        server.accept_upgrade(
            exchange,
            http::UpgradeResponse::new(
                101,
                "Switching Protocols",
                &[
                    Header {
                        name: "upgrade",
                        value: b"websocket",
                    },
                    Header {
                        name: "connection",
                        value: b"Upgrade",
                    },
                    Header {
                        name: "sec-websocket-accept",
                        value: &self.accept,
                    },
                ],
            ),
        )
    }
}

fn trim(value: &[u8]) -> &[u8] {
    let start = value
        .iter()
        .position(|b| !matches!(b, b' ' | b'\t'))
        .unwrap_or(value.len());
    let end = value
        .iter()
        .rposition(|b| !matches!(b, b' ' | b'\t'))
        .map_or(start, |i| i + 1);
    &value[start..end]
}
fn singleton<'a>(head: RequestHead<'a>, name: &str) -> Result<Option<&'a [u8]>, HandshakeError> {
    let mut values = head
        .headers
        .iter()
        .filter(|h| h.name.eq_ignore_ascii_case(name));
    let value = values.next().map(|h| trim(h.value));
    if values.next().is_some() {
        return Err(HandshakeError::BadRequest);
    }
    Ok(value)
}
fn has_token(head: RequestHead<'_>, name: &str, expected: &[u8]) -> bool {
    head.headers
        .iter()
        .filter(|h| h.name.eq_ignore_ascii_case(name))
        .any(|h| {
            h.value
                .split(|b| *b == b',')
                .any(|v| trim(v).eq_ignore_ascii_case(expected))
        })
}
fn token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}
fn extensions(mut bytes: &[u8]) -> bool {
    loop {
        bytes = trim(bytes);
        let length = bytes.iter().take_while(|b| token(**b)).count();
        if length == 0 {
            return false;
        }
        bytes = trim(&bytes[length..]);
        while bytes.first() == Some(&b';') {
            bytes = trim(&bytes[1..]);
            let length = bytes.iter().take_while(|b| token(**b)).count();
            if length == 0 {
                return false;
            }
            bytes = trim(&bytes[length..]);
            if bytes.first() == Some(&b'=') {
                bytes = trim(&bytes[1..]);
                if bytes.first() == Some(&b'"') {
                    bytes = &bytes[1..];
                    let mut count = 0;
                    loop {
                        let Some((&byte, rest)) = bytes.split_first() else {
                            return false;
                        };
                        bytes = rest;
                        if byte == b'"' {
                            break;
                        }
                        let byte = if byte == b'\\' {
                            let Some((&escaped, rest)) = bytes.split_first() else {
                                return false;
                            };
                            bytes = rest;
                            escaped
                        } else {
                            byte
                        };
                        if !token(byte) {
                            return false;
                        }
                        count += 1;
                    }
                    if count == 0 {
                        return false;
                    }
                    bytes = trim(bytes);
                } else {
                    let length = bytes.iter().take_while(|b| token(**b)).count();
                    if length == 0 {
                        return false;
                    }
                    bytes = trim(&bytes[length..]);
                }
            }
        }
        if bytes.is_empty() {
            return true;
        }
        if bytes[0] != b',' {
            return false;
        }
        bytes = &bytes[1..];
    }
}
