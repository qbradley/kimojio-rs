#![no_main]
//! Drives the HTTP/1 chunked body decoder with hostile chunk framing.
//!
//! Chunk framing is the classic desynchronisation surface: a size a peer reads
//! as decimal, signed, or radix-prefixed, an extension hiding a line break, or
//! a size that overflows the accumulator all let two readers disagree about
//! where the body ends. The decoder must never report consuming more bytes
//! than it was handed, or the caller's buffer offset walks off the message.

use arbitrary::Arbitrary;
use kimojio_fsm_http::{Http1ChunkedBody, Http1ChunkedEvent};
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct Body {
    max_body_bytes: u16,
    wire: Vec<u8>,
}

fuzz_target!(|body: Body| {
    let mut decoder = Http1ChunkedBody::new(usize::from(body.max_body_bytes));
    let mut offset = 0usize;
    while let Ok(event) = decoder.next_event(&body.wire[offset..]) {
        let remaining = body.wire.len() - offset;
        let consumed = match event {
            // No more input is coming, so a decoder that still wants some is done.
            Http1ChunkedEvent::NeedInput => break,
            Http1ChunkedEvent::Chunk { consumed, .. } => consumed,
            Http1ChunkedEvent::Complete { consumed } => {
                assert!(
                    consumed <= remaining,
                    "terminal chunk consumed more than it was given"
                );
                break;
            }
        };
        assert!(
            consumed <= remaining,
            "chunk consumed more than it was given"
        );
        if consumed == 0 {
            break;
        }
        offset += consumed;
    }
});
