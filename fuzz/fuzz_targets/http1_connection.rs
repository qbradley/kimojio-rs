#![no_main]
//! Drives the incremental HTTP/1 connection decoder with hostile bytes.
//!
//! Feeding the decoder in fuzzer-chosen slice sizes matters more here than the
//! byte content alone: the decoder must reach the same framing decision whether
//! a head, a chunk size, or a trailer block arrives whole or split across
//! reads, and a resumption bug is exactly the kind of desynchronisation that
//! lets one message be read as two.

use arbitrary::Arbitrary;
use kimojio_fsm_http::{EMPTY_HEADER, Http1ConnectionDecoder, Http1ConnectionEvent, HttpLimits};
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct Stream {
    /// Wire bytes delivered in transport-sized reads.
    reads: Vec<Vec<u8>>,
}

fuzz_target!(|stream: Stream| {
    let mut wire = Vec::new();
    for read in &stream.reads {
        wire.extend_from_slice(read);
    }

    let mut decoder = Http1ConnectionDecoder::request(HttpLimits::default());
    let mut headers = [EMPTY_HEADER; 64];
    let mut offset = 0usize;
    while let Ok(event) = decoder.next_event(&wire[offset..], &mut headers) {
        let consumed = match event {
            // No more input is coming, so a decoder that still wants some is done.
            Http1ConnectionEvent::NeedInput | Http1ConnectionEvent::Complete => break,
            Http1ConnectionEvent::Head { consumed, .. }
            | Http1ConnectionEvent::Body { consumed, .. }
            | Http1ConnectionEvent::Trailers { consumed, .. }
            | Http1ConnectionEvent::ProtocolSwitch { consumed, .. } => consumed,
        };
        assert!(
            consumed <= wire.len() - offset,
            "decoder reported consuming more than it was given"
        );
        if consumed == 0 {
            break;
        }
        offset += consumed;
    }
});
