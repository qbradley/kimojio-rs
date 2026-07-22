#![no_main]
//! Drives the HTTP/2 server state machine with hostile connection bytes.
//!
//! This is the broadest target in the set: one call reaches frame header
//! parsing, HPACK, settings negotiation, flow-control accounting, and the
//! stream state machine. The client preface is prepended so the fuzzer spends
//! its budget past the handshake instead of rediscovering a fixed 24-byte
//! string, and input is delivered in fuzzer-chosen slice sizes so that frames
//! split across reads exercise the resumption paths.

use arbitrary::Arbitrary;
use kimojio_fsm_http::{CLIENT_PREFACE, H2Server};
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct Connection {
    /// Wire bytes delivered after the preface, in transport-sized reads.
    reads: Vec<Vec<u8>>,
}

fuzz_target!(|connection: Connection| {
    let mut wire = CLIENT_PREFACE.to_vec();
    for read in &connection.reads {
        wire.extend_from_slice(read);
    }

    let mut server = H2Server::default();
    let mut offset = 0usize;
    while offset < wire.len() {
        let Ok((_event, consumed, _output)) = server.accept_event_ref(&wire[offset..]) else {
            // A connection error is terminal; a real peer sends GOAWAY and
            // stops reading rather than resynchronising mid-stream.
            break;
        };
        if consumed == 0 {
            // The decoder needs more bytes than remain, and no more are coming.
            break;
        }
        offset += consumed;
    }
});
