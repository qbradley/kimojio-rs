#![no_main]
//! Feeds hostile byte blocks to the HPACK decoder.
//!
//! Blocks are decoded through a single decoder so that dynamic table state
//! carries between them, which is where the interesting failures live: an
//! index computed from one block is applied against a table that a previous
//! block resized or evicted from.

use arbitrary::Arbitrary;
use kimojio_fsm_http::H2HeaderBlockDecoder;
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct Session {
    max_table_size: u16,
    max_header_list_size: u16,
    blocks: Vec<Vec<u8>>,
}

fuzz_target!(|session: Session| {
    let mut decoder = H2HeaderBlockDecoder::new();
    decoder.set_max_table_size(usize::from(session.max_table_size));
    let limit = usize::from(session.max_header_list_size);
    for block in &session.blocks {
        if decoder.try_decode_with_limit(block, limit).is_err() {
            // RFC 7541 section 4.1 makes a decoding failure a connection error,
            // so a real peer stops here rather than decoding the next block
            // against a table of unknown content.
            break;
        }
    }
});
