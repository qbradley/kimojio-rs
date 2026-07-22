//! Pins the exact points at which the dynamic table evicts.
//!
//! RFC 7541 section 4.4 evicts until the table is *less than or equal to* the
//! maximum size, so a table sitting exactly at its maximum keeps every entry
//! and one octet more evicts. Two independent code paths implement that
//! comparison — insertion evicts to make room for the incoming entry, and a
//! dynamic table size update evicts to fit a reduced maximum — so both need
//! pinning. A one-octet slack in either leaves the table oversized while every
//! entry still resolves, which no round-trip or decode test notices.

#![cfg(feature = "hpack-test-support")]

use kimojio_fsm_http::H2HeaderBlockDecoder;
use kimojio_fsm_http::hpack_test_support::decoder_table;

/// RFC 7541 section 4.1 charges every entry 32 octets beyond its own bytes.
const ENTRY_OVERHEAD: usize = 32;

/// Two octets of name plus three of value, so 37 octets charged per entry.
const ENTRY_SIZE: usize = 2 + 3 + ENTRY_OVERHEAD;

const OLDEST: (&[u8], &[u8]) = (b"aa", b"bbb");
const NEWEST: (&[u8], &[u8]) = (b"cc", b"ddd");

/// Encodes a dynamic table size update (RFC 7541 section 6.3).
fn table_size_update(size: usize) -> Vec<u8> {
    assert!(size >= 31, "this helper only encodes the multi-byte form");
    let mut remaining = size - 31;
    let mut encoded = vec![0x3f];
    while remaining >= 128 {
        encoded.push((remaining % 128) as u8 | 0x80);
        remaining /= 128;
    }
    encoded.push(remaining as u8);
    encoded
}

/// Encodes a literal field with incremental indexing, which inserts into the
/// dynamic table (RFC 7541 section 6.2.1).
fn indexed_literal((name, value): (&[u8], &[u8])) -> Vec<u8> {
    let mut encoded = vec![0x40];
    encoded.push(name.len() as u8);
    encoded.extend_from_slice(name);
    encoded.push(value.len() as u8);
    encoded.extend_from_slice(value);
    encoded
}

fn owned(entry: (&[u8], &[u8])) -> (Vec<u8>, Vec<u8>) {
    (entry.0.to_vec(), entry.1.to_vec())
}

/// The parts of the table that eviction decides: how much it holds and what.
struct Table {
    size: usize,
    entries: Vec<(Vec<u8>, Vec<u8>)>,
}

fn snapshot(decoder: &H2HeaderBlockDecoder) -> Table {
    let table = decoder_table(decoder);
    Table {
        size: table.size,
        entries: table.entries,
    }
}

/// Caps the table at `max_size` first, so the second insertion is the one that
/// has to make room. Exercises the eviction built into insertion.
fn insert_two_entries_capped_at(max_size: usize) -> Table {
    let mut block = table_size_update(max_size);
    block.extend(indexed_literal(OLDEST));
    block.extend(indexed_literal(NEWEST));

    let mut decoder = H2HeaderBlockDecoder::new();
    let headers = decoder
        .decode_with_limit(&block, usize::MAX)
        .expect("both fields decode regardless of eviction");
    assert_eq!(headers.len(), 2, "both fields are emitted either way");
    assert_eq!(decoder_table(&decoder).max_size, max_size);
    snapshot(&decoder)
}

/// Inserts both entries under the default 4,096-octet maximum, then shrinks the
/// table to `max_size`. Exercises the eviction built into the size update.
fn shrink_two_entries_to(max_size: usize) -> Table {
    let mut inserts = indexed_literal(OLDEST);
    inserts.extend(indexed_literal(NEWEST));

    let mut decoder = H2HeaderBlockDecoder::new();
    decoder
        .decode_with_limit(&inserts, usize::MAX)
        .expect("both fields decode");
    let initial = snapshot(&decoder);
    assert_eq!(
        initial.size,
        2 * ENTRY_SIZE,
        "both entries fit under the default"
    );
    assert_eq!(initial.entries.len(), 2);

    decoder
        .decode_with_limit(&table_size_update(max_size), usize::MAX)
        .expect("a lone size update is a valid block");
    assert_eq!(decoder_table(&decoder).max_size, max_size);
    snapshot(&decoder)
}

#[test]
fn inserting_into_a_table_that_ends_exactly_full_evicts_nothing() {
    let table = insert_two_entries_capped_at(2 * ENTRY_SIZE);

    assert_eq!(table.size, 2 * ENTRY_SIZE);
    assert_eq!(
        table.entries,
        vec![owned(NEWEST), owned(OLDEST)],
        "an insertion landing exactly on the maximum keeps every entry"
    );
}

#[test]
fn inserting_one_octet_past_the_maximum_evicts_exactly_one_entry() {
    let table = insert_two_entries_capped_at(2 * ENTRY_SIZE - 1);

    assert_eq!(
        table.size, ENTRY_SIZE,
        "one octet of overflow evicts the oldest entry and no more"
    );
    assert_eq!(table.entries, vec![owned(NEWEST)]);
}

#[test]
fn shrinking_the_table_to_exactly_its_current_size_evicts_nothing() {
    let table = shrink_two_entries_to(2 * ENTRY_SIZE);

    assert_eq!(table.size, 2 * ENTRY_SIZE);
    assert_eq!(
        table.entries,
        vec![owned(NEWEST), owned(OLDEST)],
        "shrinking to exactly the current size keeps every entry"
    );
}

#[test]
fn shrinking_the_table_one_octet_below_its_size_evicts_exactly_one_entry() {
    let table = shrink_two_entries_to(2 * ENTRY_SIZE - 1);

    assert_eq!(
        table.size, ENTRY_SIZE,
        "one octet of overflow evicts the oldest entry and no more"
    );
    assert_eq!(table.entries, vec![owned(NEWEST)]);
}
