// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use bytes::Bytes;

use crate::Error;

const HEADER_ENTRY_OVERHEAD: usize = 32;
const STATIC_TABLE: &[StaticEntry] = &[
    StaticEntry::new(1, b":authority", b""),
    StaticEntry::new(2, b":method", b"GET"),
    StaticEntry::new(3, b":method", b"POST"),
    StaticEntry::new(4, b":path", b"/"),
    StaticEntry::new(5, b":path", b"/index.html"),
    StaticEntry::new(6, b":scheme", b"http"),
    StaticEntry::new(7, b":scheme", b"https"),
    StaticEntry::new(8, b":status", b"200"),
    StaticEntry::new(9, b":status", b"204"),
    StaticEntry::new(10, b":status", b"206"),
    StaticEntry::new(11, b":status", b"304"),
    StaticEntry::new(12, b":status", b"400"),
    StaticEntry::new(13, b":status", b"404"),
    StaticEntry::new(14, b":status", b"500"),
    StaticEntry::new(28, b"content-length", b""),
    StaticEntry::new(31, b"content-type", b""),
];

struct StaticEntry {
    index: usize,
    name: &'static [u8],
    value: &'static [u8],
}

impl StaticEntry {
    const fn new(index: usize, name: &'static [u8], value: &'static [u8]) -> Self {
        Self { index, name, value }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub name: Bytes,
    pub value: Bytes,
}

impl Header {
    pub fn new(name: impl Into<Bytes>, value: impl Into<Bytes>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

pub(super) struct HeaderBlockEncoder {
    max_table_size: usize,
}

impl HeaderBlockEncoder {
    pub(super) fn new() -> Self {
        Self {
            max_table_size: 4096,
        }
    }

    pub(super) fn set_max_table_size(&mut self, max_table_size: usize) {
        self.max_table_size = max_table_size;
    }

    pub(super) fn encode(&mut self, headers: &[Header]) -> Vec<u8> {
        let _configured_dynamic_table_size = self.max_table_size;
        let mut encoded = Vec::new();
        for header in headers {
            if let Some(index) = find_static_exact(&header.name, &header.value) {
                push_prefixed_integer(&mut encoded, index, 0x7f, 0x80);
                continue;
            }

            let name_index = find_static_name(&header.name).unwrap_or(0);
            push_prefixed_integer(&mut encoded, name_index, 0x0f, 0x00);
            if name_index == 0 {
                push_string(&mut encoded, &header.name);
            }
            push_string(&mut encoded, &header.value);
        }
        encoded
    }
}

impl Default for HeaderBlockEncoder {
    fn default() -> Self {
        Self::new()
    }
}

pub(super) struct HeaderBlockDecoder {
    inner: hpack::Decoder<'static>,
}

impl HeaderBlockDecoder {
    pub(super) fn new() -> Self {
        Self {
            inner: hpack::Decoder::new(),
        }
    }

    pub(super) fn set_max_table_size(&mut self, max_table_size: usize) {
        self.inner.set_max_table_size(max_table_size);
    }

    pub(super) fn decode_with_limit(
        &mut self,
        bytes: &[u8],
        max_header_list_size: usize,
    ) -> Result<Vec<Header>, Error> {
        let headers = self
            .inner
            .decode(bytes)
            .map_err(|_| Error::Protocol("invalid HPACK header block"))?;
        let mut total = 0_usize;
        let mut decoded = Vec::with_capacity(headers.len());
        for (name, value) in headers {
            total = total
                .saturating_add(name.len())
                .saturating_add(value.len())
                .saturating_add(HEADER_ENTRY_OVERHEAD);
            if total > max_header_list_size {
                return Err(Error::Protocol("HPACK header list exceeds limit"));
            }
            decoded.push(Header::new(Bytes::from(name), Bytes::from(value)));
        }
        Ok(decoded)
    }
}

fn find_static_exact(name: &[u8], value: &[u8]) -> Option<usize> {
    STATIC_TABLE
        .iter()
        .find(|entry| entry.name == name && entry.value == value)
        .map(|entry| entry.index)
}

fn find_static_name(name: &[u8]) -> Option<usize> {
    STATIC_TABLE
        .iter()
        .find(|entry| entry.name == name)
        .map(|entry| entry.index)
}

fn push_string(encoded: &mut Vec<u8>, value: &[u8]) {
    push_prefixed_integer(encoded, value.len(), 0x7f, 0x00);
    encoded.extend_from_slice(value);
}

fn push_prefixed_integer(encoded: &mut Vec<u8>, value: usize, prefix_mask: u8, first_bits: u8) {
    let prefix_mask = prefix_mask as usize;
    if value < prefix_mask {
        encoded.push(first_bits | value as u8);
        return;
    }

    encoded.push(first_bits | prefix_mask as u8);
    let mut remaining = value - prefix_mask;
    while remaining >= 128 {
        encoded.push(((remaining & 0x7f) as u8) | 0x80);
        remaining >>= 7;
    }
    encoded.push(remaining as u8);
}

impl Default for HeaderBlockDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hpack_header_block_round_trips() {
        let headers = vec![
            Header::new(":method", "GET"),
            Header::new(":path", "/"),
            Header::new("custom-key", "custom-value"),
        ];
        let mut encoder = HeaderBlockEncoder::new();
        let mut decoder = HeaderBlockDecoder::new();

        let encoded = encoder.encode(&headers);
        let decoded = decoder.decode_with_limit(&encoded, usize::MAX).unwrap();

        assert_eq!(decoded, headers);
    }

    #[test]
    fn hpack_decodes_huffman_encoded_peer_headers() {
        let encoded = [
            0x82, 0x86, 0x84, 0x41, 0x8c, 0xf1, 0xe3, 0xc2, 0xe5, 0xf2, 0x3a, 0x6b, 0xa0, 0xab,
            0x90, 0xf4, 0xff,
        ];
        let mut decoder = HeaderBlockDecoder::new();

        let decoded = decoder.decode_with_limit(&encoded, usize::MAX).unwrap();

        assert_eq!(decoded[0], Header::new(":method", "GET"));
        assert_eq!(decoded[1], Header::new(":scheme", "http"));
        assert_eq!(decoded[2], Header::new(":path", "/"));
        assert_eq!(decoded[3], Header::new(":authority", "www.example.com"));
    }

    #[test]
    fn hpack_enforces_header_list_limit() {
        let headers = vec![Header::new("custom-key", "custom-value")];
        let mut encoder = HeaderBlockEncoder::new();
        let encoded = encoder.encode(&headers);
        let mut decoder = HeaderBlockDecoder::new();

        assert!(decoder.decode_with_limit(&encoded, 53).is_err());
        decoder.decode_with_limit(&encoded, 54).unwrap();
    }

    #[test]
    fn hpack_encoder_respects_zero_peer_table_size_without_dynamic_references() {
        let headers = vec![Header::new("custom-key", "custom-value")];
        let mut encoder = HeaderBlockEncoder::new();
        encoder.set_max_table_size(0);
        let mut peer_decoder = HeaderBlockDecoder::new();
        peer_decoder.set_max_table_size(0);

        let first = encoder.encode(&headers);
        let second = encoder.encode(&headers);

        assert_eq!(
            peer_decoder.decode_with_limit(&first, usize::MAX).unwrap(),
            headers
        );
        assert_eq!(
            peer_decoder.decode_with_limit(&second, usize::MAX).unwrap(),
            headers
        );
    }
}
