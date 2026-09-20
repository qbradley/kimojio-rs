use crate::hpack::{
    AllocationGate, Error, HeaderDecodeOutput, HeaderFieldRef, IndexedHeaderFieldEvictions,
    IndexedHeaderFieldResolver, IndexedHeaderFieldSource,
};

use super::headers::H2RawHeaderRef;

const INLINE_FIELD_CAPACITY: usize = 16;
const INLINE_BYTE_CAPACITY: usize = 512;

#[derive(Clone, Copy, Debug)]
enum CompactFieldSource {
    Arena { start: usize },
    Indexed(IndexedHeaderFieldSource),
}

#[derive(Clone, Copy, Debug)]
struct CompactFieldDescriptor {
    source: CompactFieldSource,
    name_len: usize,
    value_len: usize,
    sensitive: bool,
}

impl Default for CompactFieldDescriptor {
    fn default() -> Self {
        Self {
            source: CompactFieldSource::Arena { start: 0 },
            name_len: 0,
            value_len: 0,
            sensitive: false,
        }
    }
}

#[derive(Debug)]
pub(crate) struct CompactHeaderFields {
    inline_fields: [CompactFieldDescriptor; INLINE_FIELD_CAPACITY],
    overflow_fields: Vec<CompactFieldDescriptor>,
    inline_bytes: [u8; INLINE_BYTE_CAPACITY],
    overflow_bytes: Vec<u8>,
    field_count: usize,
    byte_len: usize,
    fields_spilled: bool,
    bytes_spilled: bool,
    configured_field_limit: usize,
    configured_byte_limit: usize,
    retained_field_limit: usize,
    retained_byte_limit: usize,
}

impl Default for CompactHeaderFields {
    fn default() -> Self {
        Self {
            inline_fields: [CompactFieldDescriptor::default(); INLINE_FIELD_CAPACITY],
            overflow_fields: Vec::new(),
            inline_bytes: [0; INLINE_BYTE_CAPACITY],
            overflow_bytes: Vec::new(),
            field_count: 0,
            byte_len: 0,
            fields_spilled: false,
            bytes_spilled: false,
            configured_field_limit: usize::MAX,
            configured_byte_limit: usize::MAX,
            retained_field_limit: 0,
            retained_byte_limit: 0,
        }
    }
}

impl CompactHeaderFields {
    pub(crate) fn len(&self) -> usize {
        self.field_count
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.field_count == 0
    }

    pub(crate) fn get<'a>(
        &'a self,
        index: usize,
        resolver: IndexedHeaderFieldResolver<'a>,
    ) -> Option<H2RawHeaderRef<'a>> {
        let descriptor = *self.descriptor(index)?;
        let field = match descriptor.source {
            CompactFieldSource::Arena { start } => {
                let bytes = self.bytes();
                let name_end = start.checked_add(descriptor.name_len)?;
                let value_end = name_end.checked_add(descriptor.value_len)?;
                HeaderFieldRef {
                    name: bytes.get(start..name_end)?,
                    value: bytes.get(name_end..value_end)?,
                    sensitive: descriptor.sensitive,
                }
            }
            CompactFieldSource::Indexed(source) => resolver.resolve(source)?,
        };
        if field.name.len() != descriptor.name_len || field.value.len() != descriptor.value_len {
            return None;
        }
        Some(H2RawHeaderRef::new(field.name, field.value).with_sensitive(descriptor.sensitive))
    }

    #[cfg(test)]
    pub(crate) fn iter<'a>(
        &'a self,
        resolver: IndexedHeaderFieldResolver<'a>,
    ) -> CompactHeaderIter<'a> {
        CompactHeaderIter {
            fields: self,
            resolver,
            index: 0,
        }
    }

    pub(crate) fn set_retention_limits(&mut self, field_limit: usize, byte_limit: usize) {
        self.configured_field_limit = field_limit;
        self.configured_byte_limit = byte_limit;
    }

    pub(crate) fn reset(&mut self) {
        self.overflow_fields.clear();
        self.overflow_bytes.clear();
        self.field_count = 0;
        self.byte_len = 0;
        self.fields_spilled = false;
        self.bytes_spilled = false;
    }

    fn descriptor(&self, index: usize) -> Option<&CompactFieldDescriptor> {
        if index >= self.field_count {
            return None;
        }
        if self.fields_spilled {
            self.overflow_fields.get(index)
        } else {
            self.inline_fields.get(index)
        }
    }

    fn descriptor_mut(&mut self, index: usize) -> Option<&mut CompactFieldDescriptor> {
        if index >= self.field_count {
            return None;
        }
        if self.fields_spilled {
            self.overflow_fields.get_mut(index)
        } else {
            self.inline_fields.get_mut(index)
        }
    }

    fn bytes(&self) -> &[u8] {
        if self.bytes_spilled {
            &self.overflow_bytes[..self.byte_len]
        } else {
            &self.inline_bytes[..self.byte_len]
        }
    }

    fn append_bytes(&mut self, bytes: &[u8]) {
        if self.bytes_spilled {
            self.overflow_bytes.extend_from_slice(bytes);
        } else {
            let end = self.byte_len + bytes.len();
            self.inline_bytes[self.byte_len..end].copy_from_slice(bytes);
        }
        self.byte_len += bytes.len();
    }

    fn prepare_field_storage(
        &mut self,
        index: usize,
        field_bytes: usize,
        allocations: &mut AllocationGate,
    ) -> Result<(bool, bool), Error> {
        let field_count = index.checked_add(1).ok_or(Error::FieldSizeOverflow)?;
        debug_assert!(field_count <= self.retained_field_limit);

        let spill_fields = self.fields_spilled || field_count > INLINE_FIELD_CAPACITY;
        if spill_fields {
            let additional = field_count.saturating_sub(self.overflow_fields.len());
            allocations.try_reserve_decoded_output(&mut self.overflow_fields, additional)?;
        }

        let spill_bytes = self.prepare_byte_storage(field_bytes, allocations)?;

        Ok((spill_fields, spill_bytes))
    }

    fn prepare_byte_storage(
        &mut self,
        additional_bytes: usize,
        allocations: &mut AllocationGate,
    ) -> Result<bool, Error> {
        let byte_len = self
            .byte_len
            .checked_add(additional_bytes)
            .ok_or(Error::FieldSizeOverflow)?;
        debug_assert!(byte_len <= self.retained_byte_limit);
        let spill_bytes = self.bytes_spilled || byte_len > INLINE_BYTE_CAPACITY;
        if spill_bytes {
            let additional = byte_len.saturating_sub(self.overflow_bytes.len());
            allocations.try_reserve_decoded_output(&mut self.overflow_bytes, additional)?;
        }
        Ok(spill_bytes)
    }

    fn commit_spills(&mut self, spill_fields: bool, spill_bytes: bool) {
        if spill_fields && !self.fields_spilled {
            self.overflow_fields
                .extend_from_slice(&self.inline_fields[..self.field_count]);
            self.fields_spilled = true;
        }
        if spill_bytes && !self.bytes_spilled {
            self.overflow_bytes
                .extend_from_slice(&self.inline_bytes[..self.byte_len]);
            self.bytes_spilled = true;
        }
    }

    #[cfg(test)]
    fn overflow_capacities(&self) -> (usize, usize) {
        (
            self.overflow_fields.capacity(),
            self.overflow_bytes.capacity(),
        )
    }

    #[cfg(test)]
    fn uses_overflow(&self) -> (bool, bool) {
        (self.fields_spilled, self.bytes_spilled)
    }

    #[cfg(test)]
    fn arena_byte_len(&self) -> usize {
        self.byte_len
    }
}

impl HeaderDecodeOutput for CompactHeaderFields {
    fn begin_block(&mut self, max_header_list_size: usize) {
        self.retained_field_limit = max_header_list_size / 32;
        self.retained_byte_limit = max_header_list_size;
        let field_retention_limit = self.retained_field_limit.min(self.configured_field_limit);
        let byte_retention_limit = self.retained_byte_limit.min(self.configured_byte_limit);
        if self.overflow_fields.capacity() > field_retention_limit {
            self.overflow_fields = Vec::new();
        } else {
            self.overflow_fields.clear();
        }
        if self.overflow_bytes.capacity() > byte_retention_limit {
            self.overflow_bytes = Vec::new();
        } else {
            self.overflow_bytes.clear();
        }
        self.reset();
    }

    fn begin_field(
        &mut self,
        index: usize,
        name_len: usize,
        value_len: usize,
        sensitive: bool,
        allocations: &mut AllocationGate,
    ) -> Result<(), Error> {
        debug_assert_eq!(index, self.field_count);
        let field_bytes = name_len
            .checked_add(value_len)
            .ok_or(Error::FieldSizeOverflow)?;
        let (spill_fields, spill_bytes) =
            self.prepare_field_storage(index, field_bytes, allocations)?;
        self.commit_spills(spill_fields, spill_bytes);

        let descriptor = CompactFieldDescriptor {
            source: CompactFieldSource::Arena {
                start: self.byte_len,
            },
            name_len,
            value_len,
            sensitive,
        };
        if self.fields_spilled {
            self.overflow_fields.push(descriptor);
        } else {
            self.inline_fields[index] = descriptor;
        }
        self.field_count += 1;
        Ok(())
    }

    fn name_byte(&mut self, _index: usize, byte: u8) {
        self.append_bytes(std::slice::from_ref(&byte));
    }

    fn name_bytes(&mut self, _index: usize, bytes: &[u8]) {
        self.append_bytes(bytes);
    }

    fn end_name(&mut self, index: usize) {
        let descriptor = self
            .descriptor(index)
            .expect("current compact header descriptor exists");
        let CompactFieldSource::Arena { start } = descriptor.source else {
            unreachable!("literal compact header uses arena storage");
        };
        debug_assert_eq!(self.byte_len, start + descriptor.name_len);
    }

    fn value_byte(&mut self, _index: usize, byte: u8) {
        self.append_bytes(std::slice::from_ref(&byte));
    }

    fn value_bytes(&mut self, _index: usize, bytes: &[u8]) {
        self.append_bytes(bytes);
    }

    fn end_field(&mut self, index: usize) {
        let descriptor = self
            .descriptor(index)
            .expect("current compact header descriptor exists");
        let CompactFieldSource::Arena { start } = descriptor.source else {
            unreachable!("literal compact header uses arena storage");
        };
        debug_assert_eq!(
            self.byte_len,
            start + descriptor.name_len + descriptor.value_len
        );
    }

    fn indexed_field(
        &mut self,
        index: usize,
        source: IndexedHeaderFieldSource,
        field: HeaderFieldRef<'_>,
        allocations: &mut AllocationGate,
    ) -> Result<(), Error> {
        debug_assert_eq!(index, self.field_count);
        let (spill_fields, spill_bytes) = self.prepare_field_storage(index, 0, allocations)?;
        self.commit_spills(spill_fields, spill_bytes);
        let descriptor = CompactFieldDescriptor {
            source: CompactFieldSource::Indexed(source),
            name_len: field.name.len(),
            value_len: field.value.len(),
            sensitive: field.sensitive,
        };
        if self.fields_spilled {
            self.overflow_fields.push(descriptor);
        } else {
            self.inline_fields[index] = descriptor;
        }
        self.field_count += 1;
        Ok(())
    }

    fn materialize_evicted_indexed_fields(
        &mut self,
        evictions: IndexedHeaderFieldEvictions<'_>,
        allocations: &mut AllocationGate,
    ) -> Result<(), Error> {
        let mut additional_bytes = 0usize;
        for index in 0..self.field_count {
            let descriptor = *self
                .descriptor(index)
                .expect("compact header descriptor is in range");
            if let CompactFieldSource::Indexed(source) = descriptor.source
                && evictions.must_materialize(source)
            {
                additional_bytes = additional_bytes
                    .checked_add(descriptor.name_len)
                    .and_then(|size| size.checked_add(descriptor.value_len))
                    .ok_or(Error::FieldSizeOverflow)?;
            }
        }
        let spill_bytes = self.prepare_byte_storage(additional_bytes, allocations)?;
        self.commit_spills(false, spill_bytes);

        for index in 0..self.field_count {
            let descriptor = *self
                .descriptor(index)
                .expect("compact header descriptor is in range");
            let CompactFieldSource::Indexed(source) = descriptor.source else {
                continue;
            };
            if !evictions.must_materialize(source) {
                continue;
            }
            let field = evictions.resolve(source).ok_or(Error::InvalidIndex)?;
            if field.name.len() != descriptor.name_len || field.value.len() != descriptor.value_len
            {
                return Err(Error::InvalidIndex);
            }
            let start = self.byte_len;
            self.append_bytes(field.name);
            self.append_bytes(field.value);
            self.descriptor_mut(index)
                .expect("compact header descriptor is in range")
                .source = CompactFieldSource::Arena { start };
        }
        Ok(())
    }

    fn field(&self, index: usize) -> HeaderFieldRef<'_> {
        let descriptor = *self
            .descriptor(index)
            .expect("decoded compact header field index is valid");
        let CompactFieldSource::Arena { start } = descriptor.source else {
            unreachable!("incremental literal compact header uses arena storage");
        };
        let bytes = self.bytes();
        let name_end = start + descriptor.name_len;
        let value_end = name_end + descriptor.value_len;
        HeaderFieldRef {
            name: &bytes[start..name_end],
            value: &bytes[name_end..value_end],
            sensitive: descriptor.sensitive,
        }
    }

    fn truncate(&mut self, len: usize) {
        if len >= self.field_count {
            return;
        }
        let byte_len = (0..len)
            .filter_map(|index| {
                let descriptor = *self
                    .descriptor(index)
                    .expect("retained compact header descriptor exists");
                match descriptor.source {
                    CompactFieldSource::Arena { start } => {
                        Some(start + descriptor.name_len + descriptor.value_len)
                    }
                    CompactFieldSource::Indexed(_) => None,
                }
            })
            .max()
            .unwrap_or(0);
        if self.fields_spilled {
            self.overflow_fields.truncate(len);
        }
        if self.bytes_spilled {
            self.overflow_bytes.truncate(byte_len);
        }
        self.field_count = len;
        self.byte_len = byte_len;
    }
}

#[cfg(test)]
pub(crate) struct CompactHeaderIter<'a> {
    fields: &'a CompactHeaderFields,
    resolver: IndexedHeaderFieldResolver<'a>,
    index: usize,
}

#[cfg(test)]
impl<'a> Iterator for CompactHeaderIter<'a> {
    type Item = H2RawHeaderRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let field = self.fields.get(self.index, self.resolver)?;
        self.index += 1;
        Some(field)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.fields.len().saturating_sub(self.index);
        (remaining, Some(remaining))
    }
}

#[cfg(test)]
impl ExactSizeIterator for CompactHeaderIter<'_> {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::head::project_h2_request_head_from_validated;
    use crate::hpack::{Decoder, HeaderField, HeaderFieldVisitor};
    use crate::server::h2::headers::{
        H2ConnectionHpackErrorState, H2HeaderValidationRole, ValidatedHeaderSectionRef,
        decode_connection_header_fields,
    };

    struct IgnoreFields;

    impl HeaderFieldVisitor for IgnoreFields {
        fn start_field(&mut self, _sensitive: bool) {}
        fn name_byte(&mut self, _byte: u8) {}
        fn end_name(&mut self) {}
        fn value_byte(&mut self, _byte: u8) {}
        fn end_field(&mut self) {}
    }

    fn decode_compact(
        decoder: &mut Decoder,
        block: &[u8],
        max_header_list_size: usize,
        fields: &mut CompactHeaderFields,
    ) -> Result<(), Error> {
        decoder.decode_with_visitor_into(block, max_header_list_size, &mut IgnoreFields, fields)
    }

    fn assert_matches_owned(
        compact: &CompactHeaderFields,
        decoder: &Decoder,
        owned: &[HeaderField],
    ) {
        assert_eq!(compact.len(), owned.len());
        for (actual, expected) in compact
            .iter(decoder.indexed_header_field_resolver())
            .zip(owned)
        {
            assert_eq!(actual.name, expected.name);
            assert_eq!(actual.value, expected.value);
            assert_eq!(actual.sensitive, expected.sensitive);
        }
    }

    #[test]
    fn inline_carrier_matches_owned_decode_for_every_field_representation() {
        let block = [
            0x82, // indexed static
            0x40, 0x01, b'a', 0x01, b'b', // incremental literal
            0xbe, // indexed dynamic
            0x00, 0x01, b'c', 0x01, b'd', // without indexing
            0x10, 0x01, b'e', 0x01, b'f', // never indexed
            0x00, 0x01, b'h', 0x8c, 0xf1, 0xe3, 0xc2, 0xe5, 0xf2, 0x3a, 0x6b, 0xa0, 0xab, 0x90,
            0xf4, 0xff, // Huffman literal value
        ];
        let mut compact_decoder = Decoder::new();
        let mut compact = CompactHeaderFields::default();
        decode_compact(&mut compact_decoder, &block, usize::MAX, &mut compact).unwrap();

        let mut owned_decoder = Decoder::new();
        let owned = owned_decoder.decode(&block, usize::MAX).unwrap();
        assert_matches_owned(&compact, &compact_decoder, &owned);
        assert_eq!(compact.uses_overflow(), (false, false));
        assert!(
            compact
                .iter(compact_decoder.indexed_header_field_resolver())
                .last()
                .is_some_and(|field| {
                    field.name == b"h" && field.value == b"www.example.com" && !field.sensitive
                })
        );
    }

    #[test]
    fn indexed_get_with_dynamic_field_uses_only_inline_output() {
        let mut decoder = Decoder::new();
        let mut compact = CompactHeaderFields::default();
        decode_compact(
            &mut decoder,
            &[
                0x40, 0x09, b'x', b'-', b'd', b'y', b'n', b'a', b'm', b'i', b'c', 0x06, b'r', b'e',
                b'u', b's', b'e', b'd',
            ],
            usize::MAX,
            &mut compact,
        )
        .unwrap();

        decoder.set_allocation_failure_after(Some(0));
        decode_compact(
            &mut decoder,
            &[0x82, 0x86, 0x84, 0xbe],
            usize::MAX,
            &mut compact,
        )
        .unwrap();

        assert_eq!(compact.len(), 4);
        assert_eq!(compact.uses_overflow(), (false, false));
        assert_eq!(compact.arena_byte_len(), 0);
        let resolver = decoder.indexed_header_field_resolver();
        assert_eq!(compact.get(0, resolver).unwrap().name, b":method");
        assert_eq!(compact.get(3, resolver).unwrap().value, b"reused");
    }

    #[test]
    fn raw_huffman_and_sensitive_literals_remain_in_the_arena() {
        let mut decoder = Decoder::new();
        let mut compact = CompactHeaderFields::default();

        decode_compact(
            &mut decoder,
            &[0x00, 0x01, b'r', 0x01, b'v'],
            usize::MAX,
            &mut compact,
        )
        .unwrap();
        assert_eq!(compact.arena_byte_len(), 2);

        decode_compact(
            &mut decoder,
            &[
                0x00, 0x01, b'h', 0x8c, 0xf1, 0xe3, 0xc2, 0xe5, 0xf2, 0x3a, 0x6b, 0xa0, 0xab, 0x90,
                0xf4, 0xff,
            ],
            usize::MAX,
            &mut compact,
        )
        .unwrap();
        assert_eq!(compact.arena_byte_len(), 16);
        let field = compact
            .get(0, decoder.indexed_header_field_resolver())
            .unwrap();
        assert_eq!(field.value, b"www.example.com");

        decode_compact(
            &mut decoder,
            &[0x10, 0x01, b's', 0x01, b'v'],
            usize::MAX,
            &mut compact,
        )
        .unwrap();
        assert_eq!(compact.arena_byte_len(), 2);
        assert!(
            compact
                .get(0, decoder.indexed_header_field_resolver())
                .unwrap()
                .sensitive
        );
    }

    #[test]
    fn dynamic_index_survives_non_evicting_same_block_insert_without_copy() {
        let mut decoder = Decoder::new();
        let mut compact = CompactHeaderFields::default();
        decode_compact(
            &mut decoder,
            &[0x40, 0x01, b'x', 0x01, b'a'],
            usize::MAX,
            &mut compact,
        )
        .unwrap();

        decode_compact(
            &mut decoder,
            &[0xbe, 0x40, 0x01, b'y', 0x01, b'b'],
            usize::MAX,
            &mut compact,
        )
        .unwrap();
        assert_eq!(compact.arena_byte_len(), 2);
        let resolver = decoder.indexed_header_field_resolver();
        assert_eq!(compact.get(0, resolver).unwrap().name, b"x");
        assert_eq!(compact.get(1, resolver).unwrap().name, b"y");
    }

    #[test]
    fn dynamic_index_views_survive_later_in_block_eviction() {
        let mut decoder = Decoder::new();
        decoder.set_max_allowed_table_size(64);
        let mut compact = CompactHeaderFields::default();
        decode_compact(
            &mut decoder,
            &[0x3f, 0x21, 0x40, 0x01, b'x', 0x01, b'a'],
            usize::MAX,
            &mut compact,
        )
        .unwrap();

        decode_compact(
            &mut decoder,
            &[0xbe, 0x40, 0x01, b'y', 0x01, b'b'],
            usize::MAX,
            &mut compact,
        )
        .unwrap();
        assert_eq!(compact.arena_byte_len(), 4);
        let resolver = decoder.indexed_header_field_resolver();
        assert_eq!(compact.get(0, resolver).unwrap().name, b"x");
        assert_eq!(compact.get(0, resolver).unwrap().value, b"a");
        assert_eq!(compact.get(1, resolver).unwrap().name, b"y");
        assert_eq!(compact.get(1, resolver).unwrap().value, b"b");
        compact.truncate(1);
        assert_eq!(compact.len(), 1);
        assert_eq!(
            compact
                .get(0, decoder.indexed_header_field_resolver())
                .unwrap()
                .value,
            b"a"
        );

        decode_compact(&mut decoder, &[0xbe], usize::MAX, &mut compact).unwrap();
        let resolver = decoder.indexed_header_field_resolver();
        assert_eq!(compact.get(0, resolver).unwrap().name, b"y");
        assert_eq!(compact.get(0, resolver).unwrap().value, b"b");
    }

    #[test]
    fn table_size_updates_and_oversized_sections_preserve_synchronization() {
        let mut decoder = Decoder::new();
        decoder.set_max_allowed_table_size(0);
        let mut compact = CompactHeaderFields::default();
        decode_compact(&mut decoder, &[0x20, 0x82], usize::MAX, &mut compact).unwrap();
        assert_eq!(compact.len(), 1);
        assert_eq!(decoder.diagnostics().table_size_updates, 1);

        let mut decoder = Decoder::new();
        assert_eq!(
            decode_compact(
                &mut decoder,
                &[0x40, 0x01, b'x', 0x01, b'a'],
                1,
                &mut compact,
            ),
            Err(Error::HeaderListTooLarge { actual: 34 })
        );
        assert!(compact.is_empty());
        decode_compact(&mut decoder, &[0xbe], usize::MAX, &mut compact).unwrap();
        let resolver = decoder.indexed_header_field_resolver();
        assert_eq!(compact.get(0, resolver).unwrap().name, b"x");
        assert_eq!(compact.get(0, resolver).unwrap().value, b"a");
    }

    #[test]
    fn overflow_capacity_is_reused_and_released_at_lower_limits() {
        let mut descriptor_block = Vec::new();
        for _ in 0..=INLINE_FIELD_CAPACITY {
            descriptor_block.extend_from_slice(&[0x00, 0x01, b'x', 0x01, b'y']);
        }

        let mut decoder = Decoder::new();
        let mut compact = CompactHeaderFields::default();
        decode_compact(&mut decoder, &descriptor_block, 1024, &mut compact).unwrap();
        assert_eq!(compact.uses_overflow(), (true, false));

        let mut byte_block = vec![0x00, 0x01, b'x', 0x7f, 0x82, 0x03];
        byte_block.extend(std::iter::repeat_n(b'z', INLINE_BYTE_CAPACITY + 1));
        decode_compact(&mut decoder, &byte_block, 1024, &mut compact).unwrap();
        assert_eq!(compact.uses_overflow(), (false, true));
        let retained = compact.overflow_capacities();
        assert!(retained.0 > INLINE_FIELD_CAPACITY);
        assert!(retained.1 > INLINE_BYTE_CAPACITY);

        compact.reset();
        assert!(compact.is_empty());
        assert_eq!(compact.uses_overflow(), (false, false));
        assert_eq!(compact.overflow_capacities(), retained);

        decode_compact(&mut decoder, &[0x82], 1024, &mut compact).unwrap();
        assert_eq!(compact.overflow_capacities(), retained);
        decode_compact(&mut decoder, &[0x82], 128, &mut compact).unwrap();
        assert_eq!(compact.overflow_capacities(), (0, 0));
    }

    #[cfg(feature = "hpack-test-support")]
    #[test]
    fn compact_spill_failure_precedes_dynamic_table_mutation() {
        let mut block = vec![0x40, 0x01, b'x', 0x7f, 0x82, 0x03];
        block.extend(std::iter::repeat_n(b'z', INLINE_BYTE_CAPACITY + 1));
        let mut succeeded = false;
        for successful_allocations in 0..16 {
            let mut decoder = Decoder::new();
            let mut compact = CompactHeaderFields::default();
            decoder.set_allocation_failure_after(Some(successful_allocations));

            match decode_compact(&mut decoder, &block, 1024, &mut compact) {
                Err(Error::AllocationFailed) => {
                    assert!(decoder.test_table_snapshot().entries.is_empty());
                }
                Ok(()) => {
                    assert_eq!(decoder.test_table_snapshot().entries.len(), 1);
                    assert_eq!(
                        compact
                            .get(0, decoder.indexed_header_field_resolver())
                            .unwrap()
                            .value
                            .len(),
                        INLINE_BYTE_CAPACITY + 1
                    );
                    succeeded = true;
                    break;
                }
                Err(error) => panic!("unexpected compact decoder error: {error:?}"),
            }
        }
        assert!(succeeded, "allocation-failure sweep never reached success");
    }

    #[cfg(feature = "hpack-test-support")]
    #[test]
    fn eviction_materialization_failure_precedes_dynamic_table_mutation() {
        let mut decoder = Decoder::new();
        decoder.set_max_allowed_table_size(600);
        let mut seed = vec![0x3f, 0xb9, 0x04, 0x40, 0x01, b'x', 0x7f, 0x89, 0x03];
        seed.extend(std::iter::repeat_n(b'a', 520));
        decoder.decode(&seed, 2_000).unwrap();
        let before = decoder.test_table_snapshot().entries;

        let mut block = vec![0xbe, 0x40, 0x01, b'y', 0x30];
        block.extend(std::iter::repeat_n(b'b', 48));
        let mut compact = CompactHeaderFields::default();
        decoder.set_allocation_failure_after(Some(0));
        assert_eq!(
            decode_compact(&mut decoder, &block, 2_000, &mut compact),
            Err(Error::AllocationFailed)
        );
        assert_eq!(decoder.test_table_snapshot().entries, before);
    }

    #[test]
    fn connection_decode_accepts_compact_output_sink() {
        let mut decoder = Decoder::new();
        let mut compact = CompactHeaderFields::default();
        let mut last_hpack_error = None;
        let mut last_protocol_error = None;
        let mut terminal_protocol_error = None;
        decode_connection_header_fields(
            &mut decoder,
            &mut compact,
            &[0x82, 0x86, 0x84],
            usize::MAX,
            1,
            H2HeaderValidationRole::Request,
            H2ConnectionHpackErrorState {
                last_hpack_error: &mut last_hpack_error,
                last_protocol_error: &mut last_protocol_error,
                terminal_protocol_error: &mut terminal_protocol_error,
            },
        )
        .unwrap();

        assert_eq!(compact.len(), 3);
        assert!(last_hpack_error.is_none());
        assert!(last_protocol_error.is_none());
        assert!(terminal_protocol_error.is_none());
    }

    #[test]
    fn validated_compact_section_preserves_traversal_and_role_provenance() {
        let mut decoder = Decoder::new();
        let mut compact = CompactHeaderFields::default();
        let mut last_hpack_error = None;
        let mut last_protocol_error = None;
        let mut terminal_protocol_error = None;
        let section = decode_connection_header_fields(
            &mut decoder,
            &mut compact,
            &[0x82, 0x86, 0x84, 0x00, 0x01, b'x', 0x01, b'y'],
            usize::MAX,
            1,
            H2HeaderValidationRole::Request,
            H2ConnectionHpackErrorState {
                last_hpack_error: &mut last_hpack_error,
                last_protocol_error: &mut last_protocol_error,
                terminal_protocol_error: &mut terminal_protocol_error,
            },
        )
        .unwrap();
        assert_ne!(section.role, H2HeaderValidationRole::Response);
        let head = project_h2_request_head_from_validated(
            ValidatedHeaderSectionRef::new_compact(
                &compact,
                decoder.indexed_header_field_resolver(),
                section,
            ),
            crate::HttpLimits::new(),
        )
        .unwrap();
        assert_eq!(head.method(), b"GET");
        assert_eq!(head.path(), b"/");
        let mut fields = head.fields().iter();
        assert_eq!(fields.len(), 1);
        assert_eq!(fields.next(), Some(H2RawHeaderRef::new(b"x", b"y")));
        assert_eq!(fields.next(), None);
    }
}
