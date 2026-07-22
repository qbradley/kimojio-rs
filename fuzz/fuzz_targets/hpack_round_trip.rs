#![no_main]
//! Asserts that HPACK encoding round-trips through the decoder.
//!
//! The encoder and decoder share a static table and a Huffman table, so this
//! cannot detect a corrupted shared table (both sides would agree on the same
//! wrong answer). What it does detect is the encoder emitting a block the
//! decoder rejects, and either side mishandling a hostile name or value:
//! lengths that straddle an integer-continuation boundary, bytes that trigger
//! long Huffman codes, and fields sized to land exactly on an eviction edge.

use kimojio_fsm_http::{H2HeaderBlockDecoder, H2HeaderBlockEncoder, H2RawHeader};
use libfuzzer_sys::fuzz_target;

/// Keeps a single field small enough that a case stays fast to replay.
const MAX_FIELD_BYTES: usize = 4 * 1024;

fuzz_target!(|fields: Vec<(Vec<u8>, Vec<u8>)>| {
    let headers: Vec<H2RawHeader> = fields
        .into_iter()
        .take(64)
        .map(|(mut name, mut value)| {
            name.truncate(MAX_FIELD_BYTES);
            value.truncate(MAX_FIELD_BYTES);
            H2RawHeader::new(name, value)
        })
        .collect();

    let mut encoder = H2HeaderBlockEncoder::default();
    let encoded = encoder.encode(&headers);

    let mut decoder = H2HeaderBlockDecoder::new();
    let decoded = decoder
        .decode_with_limit(&encoded, usize::MAX)
        .expect("a block this encoder produced must decode");
    assert_eq!(decoded, headers, "round trip changed the header list");
});
