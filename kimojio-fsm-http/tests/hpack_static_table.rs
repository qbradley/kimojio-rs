//! Pins every RFC 7541 Appendix A static table entry through the public decoder.
//!
//! A single indexed representation per index reaches every static entry, so one
//! block plus one digest covers the whole table. Round-trip tests cannot: they
//! encode and decode through the same table, so a corrupted entry stays
//! self-consistent and invisible.

use kimojio_fsm_http::H2HeaderBlockDecoder;
use sha2::{Digest, Sha256};

const STATIC_TABLE_LEN: u8 = 61;
const STATIC_TABLE_SHA256: &str =
    "f823911faa3edd39950237294c336f2abdfbe35fda853200d33a0b5e1f47d473";

fn decode_whole_static_table() -> String {
    let block: Vec<u8> = (1..=STATIC_TABLE_LEN).map(|index| 0x80 | index).collect();
    let headers = H2HeaderBlockDecoder::new()
        .decode_with_limit(&block, usize::MAX)
        .expect("every static index decodes");
    assert_eq!(headers.len(), usize::from(STATIC_TABLE_LEN));
    let mut hasher = Sha256::new();
    for header in &headers {
        hasher.update(&header.name);
        hasher.update(b"\0");
        hasher.update(&header.value);
        hasher.update(b"\n");
    }
    format!("{:x}", hasher.finalize())
}

#[test]
fn static_table_entries_match_rfc_7541_appendix_a() {
    assert_eq!(decode_whole_static_table(), STATIC_TABLE_SHA256);
}
