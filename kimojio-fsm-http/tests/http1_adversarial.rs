//! Adversarial HTTP/1 request framing coverage.
//!
//! Request smuggling lives in the gap between two peers that read the same
//! bytes and disagree about where one message ends and the next begins. Message
//! *tokenizing* is delegated to `httparse`, so the surface that is genuinely
//! ours is the framing layer: `Transfer-Encoding`, `Content-Length`, the
//! `Host` requirement, and chunk-size parsing. This suite pins the decision we
//! make for each hostile shape so that a future change cannot quietly relax one
//! of them.
//!
//! Cases marked "lenient by design" record a deliberate acceptance rather than
//! an oversight; the comment gives the specification citation.

use kimojio_fsm_http::{
    Http1BodyKind, Http1ChunkedBody, Http1ChunkedEvent, Http1Server, ServerError,
};

#[derive(Debug, Eq, PartialEq)]
enum Outcome {
    /// `parse_request_head` refused the message.
    HeadRejected(ServerError),
    /// The head parsed and the body framing resolved to this kind.
    Framing(Http1BodyKind),
    /// The head parsed but the body framing was refused.
    FramingRejected(ServerError),
}

fn classify(raw: &[u8]) -> Outcome {
    let mut server = Http1Server;
    let mut headers = [httparse::EMPTY_HEADER; 16];
    match server.parse_request_head(raw, &mut headers) {
        Err(error) => Outcome::HeadRejected(error),
        Ok((request, _)) => match Http1Server::request_body_kind(request.headers) {
            Err(error) => Outcome::FramingRejected(error),
            Ok(kind) => Outcome::Framing(kind),
        },
    }
}

/// Builds `POST / HTTP/1.1` carrying `host: a` plus the supplied header block.
fn post(headers: &str) -> Vec<u8> {
    format!("POST / HTTP/1.1\r\nhost: a\r\n{headers}\r\n").into_bytes()
}

fn check(cases: &[(&str, Vec<u8>, Outcome)]) {
    for (name, raw, expected) in cases {
        assert_eq!(&classify(raw), expected, "case: {name}");
    }
}

#[test]
fn transfer_encoding_framing_rejects_smuggling_shapes() {
    check(&[
        (
            "single chunked coding",
            post("transfer-encoding: chunked\r\n"),
            Outcome::Framing(Http1BodyKind::Chunked),
        ),
        // RFC 9112 6.1: "A sender MUST NOT apply the chunked transfer coding
        // more than once to a message body." Peers disagree about whether to
        // reject, strip one layer, or apply both, so we refuse the message.
        (
            "chunked applied twice in one field",
            post("transfer-encoding: chunked, chunked\r\n"),
            Outcome::FramingRejected(ServerError::InvalidRequest),
        ),
        (
            "chunked applied twice across two fields",
            post("transfer-encoding: chunked\r\ntransfer-encoding: chunked\r\n"),
            Outcome::FramingRejected(ServerError::InvalidRequest),
        ),
        // A present-but-empty Transfer-Encoding must not silently fall through
        // to Content-Length: a peer that treats the field's presence as chunked
        // framing would desynchronise from us on the very next message.
        (
            "empty field value alongside content-length",
            post("transfer-encoding: \r\ncontent-length: 5\r\n"),
            Outcome::FramingRejected(ServerError::InvalidRequest),
        ),
        (
            "empty field value alone",
            post("transfer-encoding: \r\n"),
            Outcome::FramingRejected(ServerError::InvalidRequest),
        ),
        (
            "comma-only field value alongside content-length",
            post("transfer-encoding: ,\r\ncontent-length: 5\r\n"),
            Outcome::FramingRejected(ServerError::InvalidRequest),
        ),
        // Empty list elements are tolerated around a single real coding, which
        // the RFC 9110 5.6.1 "#rule" legacy allowance permits.
        (
            "trailing comma after chunked",
            post("transfer-encoding: chunked,\r\n"),
            Outcome::Framing(Http1BodyKind::Chunked),
        ),
        (
            "leading comma before chunked",
            post("transfer-encoding: ,chunked\r\n"),
            Outcome::Framing(Http1BodyKind::Chunked),
        ),
        // Transfer-coding names are case-insensitive tokens.
        (
            "uppercase coding name",
            post("transfer-encoding: CHUNKED\r\n"),
            Outcome::Framing(Http1BodyKind::Chunked),
        ),
        (
            "tab padded coding name",
            post("transfer-encoding: \tchunked\t\r\n"),
            Outcome::Framing(Http1BodyKind::Chunked),
        ),
        // Codings we do not implement are refused rather than ignored, so a
        // body we cannot delimit never reaches the application.
        (
            "chunked with a parameter",
            post("transfer-encoding: chunked;q=1\r\n"),
            Outcome::FramingRejected(ServerError::UnsupportedTransferEncoding),
        ),
        (
            "gzip stacked under chunked",
            post("transfer-encoding: gzip, chunked\r\n"),
            Outcome::FramingRejected(ServerError::UnsupportedTransferEncoding),
        ),
        (
            "chunked stacked under gzip",
            post("transfer-encoding: chunked, gzip\r\n"),
            Outcome::FramingRejected(ServerError::UnsupportedTransferEncoding),
        ),
        (
            "identity coding",
            post("transfer-encoding: identity\r\n"),
            Outcome::FramingRejected(ServerError::UnsupportedTransferEncoding),
        ),
        (
            "chunked then identity across two fields",
            post("transfer-encoding: chunked\r\ntransfer-encoding: identity\r\n"),
            Outcome::FramingRejected(ServerError::UnsupportedTransferEncoding),
        ),
        // RFC 9112 6.3: a message carrying both framing signals "ought to be
        // handled as an error".
        (
            "chunked alongside content-length",
            post("transfer-encoding: chunked\r\ncontent-length: 5\r\n"),
            Outcome::FramingRejected(ServerError::InvalidContentLength),
        ),
    ]);
}

#[test]
fn content_length_framing_pins_accepted_and_rejected_shapes() {
    check(&[
        (
            "plain value",
            post("content-length: 5\r\n"),
            Outcome::Framing(Http1BodyKind::ContentLength(5)),
        ),
        (
            "absent value frames an empty body",
            post(""),
            Outcome::Framing(Http1BodyKind::Empty),
        ),
        // Anything other than DIGIT is refused, so a peer cannot smuggle a
        // second interpretation past a lenient signed or hexadecimal parser.
        (
            "plus sign prefix",
            post("content-length: +5\r\n"),
            Outcome::FramingRejected(ServerError::InvalidContentLength),
        ),
        (
            "negative value",
            post("content-length: -5\r\n"),
            Outcome::FramingRejected(ServerError::InvalidContentLength),
        ),
        (
            "hexadecimal value",
            post("content-length: 0x5\r\n"),
            Outcome::FramingRejected(ServerError::InvalidContentLength),
        ),
        (
            "empty value",
            post("content-length: \r\n"),
            Outcome::FramingRejected(ServerError::InvalidContentLength),
        ),
        (
            "value overflowing usize",
            post("content-length: 99999999999999999999999999\r\n"),
            Outcome::FramingRejected(ServerError::InvalidContentLength),
        ),
        // Duplicates are accepted only when every copy agrees, per RFC 9110 8.6.
        (
            "duplicate fields that agree",
            post("content-length: 5\r\ncontent-length: 5\r\n"),
            Outcome::Framing(Http1BodyKind::ContentLength(5)),
        ),
        (
            "duplicate fields that disagree",
            post("content-length: 5\r\ncontent-length: 6\r\n"),
            Outcome::FramingRejected(ServerError::InvalidContentLength),
        ),
        (
            "comma list whose members agree",
            post("content-length: 5, 5\r\n"),
            Outcome::Framing(Http1BodyKind::ContentLength(5)),
        ),
        (
            "comma list whose members disagree",
            post("content-length: 5, 6\r\n"),
            Outcome::FramingRejected(ServerError::InvalidContentLength),
        ),
        // Lenient by design: RFC 9110 5.5 allows optional whitespace around a
        // field value, and Content-Length is 1*DIGIT with no ban on redundant
        // leading zeroes.
        (
            "surrounding whitespace",
            post("content-length: 5 \r\n"),
            Outcome::Framing(Http1BodyKind::ContentLength(5)),
        ),
        (
            "redundant leading zeroes",
            post("content-length: 005\r\n"),
            Outcome::Framing(Http1BodyKind::ContentLength(5)),
        ),
    ]);
}

#[test]
fn http1_1_requires_exactly_one_host_field() {
    // RFC 9112 3.2: a server "MUST respond with a 400 (Bad Request) status code
    // to any HTTP/1.1 request message that lacks a Host header field and to any
    // request message that contains more than one Host header field line".
    // Ambiguous authority lets a peer route one message to two origins.
    check(&[
        (
            "exactly one host field",
            b"GET / HTTP/1.1\r\nhost: a\r\n\r\n".to_vec(),
            Outcome::Framing(Http1BodyKind::Empty),
        ),
        (
            "no host field",
            b"GET / HTTP/1.1\r\n\r\n".to_vec(),
            Outcome::HeadRejected(ServerError::InvalidRequest),
        ),
        (
            "two host fields that agree",
            b"GET / HTTP/1.1\r\nhost: a\r\nhost: a\r\n\r\n".to_vec(),
            Outcome::HeadRejected(ServerError::InvalidRequest),
        ),
        (
            "two host fields that disagree",
            b"GET / HTTP/1.1\r\nhost: a\r\nhost: b\r\n\r\n".to_vec(),
            Outcome::HeadRejected(ServerError::InvalidRequest),
        ),
        (
            "two host fields differing only in name case",
            b"GET / HTTP/1.1\r\nHost: a\r\nhost: b\r\n\r\n".to_vec(),
            Outcome::HeadRejected(ServerError::InvalidRequest),
        ),
        // The requirement is HTTP/1.1 specific; HTTP/1.0 predates it.
        (
            "no host field on HTTP/1.0",
            b"GET / HTTP/1.0\r\n\r\n".to_vec(),
            Outcome::Framing(Http1BodyKind::Empty),
        ),
    ]);
}

#[test]
fn request_line_and_field_syntax_rejections_hold() {
    check(&[
        (
            "double space in request line",
            b"GET  / HTTP/1.1\r\nhost: a\r\n\r\n".to_vec(),
            Outcome::HeadRejected(ServerError::Parse),
        ),
        (
            "unrecognised minor version",
            b"GET / HTTP/1.11\r\nhost: a\r\n\r\n".to_vec(),
            Outcome::HeadRejected(ServerError::Parse),
        ),
        (
            "space inside the request target",
            b"GET /a b HTTP/1.1\r\nhost: a\r\n\r\n".to_vec(),
            Outcome::HeadRejected(ServerError::Parse),
        ),
        // RFC 9112 5.2 removed obs-fold outside message/http; accepting it lets
        // a peer hide a header line from us or from an intermediary.
        (
            "obs-fold continuation line",
            b"GET / HTTP/1.1\r\nhost: a\r\nx: 1\r\n 2\r\n\r\n".to_vec(),
            Outcome::HeadRejected(ServerError::Parse),
        ),
        (
            "whitespace before the field colon",
            b"GET / HTTP/1.1\r\nhost: a\r\nx : 1\r\n\r\n".to_vec(),
            Outcome::HeadRejected(ServerError::Parse),
        ),
        (
            "empty field name",
            b"GET / HTTP/1.1\r\nhost: a\r\n: 1\r\n\r\n".to_vec(),
            Outcome::HeadRejected(ServerError::Parse),
        ),
        (
            "NUL inside a field value",
            b"GET / HTTP/1.1\r\nhost: a\r\nx: 1\x002\r\n\r\n".to_vec(),
            Outcome::HeadRejected(ServerError::Parse),
        ),
        (
            "bare CR inside a field value",
            b"GET / HTTP/1.1\r\nhost: a\r\nx: 1\r2\r\n\r\n".to_vec(),
            Outcome::HeadRejected(ServerError::Parse),
        ),
        // Lenient by design: RFC 9112 2.2 permits a recipient to "recognize a
        // single LF as a line terminator and ignore any preceding CR". We match
        // the widely deployed `httparse` behaviour here rather than pre-scanning
        // every request head on the hot path.
        (
            "bare LF line terminators",
            b"POST / HTTP/1.1\nhost: a\ncontent-length: 5\n\n".to_vec(),
            Outcome::Framing(Http1BodyKind::ContentLength(5)),
        ),
    ]);
}

#[derive(Debug, Eq, PartialEq)]
enum ChunkOutcome {
    NeedInput,
    Chunk(Vec<u8>),
    Complete,
    Rejected(ServerError),
}

fn chunk(raw: &[u8]) -> ChunkOutcome {
    match Http1ChunkedBody::new(64 * 1024).next_event(raw) {
        Err(error) => ChunkOutcome::Rejected(error),
        Ok(Http1ChunkedEvent::NeedInput) => ChunkOutcome::NeedInput,
        Ok(Http1ChunkedEvent::Chunk { chunk, .. }) => ChunkOutcome::Chunk(chunk.to_vec()),
        Ok(Http1ChunkedEvent::Complete { .. }) => ChunkOutcome::Complete,
    }
}

#[test]
fn chunk_size_parsing_rejects_adversarial_encodings() {
    let cases: &[(&str, &[u8], ChunkOutcome)] = &[
        (
            "plain hexadecimal size",
            b"5\r\nhello\r\n",
            ChunkOutcome::Chunk(b"hello".to_vec()),
        ),
        (
            "uppercase hexadecimal size",
            b"A\r\n0123456789\r\n",
            ChunkOutcome::Chunk(b"0123456789".to_vec()),
        ),
        ("terminal chunk", b"0\r\n\r\n", ChunkOutcome::Complete),
        // A size a peer might read as decimal, signed, or prefixed must not be
        // accepted, or the two of us disagree on where the chunk ends.
        (
            "plus sign prefix",
            b"+5\r\nhello\r\n",
            ChunkOutcome::Rejected(ServerError::Parse),
        ),
        (
            "minus sign prefix",
            b"-5\r\nhello\r\n",
            ChunkOutcome::Rejected(ServerError::Parse),
        ),
        (
            "0x radix prefix",
            b"0x5\r\nhello\r\n",
            ChunkOutcome::Rejected(ServerError::Parse),
        ),
        (
            "trailing garbage after the size",
            b"5abcz\r\nhello\r\n",
            ChunkOutcome::Rejected(ServerError::Parse),
        ),
        (
            "embedded whitespace in the size",
            b"5 5\r\nhello\r\n",
            ChunkOutcome::Rejected(ServerError::Parse),
        ),
        (
            "empty size field",
            b"\r\nhello\r\n",
            ChunkOutcome::Rejected(ServerError::Parse),
        ),
        (
            "size overflowing usize",
            b"FFFFFFFFFFFFFFFFF\r\nhello\r\n",
            ChunkOutcome::Rejected(ServerError::Parse),
        ),
        (
            "bare LF cannot terminate a chunk size",
            b"5\nhello\n",
            ChunkOutcome::NeedInput,
        ),
        // Chunk extensions are ignored, but must not smuggle a line break.
        (
            "chunk extension",
            b"5;name=value\r\nhello\r\n",
            ChunkOutcome::Chunk(b"hello".to_vec()),
        ),
        (
            "chunk extension containing a bare LF",
            b"5;name=va\nlue\r\nhello\r\n",
            ChunkOutcome::Rejected(ServerError::Parse),
        ),
    ];

    for (name, raw, expected) in cases {
        assert_eq!(&chunk(raw), expected, "case: {name}");
    }
}

#[test]
fn chunk_size_honours_the_configured_body_limit() {
    let mut body = Http1ChunkedBody::new(4);
    assert!(matches!(
        body.next_event(b"5\r\nhello\r\n"),
        Err(ServerError::BodyTooLarge { limit: 4, .. })
    ));
}
