//! RFC 7541 Appendix A static table with a development-time perfect hash.
//!
//! Lookup does not allocate and does not depend on extra crates. The hash
//! parameters are ordinary `const` tables. This module does not use macros or
//! compile-time generation.
//!
//! # Regenerating the hash
//!
//! Rebuild `ASSO_VALUES` and `NAME_INDEX` when `STATIC_TABLE` changes. GNU
//! gperf 3.1 computed the current tables. The generated hash is:
//!
//! `hash = name.len() + asso_values[first_byte] + asso_values[last_byte]`
//!
//! 1. Write a gperf input with one keyword per unique name and the first RFC
//!    index for that name:
//!
//!    ```text
//!    struct name_entry { const char *name; int index; };
//!    %%
//!    :authority,1
//!    :method,2
//!    :path,4
//!    ```
//!
//!    A complete input can be built from `STATIC_TABLE` by keeping the first
//!    index of each name.
//!
//! 2. Run:
//!
//!    ```sh
//!    gperf -t -L ANSI-C -C -N lookup_name -K name hpack-static-names.gperf
//!    ```
//!
//! 3. Copy the generated `asso_values` array into `ASSO_VALUES`.
//! 4. Fill `NAME_INDEX` so `NAME_INDEX[hash]` is that RFC index. Use `0` for
//!    unused slots. Size the table to `MAX_HASH_VALUE + 1` from the gperf
//!    output.
//! 5. Run the crate tests. `hash_is_perfect_for_static_names` fails if the
//!    tables no longer distinguish every unique name.
//!
//! `name_hash` uses the shortest and longest names in `STATIC_TABLE`. A name
//! outside that range cannot hit the current gperf tables.

pub(super) const STATIC_TABLE_LEN: usize = 61;

/// gperf `asso_values` for the unique static-table names.
const ASSO_VALUES: [u8; 256] = [
    89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89,
    89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89,
    89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 30, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89,
    89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89,
    89, 15, 89, 0, 50, 0, 45, 10, 15, 0, 89, 10, 25, 10, 30, 89, 25, 89, 0, 25, 15, 0, 15, 40, 89,
    5, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89,
    89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89,
    89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89,
    89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89,
    89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89,
    89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89, 89,
];

/// RFC index of the first static entry for the name that hashes to this slot.
/// `0` means the slot is unused.
const NAME_INDEX: [u8; 89] = [
    0, 0, 0, 0, 0, 50, 32, 51, 42, 0, 0, 53, 31, 30, 34, 0, 27, 40, 21, 43, 0, 35, 52, 39, 59, 58,
    26, 0, 41, 28, 17, 54, 36, 60, 38, 55, 19, 6, 24, 45, 16, 0, 57, 48, 15, 1, 29, 47, 0, 25, 4,
    0, 0, 18, 33, 56, 61, 0, 23, 37, 22, 0, 8, 46, 0, 0, 0, 0, 0, 0, 0, 0, 20, 0, 49, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 2, 44,
];

pub(super) fn find_static_exact(name: &[u8], value: &[u8]) -> Option<usize> {
    let start = find_static_name(name)?;
    STATIC_TABLE[start - 1..]
        .iter()
        .take_while(|(candidate_name, _)| *candidate_name == name)
        .position(|(_, candidate_value)| *candidate_value == value)
        .map(|offset| start + offset)
}

pub(super) fn find_static_name(name: &[u8]) -> Option<usize> {
    let hash = name_hash(name)?;
    let index = usize::from(*NAME_INDEX.get(hash)?);
    if index == 0 {
        return None;
    }
    (STATIC_TABLE[index - 1].0 == name).then_some(index)
}

const fn name_len_bounds() -> (usize, usize) {
    let mut min = usize::MAX;
    let mut max = 0usize;
    let mut i = 0;
    while i < STATIC_TABLE_LEN {
        let len = STATIC_TABLE[i].0.len();
        if len < min {
            min = len;
        }
        if len > max {
            max = len;
        }
        i += 1;
    }
    (min, max)
}

const MIN_NAME_LEN: usize = name_len_bounds().0;
const MAX_NAME_LEN: usize = name_len_bounds().1;

fn name_hash(name: &[u8]) -> Option<usize> {
    let len = name.len();
    if !(MIN_NAME_LEN..=MAX_NAME_LEN).contains(&len) {
        return None;
    }
    Some(
        len + usize::from(ASSO_VALUES[usize::from(name[0])])
            + usize::from(ASSO_VALUES[usize::from(name[len - 1])]),
    )
}

// RFC 7541 Appendix A.
pub(super) const STATIC_TABLE: [(&[u8], &[u8]); STATIC_TABLE_LEN] = [
    (b":authority", b""),
    (b":method", b"GET"),
    (b":method", b"POST"),
    (b":path", b"/"),
    (b":path", b"/index.html"),
    (b":scheme", b"http"),
    (b":scheme", b"https"),
    (b":status", b"200"),
    (b":status", b"204"),
    (b":status", b"206"),
    (b":status", b"304"),
    (b":status", b"400"),
    (b":status", b"404"),
    (b":status", b"500"),
    (b"accept-charset", b""),
    (b"accept-encoding", b"gzip, deflate"),
    (b"accept-language", b""),
    (b"accept-ranges", b""),
    (b"accept", b""),
    (b"access-control-allow-origin", b""),
    (b"age", b""),
    (b"allow", b""),
    (b"authorization", b""),
    (b"cache-control", b""),
    (b"content-disposition", b""),
    (b"content-encoding", b""),
    (b"content-language", b""),
    (b"content-length", b""),
    (b"content-location", b""),
    (b"content-range", b""),
    (b"content-type", b""),
    (b"cookie", b""),
    (b"date", b""),
    (b"etag", b""),
    (b"expect", b""),
    (b"expires", b""),
    (b"from", b""),
    (b"host", b""),
    (b"if-match", b""),
    (b"if-modified-since", b""),
    (b"if-none-match", b""),
    (b"if-range", b""),
    (b"if-unmodified-since", b""),
    (b"last-modified", b""),
    (b"link", b""),
    (b"location", b""),
    (b"max-forwards", b""),
    (b"proxy-authenticate", b""),
    (b"proxy-authorization", b""),
    (b"range", b""),
    (b"referer", b""),
    (b"refresh", b""),
    (b"retry-after", b""),
    (b"server", b""),
    (b"set-cookie", b""),
    (b"strict-transport-security", b""),
    (b"transfer-encoding", b""),
    (b"user-agent", b""),
    (b"vary", b""),
    (b"via", b""),
    (b"www-authenticate", b""),
];

#[cfg(test)]
mod tests {
    use super::{NAME_INDEX, STATIC_TABLE, find_static_exact, find_static_name, name_hash};

    #[test]
    fn every_static_entry_has_exact_and_name_index() {
        for (offset, (name, value)) in STATIC_TABLE.iter().enumerate() {
            let index = offset + 1;
            let first_name = STATIC_TABLE
                .iter()
                .position(|(candidate, _)| candidate == name)
                .map(|pos| pos + 1);
            assert_eq!(find_static_exact(name, value), Some(index));
            assert_eq!(find_static_name(name), first_name);
        }
    }

    #[test]
    fn hash_is_perfect_for_static_names() {
        let mut hashes = [None; NAME_INDEX.len()];
        for (name, _) in STATIC_TABLE {
            let hash = name_hash(name).expect("static name hashes");
            match hashes[hash] {
                None => hashes[hash] = Some(name),
                Some(previous) if previous == name => {}
                Some(previous) => panic!("{name:?} collides with {previous:?} at {hash}"),
            }
        }
    }

    #[test]
    fn unknown_names_and_values_miss() {
        assert_eq!(find_static_name(b""), None);
        assert_eq!(find_static_name(b"ab"), None);
        assert_eq!(find_static_name(b":methodx"), None);
        assert_eq!(find_static_name(b"content-types"), None);
        assert_eq!(find_static_exact(b":method", b"PATCH"), None);
        assert_eq!(find_static_exact(b":status", b"201"), None);
        assert_eq!(find_static_exact(b"cookie", b"a=b"), None);
        assert_eq!(find_static_exact(b"not-a-header", b""), None);
        assert_eq!(find_static_name(b"reuse"), None);
        assert_eq!(find_static_exact(b"reuse", b""), None);
    }
}
