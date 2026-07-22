use crate::{Http1BodyFraming, Http1Version, ParseHeader};

/// Iterates over non-empty tokens in one comma-list field value.
///
/// RFC 9110 section 5.6.1 permits optional whitespace and empty list elements.
pub(crate) fn comma_list_tokens(value: &[u8]) -> impl Iterator<Item = &[u8]> {
    value
        .split(|byte| *byte == b',')
        .map(|token| token.trim_ascii())
        .filter(|token| !token.is_empty())
}

/// Returns whether one `Connection` field value contains `token`.
///
/// RFC 9110 sections 7.6.1 and 5.6.1 define connection options as
/// case-insensitive comma-list tokens. Optional whitespace and empty list
/// elements are ignored.
pub fn connection_header_value_has_token(value: &[u8], token: &[u8]) -> bool {
    !token.is_empty() && comma_list_tokens(value).any(|option| option.eq_ignore_ascii_case(token))
}

fn http1_message_is_persistent(version: Http1Version, headers: &[ParseHeader<'_>]) -> bool {
    let mut keep_alive = false;
    for header in headers {
        if !header.name.eq_ignore_ascii_case("connection") {
            continue;
        }
        if connection_header_value_has_token(header.value, b"close") {
            return false;
        }
        if connection_header_value_has_token(header.value, b"keep-alive") {
            keep_alive = true;
        }
    }

    matches!(version, Http1Version::Http11) || keep_alive
}

/// Returns whether an HTTP/1 request permits the connection to persist.
///
/// RFC 9112 section 9.3 makes HTTP/1.1 persistent by default and HTTP/1.0
/// persistent only when `Connection: keep-alive` is present. A `close` option
/// always takes precedence. Per RFC 9110 sections 7.6.1 and 5.6.1, connection
/// options are matched as case-insensitive comma-list tokens across every
/// field line, with optional whitespace and empty list elements ignored.
pub fn http1_request_is_persistent(version: Http1Version, headers: &[ParseHeader<'_>]) -> bool {
    http1_message_is_persistent(version, headers)
}

/// Returns whether an HTTP/1 response permits the connection to persist.
///
/// RFC 9112 section 9.3 applies the same version defaults and `Connection`
/// option precedence to responses as requests.
pub(crate) fn http1_response_is_persistent(
    version: Http1Version,
    headers: &[ParseHeader<'_>],
) -> bool {
    http1_message_is_persistent(version, headers)
}

pub(crate) const fn http1_connection_persists(
    request_persists: bool,
    response_writes_body: bool,
    response_framing: Http1BodyFraming,
) -> bool {
    // RFC 9112 sections 6.3 and 9.3 require every message on a persistent
    // connection to have a self-defined length.
    request_persists
        && (!response_writes_body || !matches!(response_framing, Http1BodyFraming::None))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header<'a>(name: &'a str, value: &'a [u8]) -> ParseHeader<'a> {
        ParseHeader { name, value }
    }

    #[test]
    fn derives_request_persistence_from_version_and_connection_options() {
        assert!(http1_request_is_persistent(Http1Version::Http11, &[]));
        assert!(!http1_request_is_persistent(Http1Version::Http10, &[]));
        assert!(http1_request_is_persistent(
            Http1Version::Http10,
            &[header("Connection", b" keep-alive, Upgrade")]
        ));
        assert!(!http1_request_is_persistent(
            Http1Version::Http11,
            &[header("connection", b"upgrade, CLOSE")]
        ));
    }

    #[test]
    fn derives_response_persistence_from_version_and_connection_options() {
        assert!(http1_response_is_persistent(Http1Version::Http11, &[]));
        assert!(!http1_response_is_persistent(Http1Version::Http10, &[]));
        assert!(http1_response_is_persistent(
            Http1Version::Http10,
            &[header("Connection", b"upgrade, keep-alive")]
        ));
        assert!(!http1_response_is_persistent(
            Http1Version::Http11,
            &[
                header("Connection", b"keep-alive"),
                header("connection", b"CLOSE")
            ]
        ));
    }

    #[test]
    fn parses_all_connection_lines_as_token_lists() {
        let headers = [
            header("connection", b", keep-alive,, Upgrade,"),
            header("Connection", b"\tclose\t"),
        ];
        assert!(!http1_request_is_persistent(Http1Version::Http10, &headers));

        for value in [
            b"xclose".as_slice(),
            b"closex",
            b"not-close",
            b"\"close\"",
            b"close;parameter",
        ] {
            assert!(
                http1_request_is_persistent(Http1Version::Http11, &[header("connection", value)]),
                "{value:?} is not the close token"
            );
        }
    }

    #[test]
    fn eof_delimited_response_forces_connection_close() {
        assert!(!http1_connection_persists(
            true,
            true,
            Http1BodyFraming::None
        ));
        assert!(http1_connection_persists(
            true,
            true,
            Http1BodyFraming::ContentLength(0)
        ));
        assert!(http1_connection_persists(
            true,
            false,
            Http1BodyFraming::None
        ));
    }
}
