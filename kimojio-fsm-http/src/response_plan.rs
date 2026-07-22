use crate::{
    Http1Server, Http1Version, HttpLimits, ParseHeader, ServerError,
    persistence::http1_connection_persists,
};

/// How an HTTP/1 response body is framed on the wire.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Http1BodyFraming {
    /// No response body framing header is emitted.
    None,
    /// The response carries the declared content length.
    ContentLength(usize),
    /// The response uses HTTP/1.1 chunked transfer coding.
    Chunked,
}

/// Borrowed request metadata needed to plan an HTTP/1 response.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Http1ResponseContext<'a> {
    /// The request method token.
    pub method: &'a [u8],
    /// The HTTP/1 wire version to preserve in the response.
    pub version: Http1Version,
    /// Whether the caller intends to keep the connection open after the response.
    pub keep_alive: bool,
}

/// Borrowed response metadata and the optional length of its caller-owned body.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Http1ResponseParts<'a> {
    /// The numeric response status.
    pub status: u16,
    /// The response reason phrase.
    pub reason: &'a str,
    /// Caller-supplied response headers in wire order.
    pub headers: &'a [ParseHeader<'a>],
    /// The body length, or `None` when it will be streamed to completion.
    pub body_len: Option<usize>,
}

/// A prepared HTTP/1 response head plus its body-framing decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Http1ResponsePlan {
    head: Vec<u8>,
    writes_body: bool,
    framing: Http1BodyFraming,
    closes_connection: bool,
}

impl Http1ResponsePlan {
    /// Builds a response plan, applying HTTP/1 response semantics.
    pub fn new(
        request: Http1ResponseContext<'_>,
        response: Http1ResponseParts<'_>,
        limits: HttpLimits,
    ) -> Result<Self, ServerError> {
        let writes_body = response_allows_body(response.status) && request.method != b"HEAD";
        let content_length = response_content_length(response.status, response.body_len);
        let framing = if writes_body && response.body_len.is_none() {
            match request.version {
                Http1Version::Http10 => Http1BodyFraming::None,
                Http1Version::Http11 => Http1BodyFraming::Chunked,
            }
        } else {
            content_length.map_or(Http1BodyFraming::None, Http1BodyFraming::ContentLength)
        };
        let closes_connection =
            !http1_connection_persists(request.keep_alive, writes_body, framing);
        let mut headers = response
            .headers
            .iter()
            .copied()
            .filter(|header| !is_framing_header(header.name.as_bytes()))
            .collect::<Vec<_>>();
        if matches!(framing, Http1BodyFraming::Chunked) {
            headers.push(ParseHeader {
                name: "transfer-encoding",
                value: b"chunked",
            });
        }
        if closes_connection {
            headers.push(ParseHeader {
                name: "connection",
                value: b"close",
            });
        } else if matches!(request.version, Http1Version::Http10) {
            headers.push(ParseHeader {
                name: "connection",
                value: b"keep-alive",
            });
        }

        let version = match request.version {
            Http1Version::Http10 => 0,
            Http1Version::Http11 => 1,
        };
        let head = Http1Server::response_head_bytes_with_raw_headers_and_limits(
            version,
            response.status,
            response.reason,
            &headers,
            content_length,
            response.body_len.unwrap_or(0),
            limits,
        )?;

        Ok(Self {
            head,
            writes_body,
            framing,
            closes_connection,
        })
    }

    /// The encoded response head, ready to write.
    pub fn head(&self) -> &[u8] {
        &self.head
    }

    /// Whether the caller must write the response body after the head.
    pub const fn writes_body(&self) -> bool {
        self.writes_body
    }

    /// The body framing this plan selected.
    pub const fn framing(&self) -> Http1BodyFraming {
        self.framing
    }

    /// Whether the connection must close after this response.
    pub const fn closes_connection(&self) -> bool {
        self.closes_connection
    }
}

/// Returns whether a header is controlled by HTTP message framing or connection semantics.
pub fn is_framing_header(name: &[u8]) -> bool {
    [
        b"content-length".as_slice(),
        b"transfer-encoding",
        b"connection",
        b"keep-alive",
        b"proxy-connection",
        b"upgrade",
        b"te",
    ]
    .iter()
    .any(|framing| name.eq_ignore_ascii_case(framing))
}

fn response_allows_body(status: u16) -> bool {
    !matches!(status, 100..=199 | 204 | 304)
}

fn response_content_length(status: u16, body_len: Option<usize>) -> Option<usize> {
    if matches!(status, 100..=199 | 204) {
        None
    } else {
        body_len
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HttpErrorKind;

    fn plan(
        method: &[u8],
        version: Http1Version,
        status: u16,
        headers: &[ParseHeader<'_>],
        body_len: usize,
        limits: HttpLimits,
    ) -> Result<Http1ResponsePlan, ServerError> {
        Http1ResponsePlan::new(
            Http1ResponseContext {
                method,
                version,
                keep_alive: false,
            },
            Http1ResponseParts {
                status,
                reason: "Reason",
                headers,
                body_len: Some(body_len),
            },
            limits,
        )
    }

    #[test]
    fn status_codes_without_bodies_suppress_body_writes() {
        for status in [100, 199, 204, 304] {
            let plan = plan(
                b"GET",
                Http1Version::Http11,
                status,
                &[],
                12,
                HttpLimits::new(),
            )
            .unwrap();
            assert!(!plan.writes_body(), "status {status}");
            if status == 304 {
                assert_eq!(plan.framing(), Http1BodyFraming::ContentLength(12));
                assert!(
                    plan.head()
                        .windows(b"content-length: 12\r\n".len())
                        .any(|bytes| bytes == b"content-length: 12\r\n")
                );
            } else {
                assert_eq!(plan.framing(), Http1BodyFraming::None);
                assert!(
                    !plan
                        .head()
                        .windows(b"content-length:".len())
                        .any(|bytes| bytes == b"content-length:")
                );
            }
        }
    }

    #[test]
    fn head_suppresses_body_but_keeps_content_length() {
        let plan = plan(
            b"HEAD",
            Http1Version::Http11,
            200,
            &[],
            12,
            HttpLimits::new(),
        )
        .unwrap();

        assert!(!plan.writes_body());
        assert_eq!(plan.framing(), Http1BodyFraming::ContentLength(12));
        assert!(
            std::str::from_utf8(plan.head())
                .unwrap()
                .contains("content-length: 12\r\n")
        );
    }

    #[test]
    fn filters_caller_supplied_framing_headers() {
        let headers = [
            ParseHeader {
                name: "Content-Length",
                value: b"999",
            },
            ParseHeader {
                name: "transfer-encoding",
                value: b"chunked",
            },
            ParseHeader {
                name: "connection",
                value: b"keep-alive",
            },
            ParseHeader {
                name: "keep-alive",
                value: b"timeout=5",
            },
            ParseHeader {
                name: "proxy-connection",
                value: b"keep-alive",
            },
            ParseHeader {
                name: "upgrade",
                value: b"websocket",
            },
            ParseHeader {
                name: "te",
                value: b"trailers",
            },
            ParseHeader {
                name: "x-test",
                value: b"yes",
            },
        ];
        let plan = plan(
            b"GET",
            Http1Version::Http11,
            200,
            &headers,
            5,
            HttpLimits::new(),
        )
        .unwrap();
        let head = std::str::from_utf8(plan.head()).unwrap();

        assert!(head.contains("content-length: 5\r\n"));
        assert_eq!(head.matches("content-length:").count(), 1);
        assert!(head.contains("connection: close\r\n"));
        assert!(!head.contains("connection: keep-alive\r\n"));
        assert!(!head.contains("transfer-encoding:"));
        assert!(!head.contains("keep-alive:"));
        assert!(!head.contains("proxy-connection:"));
        assert!(!head.contains("upgrade:"));
        assert!(!head.contains("te:"));
        assert!(head.contains("x-test: yes\r\n"));
    }

    #[test]
    fn preserves_http1_response_version() {
        let http10 = plan(b"GET", Http1Version::Http10, 200, &[], 0, HttpLimits::new()).unwrap();
        let http11 = plan(b"GET", Http1Version::Http11, 200, &[], 0, HttpLimits::new()).unwrap();

        assert!(http10.head().starts_with(b"HTTP/1.0 200 Reason\r\n"));
        assert!(http11.head().starts_with(b"HTTP/1.1 200 Reason\r\n"));
    }

    #[test]
    fn emits_connection_close_for_nonpersistent_response() {
        let plan = plan(b"GET", Http1Version::Http11, 200, &[], 0, HttpLimits::new()).unwrap();
        assert!(plan.closes_connection());
        assert!(
            std::str::from_utf8(plan.head())
                .unwrap()
                .contains("connection: close\r\n")
        );

        let persistent = Http1ResponsePlan::new(
            Http1ResponseContext {
                method: b"GET",
                version: Http1Version::Http11,
                keep_alive: true,
            },
            Http1ResponseParts {
                status: 200,
                reason: "OK",
                headers: &[],
                body_len: Some(0),
            },
            HttpLimits::new(),
        )
        .unwrap();
        assert!(!persistent.closes_connection());
        assert!(
            !persistent
                .head()
                .windows(b"connection: close\r\n".len())
                .any(|bytes| bytes == b"connection: close\r\n")
        );
    }

    #[test]
    fn advertises_persistence_to_http1_0_clients() {
        let persistent = Http1ResponsePlan::new(
            Http1ResponseContext {
                method: b"GET",
                version: Http1Version::Http10,
                keep_alive: true,
            },
            Http1ResponseParts {
                status: 200,
                reason: "OK",
                headers: &[],
                body_len: Some(0),
            },
            HttpLimits::new(),
        )
        .unwrap();

        assert!(!persistent.closes_connection());
        assert!(
            persistent
                .head()
                .windows(b"connection: keep-alive\r\n".len())
                .any(|bytes| bytes == b"connection: keep-alive\r\n")
        );
    }

    #[test]
    fn enforces_header_count_and_head_byte_limits() {
        let count_error = plan(
            b"GET",
            Http1Version::Http11,
            200,
            &[],
            0,
            HttpLimits::new().set_max_headers(1),
        )
        .unwrap_err();
        assert_eq!(count_error.classify().kind(), HttpErrorKind::TooManyHeaders);
        assert_eq!(count_error.classify().limit().unwrap().actual(), Some(2));

        let baseline = plan(b"GET", Http1Version::Http11, 200, &[], 0, HttpLimits::new()).unwrap();
        let byte_error = plan(
            b"GET",
            Http1Version::Http11,
            200,
            &[],
            0,
            HttpLimits::new().set_max_header_bytes(baseline.head().len() - 1),
        )
        .unwrap_err();
        assert_eq!(byte_error.classify().kind(), HttpErrorKind::HeadersTooLarge);
        assert_eq!(
            byte_error.classify().limit().unwrap().actual(),
            Some(baseline.head().len())
        );
    }
}
