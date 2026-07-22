//! Regression coverage for HTTP/2 header fields larger than the validator's
//! inline buffer.
//!
//! Found by `fuzz/fuzz_targets/h2_server_accept.rs`. The header validator keeps
//! the field name and value in 32-byte inline buffers while counting every byte
//! the peer sent, so the counter can run past the buffer. Two accessors guarded
//! that with `bool::then_some`, whose argument is evaluated eagerly, so the
//! slice was taken before the guard could reject it and any field longer than
//! 32 bytes panicked the connection.
//!
//! Names of that size are ordinary, not hostile: `content-security-policy-
//! report-only` is 35 bytes. These tests therefore pin the semantics as well as
//! the absence of a panic, sweeping the boundary rather than probing one length.

use kimojio_fsm_http::{CLIENT_PREFACE, H2HeaderBlockEncoder, H2RawHeader, H2Request, H2Server};

/// The validator's inline buffer size, and so the boundary under test.
const INLINE_BUFFER_BYTES: usize = 32;

fn settings_frame() -> Vec<u8> {
    vec![0, 0, 0, 0x04, 0x00, 0, 0, 0, 0]
}

fn headers_frame(block: &[u8]) -> Vec<u8> {
    let length = u32::try_from(block.len()).expect("test block fits a frame");
    let mut frame = Vec::with_capacity(9 + block.len());
    frame.extend_from_slice(&length.to_be_bytes()[1..]);
    frame.push(0x01); // HEADERS
    frame.push(0x05); // END_STREAM | END_HEADERS
    frame.extend_from_slice(&1u32.to_be_bytes()); // stream 1
    frame.extend_from_slice(block);
    frame
}

/// Feeds one request to a fresh server and reports whether it was accepted.
fn accept_request(extra: &[(Vec<u8>, Vec<u8>)]) -> Option<H2Request> {
    let mut headers = vec![
        H2RawHeader::new(&b":method"[..], &b"GET"[..]),
        H2RawHeader::new(&b":scheme"[..], &b"https"[..]),
        H2RawHeader::new(&b":path"[..], &b"/"[..]),
        H2RawHeader::new(&b":authority"[..], &b"example.test"[..]),
    ];
    for (name, value) in extra {
        headers.push(H2RawHeader::new(name.clone(), value.clone()));
    }

    let block = H2HeaderBlockEncoder::default().encode(&headers);
    let mut wire = CLIENT_PREFACE.to_vec();
    wire.extend_from_slice(&settings_frame());
    wire.extend_from_slice(&headers_frame(&block));

    H2Server::default()
        .accept(&wire)
        .ok()
        .and_then(|(request, _consumed, _output)| request)
}

fn repeated(byte: u8, len: usize) -> Vec<u8> {
    core::iter::repeat_n(byte, len).collect()
}

#[test]
fn header_names_spanning_the_inline_buffer_are_accepted() {
    // A field name is a token, and length carries no meaning to the validator:
    // one longer than the buffer simply cannot match a name it recognises.
    for len in 1..=(INLINE_BUFFER_BYTES * 2) {
        let name = repeated(b'a', len);
        assert!(
            accept_request(&[(name, b"value".to_vec())]).is_some(),
            "a {len}-byte header name must be accepted"
        );
    }
}

#[test]
fn header_values_spanning_the_inline_buffer_are_accepted() {
    // Unlike a name, an ordinary field's value is never read back out of the
    // inline buffer, so this pins the surrounding value bookkeeping rather than
    // the accessor; `oversized_te_values_are_rejected_rather_than_panicking`
    // covers the value accessor itself.
    for len in 0..=(INLINE_BUFFER_BYTES * 2) {
        let value = repeated(b'v', len);
        assert!(
            accept_request(&[(b"x-probe".to_vec(), value)]).is_some(),
            "a {len}-byte header value must be accepted"
        );
    }
}

#[test]
fn oversized_te_values_are_rejected_rather_than_panicking() {
    // RFC 9113 section 8.2.2 allows exactly one `te` value, so an oversized one
    // must be refused. The point of the sweep is that it is refused by the
    // validator rather than by an out-of-range slice.
    assert!(
        accept_request(&[(b"te".to_vec(), b"trailers".to_vec())]).is_some(),
        "te: trailers is the one permitted value"
    );
    for len in 1..=(INLINE_BUFFER_BYTES * 2) {
        let value = repeated(b't', len);
        assert!(
            accept_request(&[(b"te".to_vec(), value)]).is_none(),
            "a {len}-byte te value must be refused"
        );
    }
}

#[test]
fn pseudo_header_names_spanning_the_inline_buffer_are_rejected() {
    // An unknown pseudo-header is a connection-level malformation, and one too
    // long to store must reach that verdict rather than an out-of-range slice.
    for len in 1..=(INLINE_BUFFER_BYTES * 2) {
        let mut name = vec![b':'];
        name.extend(repeated(b'p', len));
        assert!(
            accept_request(&[(name, b"value".to_vec())]).is_none(),
            "an unknown pseudo-header of {len} bytes must be refused"
        );
    }
}
