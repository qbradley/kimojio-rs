#[cfg(test)]
use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::hash::BuildHasher;

use rustc_hash::FxBuildHasher;

use crate::huffman_table::HUFFMAN_CODES;

mod static_table;
use static_table::{STATIC_TABLE, STATIC_TABLE_LEN, find_static_exact, find_static_name};

const DEFAULT_TABLE_SIZE: usize = 4096;
pub(crate) const MAX_TABLE_SIZE: usize = 1_048_576;
/// Entry count above which name/exact lookup uses FxHash indexes.
const HASH_INDEX_THRESHOLD: usize = 16;

#[derive(Clone, Debug, Eq, PartialEq)]
/// An owned, byte-preserving HTTP/2 header occurrence.
pub struct HeaderField {
    /// Header or pseudo-header name bytes.
    pub name: Vec<u8>,
    /// Header value bytes.
    pub value: Vec<u8>,
    /// Whether this occurrence must use HPACK's never-indexed representation.
    pub sensitive: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct HeaderFieldRef<'a> {
    pub(crate) name: &'a [u8],
    pub(crate) value: &'a [u8],
    pub(crate) sensitive: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IndexedHeaderFieldSource {
    Static(usize),
    Dynamic(u64),
}

#[derive(Clone, Copy)]
pub(crate) struct IndexedHeaderFieldResolver<'a> {
    table: &'a DynamicTable,
}

#[derive(Clone, Copy)]
pub(crate) struct IndexedHeaderFieldEvictions<'a> {
    resolver: IndexedHeaderFieldResolver<'a>,
    first_retained_dynamic_id: u64,
}

pub(crate) trait HeaderFieldVisitor {
    fn start_field(&mut self, sensitive: bool);
    fn name_byte(&mut self, byte: u8);
    fn name_bytes(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.name_byte(byte);
        }
    }
    fn end_name(&mut self);
    fn value_byte(&mut self, byte: u8);
    fn value_bytes(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.value_byte(byte);
        }
    }
    fn end_field(&mut self);
}

pub(crate) trait HeaderDecodeOutput {
    fn begin_block(&mut self, max_header_list_size: usize);

    fn begin_field(
        &mut self,
        index: usize,
        name_len: usize,
        value_len: usize,
        sensitive: bool,
        allocations: &mut AllocationGate,
    ) -> Result<(), Error>;

    fn name_byte(&mut self, index: usize, byte: u8);

    fn name_bytes(&mut self, index: usize, bytes: &[u8]) {
        for &byte in bytes {
            self.name_byte(index, byte);
        }
    }

    fn end_name(&mut self, index: usize);

    fn value_byte(&mut self, index: usize, byte: u8);

    fn value_bytes(&mut self, index: usize, bytes: &[u8]) {
        for &byte in bytes {
            self.value_byte(index, byte);
        }
    }

    fn end_field(&mut self, index: usize);

    fn indexed_field(
        &mut self,
        index: usize,
        source: IndexedHeaderFieldSource,
        field: HeaderFieldRef<'_>,
        allocations: &mut AllocationGate,
    ) -> Result<(), Error> {
        let _ = source;
        self.begin_field(
            index,
            field.name.len(),
            field.value.len(),
            field.sensitive,
            allocations,
        )?;
        self.name_bytes(index, field.name);
        self.end_name(index);
        self.value_bytes(index, field.value);
        self.end_field(index);
        Ok(())
    }

    fn materialize_evicted_indexed_fields(
        &mut self,
        _evictions: IndexedHeaderFieldEvictions<'_>,
        _allocations: &mut AllocationGate,
    ) -> Result<(), Error> {
        Ok(())
    }

    fn field(&self, index: usize) -> HeaderFieldRef<'_>;

    fn truncate(&mut self, len: usize);
}

struct IgnoreHeaderFields;

impl HeaderFieldVisitor for IgnoreHeaderFields {
    fn start_field(&mut self, _sensitive: bool) {}
    fn name_byte(&mut self, _byte: u8) {}
    fn end_name(&mut self) {}
    fn value_byte(&mut self, _byte: u8) {}
    fn end_field(&mut self) {}
}

fn visit_field(visitor: &mut impl HeaderFieldVisitor, name: &[u8], value: &[u8], sensitive: bool) {
    visitor.start_field(sensitive);
    visitor.name_bytes(name);
    visitor.end_name();
    visitor.value_bytes(value);
    visitor.end_field();
}

struct OutputVisitor<'a, Output, Visitor> {
    output: &'a mut Output,
    visitor: &'a mut Visitor,
    field_index: usize,
}

impl<Output, Visitor> HeaderFieldVisitor for OutputVisitor<'_, Output, Visitor>
where
    Output: HeaderDecodeOutput,
    Visitor: HeaderFieldVisitor,
{
    fn start_field(&mut self, sensitive: bool) {
        self.visitor.start_field(sensitive);
    }

    fn name_byte(&mut self, byte: u8) {
        self.output.name_byte(self.field_index, byte);
        self.visitor.name_byte(byte);
    }

    fn name_bytes(&mut self, bytes: &[u8]) {
        self.output.name_bytes(self.field_index, bytes);
        self.visitor.name_bytes(bytes);
    }

    fn end_name(&mut self) {
        self.output.end_name(self.field_index);
        self.visitor.end_name();
    }

    fn value_byte(&mut self, byte: u8) {
        self.output.value_byte(self.field_index, byte);
        self.visitor.value_byte(byte);
    }

    fn value_bytes(&mut self, bytes: &[u8]) {
        self.output.value_bytes(self.field_index, bytes);
        self.visitor.value_bytes(bytes);
    }

    fn end_field(&mut self) {
        self.output.end_field(self.field_index);
        self.visitor.end_field();
    }
}

#[derive(Clone, Copy)]
enum LiteralKind {
    Incremental,
    WithoutIndexing,
    NeverIndexed,
}

impl LiteralKind {
    const fn prefix_bits(self) -> u8 {
        match self {
            Self::Incremental => 6,
            Self::WithoutIndexing | Self::NeverIndexed => 4,
        }
    }

    const fn marker(self) -> u8 {
        match self {
            Self::Incremental => 0x40,
            Self::WithoutIndexing => 0,
            Self::NeverIndexed => 0x10,
        }
    }
}

struct LiteralPreview {
    end: usize,
    decoded_name_len: usize,
    decoded_value_len: usize,
    huffman_strings: u64,
    plain_strings: u64,
}

impl HeaderField {
    /// Creates a nonsensitive owned header occurrence.
    pub fn new(name: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
            sensitive: false,
        }
    }

    /// Sets whether this occurrence must use HPACK's never-indexed form.
    pub const fn with_sensitive(mut self, sensitive: bool) -> Self {
        self.sensitive = sensitive;
        self
    }

    #[cfg(test)]
    pub(crate) fn sensitive(name: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
            sensitive: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Diagnostics {
    pub(crate) encoded_blocks: u64,
    pub(crate) decoded_blocks: u64,
    pub(crate) indexed_fields: u64,
    pub(crate) incremental_fields: u64,
    pub(crate) without_indexing_fields: u64,
    pub(crate) never_indexed_fields: u64,
    pub(crate) huffman_strings: u64,
    pub(crate) plain_strings: u64,
    pub(crate) table_size_updates: u64,
    pub(crate) table_insertions: u64,
    pub(crate) table_evictions: u64,
    pub(crate) compression_errors: u64,
    pub(crate) header_list_too_large: u64,
    pub(crate) field_bytes: u64,
    pub(crate) wire_bytes: u64,
}

impl Diagnostics {
    fn add_assign(&mut self, other: Self) {
        self.encoded_blocks = self.encoded_blocks.saturating_add(other.encoded_blocks);
        self.decoded_blocks = self.decoded_blocks.saturating_add(other.decoded_blocks);
        self.indexed_fields = self.indexed_fields.saturating_add(other.indexed_fields);
        self.incremental_fields = self
            .incremental_fields
            .saturating_add(other.incremental_fields);
        self.without_indexing_fields = self
            .without_indexing_fields
            .saturating_add(other.without_indexing_fields);
        self.never_indexed_fields = self
            .never_indexed_fields
            .saturating_add(other.never_indexed_fields);
        self.huffman_strings = self.huffman_strings.saturating_add(other.huffman_strings);
        self.plain_strings = self.plain_strings.saturating_add(other.plain_strings);
        self.table_size_updates = self
            .table_size_updates
            .saturating_add(other.table_size_updates);
        self.table_insertions = self.table_insertions.saturating_add(other.table_insertions);
        self.table_evictions = self.table_evictions.saturating_add(other.table_evictions);
        self.compression_errors = self
            .compression_errors
            .saturating_add(other.compression_errors);
        self.header_list_too_large = self
            .header_list_too_large
            .saturating_add(other.header_list_too_large);
        self.field_bytes = self.field_bytes.saturating_add(other.field_bytes);
        self.wire_bytes = self.wire_bytes.saturating_add(other.wire_bytes);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    TruncatedInteger,
    TruncatedString,
    IntegerOverflow,
    InvalidIndex,
    InvalidMaxDynamicSize,
    InvalidTableSizeUpdate,
    TableSizeUpdateAfterField,
    InvalidHuffman,
    HeaderListTooLarge { actual: usize },
    FieldSizeOverflow,
    StateOverflow,
    DecoderPoisoned,
    AllocationFailed,
}

#[derive(Clone, Default)]
pub(crate) struct AllocationGate {
    fail_after: Option<usize>,
    #[cfg(any(test, feature = "hpack-test-support"))]
    observations: AllocationObservations,
}

#[derive(Clone, Copy)]
enum AllocationPurpose {
    Other,
    DecodedOutput,
    DynamicTableSynchronization,
}

#[cfg(any(test, feature = "hpack-test-support"))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct AllocationObservations {
    decoded_output_allocations: usize,
    dynamic_table_synchronization_allocations: usize,
}

#[cfg(any(test, feature = "hpack-test-support"))]
impl AllocationObservations {
    fn record(&mut self, purpose: AllocationPurpose) {
        let counter = match purpose {
            AllocationPurpose::Other => return,
            AllocationPurpose::DecodedOutput => &mut self.decoded_output_allocations,
            AllocationPurpose::DynamicTableSynchronization => {
                &mut self.dynamic_table_synchronization_allocations
            }
        };
        *counter = counter.saturating_add(1);
    }

    fn since(self, earlier: Self) -> Self {
        Self {
            decoded_output_allocations: self
                .decoded_output_allocations
                .saturating_sub(earlier.decoded_output_allocations),
            dynamic_table_synchronization_allocations: self
                .dynamic_table_synchronization_allocations
                .saturating_sub(earlier.dynamic_table_synchronization_allocations),
        }
    }
}

impl AllocationGate {
    #[cfg(any(test, feature = "hpack-test-support"))]
    fn set_failure_after(&mut self, successful_allocations: Option<usize>) {
        self.fail_after = successful_allocations;
    }

    fn before_allocation(&mut self) -> Result<(), Error> {
        let Some(remaining) = self.fail_after.as_mut() else {
            return Ok(());
        };
        if *remaining == 0 {
            return Err(Error::AllocationFailed);
        }
        *remaining -= 1;
        Ok(())
    }

    fn record_success(&mut self, purpose: AllocationPurpose) {
        #[cfg(any(test, feature = "hpack-test-support"))]
        self.observations.record(purpose);
        #[cfg(not(any(test, feature = "hpack-test-support")))]
        let _ = purpose;
    }

    #[cfg(any(test, feature = "hpack-test-support"))]
    fn observations(&self) -> AllocationObservations {
        self.observations
    }

    fn try_reserve_vec<T>(
        &mut self,
        values: &mut Vec<T>,
        additional: usize,
        purpose: AllocationPurpose,
    ) -> Result<(), Error> {
        if additional <= values.capacity().saturating_sub(values.len()) {
            return Ok(());
        }
        self.before_allocation()?;
        values
            .try_reserve_exact(additional)
            .map_err(|_| Error::AllocationFailed)?;
        self.record_success(purpose);
        Ok(())
    }

    pub(crate) fn try_reserve_decoded_output<T>(
        &mut self,
        values: &mut Vec<T>,
        additional: usize,
    ) -> Result<(), Error> {
        self.try_reserve_vec(values, additional, AllocationPurpose::DecodedOutput)
    }

    fn try_reserve_deque<T>(
        &mut self,
        values: &mut VecDeque<T>,
        additional: usize,
    ) -> Result<(), Error> {
        if additional <= values.capacity().saturating_sub(values.len()) {
            return Ok(());
        }
        self.before_allocation()?;
        values
            .try_reserve(additional)
            .map_err(|_| Error::AllocationFailed)?;
        self.record_success(AllocationPurpose::DynamicTableSynchronization);
        Ok(())
    }

    fn try_reserve_map<K, V, S>(
        &mut self,
        values: &mut HashMap<K, V, S>,
        additional: usize,
    ) -> Result<(), Error>
    where
        K: Eq + std::hash::Hash,
        S: BuildHasher,
    {
        if additional <= values.capacity().saturating_sub(values.len()) {
            return Ok(());
        }
        self.before_allocation()?;
        values
            .try_reserve(additional)
            .map_err(|_| Error::AllocationFailed)?;
        self.record_success(AllocationPurpose::DynamicTableSynchronization);
        Ok(())
    }
}

struct Entry {
    bytes: Vec<u8>,
    metadata: u64,
}

impl Clone for Entry {
    fn clone(&self) -> Self {
        Self {
            bytes: self.bytes.clone(),
            metadata: self.metadata,
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.bytes.clone_from(&source.bytes);
        self.metadata = source.metadata;
    }
}

impl Entry {
    const NAME_BITS: u32 = 21;
    const DELTA_BITS: u32 = 16;
    const NAME_MASK: u64 = (1 << Self::NAME_BITS) - 1;
    const DELTA_MASK: u64 = (1 << Self::DELTA_BITS) - 1;

    fn try_new(name: &[u8], value: &[u8], allocations: &mut AllocationGate) -> Result<Self, Error> {
        let length = name
            .len()
            .checked_add(value.len())
            .ok_or(Error::FieldSizeOverflow)?;
        let name_len = u32::try_from(name.len()).map_err(|_| Error::FieldSizeOverflow)?;
        if u64::from(name_len) > Self::NAME_MASK {
            return Err(Error::FieldSizeOverflow);
        }
        let mut bytes = Vec::new();
        allocations.try_reserve_vec(
            &mut bytes,
            length,
            AllocationPurpose::DynamicTableSynchronization,
        )?;
        bytes.extend_from_slice(name);
        bytes.extend_from_slice(value);
        Ok(Self {
            bytes,
            metadata: u64::from(name_len),
        })
    }

    fn try_clone_from(
        &mut self,
        source: &Self,
        allocations: &mut AllocationGate,
    ) -> Result<(), Error> {
        let additional = source.bytes.len().saturating_sub(self.bytes.len());
        allocations.try_reserve_vec(
            &mut self.bytes,
            additional,
            AllocationPurpose::DynamicTableSynchronization,
        )?;
        self.bytes.clear();
        self.bytes.extend_from_slice(&source.bytes);
        self.metadata = source.metadata;
        Ok(())
    }

    fn name(&self) -> &[u8] {
        &self.bytes[..self.name_len()]
    }

    fn value(&self) -> &[u8] {
        &self.bytes[self.name_len()..]
    }

    fn size(&self) -> usize {
        self.bytes.len() + 32
    }

    fn name_len(&self) -> usize {
        (self.metadata & Self::NAME_MASK) as usize
    }

    fn previous_name_delta(&self) -> u32 {
        ((self.metadata >> Self::NAME_BITS) & Self::DELTA_MASK) as u32
    }

    fn previous_exact_delta(&self) -> u32 {
        ((self.metadata >> (Self::NAME_BITS + Self::DELTA_BITS)) & Self::DELTA_MASK) as u32
    }

    fn set_collision_deltas(&mut self, name_delta: u32, exact_delta: u32) {
        debug_assert!(u64::from(name_delta) <= Self::DELTA_MASK);
        debug_assert!(u64::from(exact_delta) <= Self::DELTA_MASK);
        self.metadata = (self.metadata & Self::NAME_MASK)
            | (u64::from(name_delta) << Self::NAME_BITS)
            | (u64::from(exact_delta) << (Self::NAME_BITS + Self::DELTA_BITS));
    }
}

struct EntryDeque {
    entries: VecDeque<Entry>,
}

impl Clone for EntryDeque {
    fn clone(&self) -> Self {
        Self {
            entries: self.entries.clone(),
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.entries.clone_from(&source.entries);
    }
}

impl EntryDeque {
    fn new() -> Self {
        Self {
            entries: VecDeque::new(),
        }
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn push_front(&mut self, entry: Entry) {
        self.entries.push_front(entry);
    }

    fn pop_back(&mut self) -> Option<Entry> {
        self.entries.pop_back()
    }

    fn get(&self, offset: usize) -> Option<&Entry> {
        self.entries.get(offset)
    }

    fn get_mut(&mut self, offset: usize) -> Option<&mut Entry> {
        self.entries.get_mut(offset)
    }

    fn clear_retaining_storage(&mut self) {
        self.entries.clear();
    }

    fn release_storage(&mut self) {
        self.entries = VecDeque::new();
    }

    fn try_reserve(
        &mut self,
        additional: usize,
        allocations: &mut AllocationGate,
    ) -> Result<(), Error> {
        allocations.try_reserve_deque(&mut self.entries, additional)
    }

    fn try_clone_from(
        &mut self,
        source: &Self,
        allocations: &mut AllocationGate,
    ) -> Result<(), Error> {
        let additional = source.entries.len().saturating_sub(self.entries.len());
        self.try_reserve(additional, allocations)?;
        while self.entries.len() < source.entries.len() {
            self.entries.push_back(Entry {
                bytes: Vec::new(),
                metadata: 0,
            });
        }
        self.entries.truncate(source.entries.len());
        for (entry, source) in self.entries.iter_mut().zip(&source.entries) {
            entry.try_clone_from(source, allocations)?;
        }
        Ok(())
    }

    #[cfg(any(test, feature = "hpack-test-support"))]
    fn capacity(&self) -> usize {
        self.entries.capacity()
    }
}

struct DynamicTable {
    entries: EntryDeque,
    size: usize,
    max_size: usize,
    next_id: u64,
    hash_builder: FxBuildHasher,
    newest_name: HashMap<u64, u64, FxBuildHasher>,
    newest_exact: HashMap<u64, u64, FxBuildHasher>,
    #[cfg(test)]
    lookup_work: Cell<usize>,
}

impl Clone for DynamicTable {
    fn clone(&self) -> Self {
        Self {
            entries: self.entries.clone(),
            size: self.size,
            max_size: self.max_size,
            next_id: self.next_id,
            hash_builder: self.hash_builder,
            newest_name: self.newest_name.clone(),
            newest_exact: self.newest_exact.clone(),
            #[cfg(test)]
            lookup_work: self.lookup_work.clone(),
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.entries.clone_from(&source.entries);
        self.size = source.size;
        self.max_size = source.max_size;
        self.next_id = source.next_id;
        self.hash_builder.clone_from(&source.hash_builder);
        self.newest_name.clone_from(&source.newest_name);
        self.newest_exact.clone_from(&source.newest_exact);
        #[cfg(test)]
        self.lookup_work.clone_from(&source.lookup_work);
    }
}

#[cfg(feature = "hpack-test-support")]
pub(crate) struct TestTableSnapshot {
    pub(crate) max_size: usize,
    pub(crate) size: usize,
    pub(crate) entries: Vec<(Vec<u8>, Vec<u8>)>,
    pub(crate) container_capacities: [usize; 3],
}

impl DynamicTable {
    fn new(max_size: usize) -> Self {
        Self {
            entries: EntryDeque::new(),
            size: 0,
            max_size: max_size.min(MAX_TABLE_SIZE),
            next_id: 0,
            hash_builder: FxBuildHasher,
            newest_name: HashMap::with_hasher(FxBuildHasher),
            newest_exact: HashMap::with_hasher(FxBuildHasher),
            #[cfg(test)]
            lookup_work: Cell::new(0),
        }
    }

    fn try_clone_from(
        &mut self,
        source: &Self,
        allocations: &mut AllocationGate,
    ) -> Result<(), Error> {
        if source.entries.len() == 0 {
            self.entries.release_storage();
            self.drop_hash_indexes();
        } else {
            self.entries.try_clone_from(&source.entries, allocations)?;
            self.newest_name.clear();
            allocations.try_reserve_map(&mut self.newest_name, source.newest_name.len())?;
            self.newest_name
                .extend(source.newest_name.iter().map(|(&hash, &id)| (hash, id)));
            self.newest_exact.clear();
            allocations.try_reserve_map(&mut self.newest_exact, source.newest_exact.len())?;
            self.newest_exact
                .extend(source.newest_exact.iter().map(|(&hash, &id)| (hash, id)));
        }
        self.size = source.size;
        self.max_size = source.max_size;
        self.next_id = source.next_id;
        self.hash_builder.clone_from(&source.hash_builder);
        #[cfg(test)]
        self.lookup_work.clone_from(&source.lookup_work);
        Ok(())
    }

    fn set_max_size_retaining_storage(&mut self, max_size: usize) -> usize {
        self.max_size = max_size.min(MAX_TABLE_SIZE);
        self.evict_to_limit()
    }

    fn set_max_size_releasing_storage(&mut self, max_size: usize) -> usize {
        let evictions = self.set_max_size_retaining_storage(max_size);
        if self.entries.len() == 0 {
            self.release_empty_storage();
        }
        evictions
    }

    fn get(&self, index: usize) -> Option<(&[u8], &[u8])> {
        if index == 0 {
            return None;
        }
        if index <= STATIC_TABLE_LEN {
            return Some(STATIC_TABLE[index - 1]);
        }
        self.entries
            .get(index - STATIC_TABLE_LEN - 1)
            .map(|entry| (entry.name(), entry.value()))
    }

    fn get_indexed(&self, index: usize) -> Option<(IndexedHeaderFieldSource, HeaderFieldRef<'_>)> {
        if index == 0 {
            return None;
        }
        if index <= STATIC_TABLE_LEN {
            let (name, value) = STATIC_TABLE[index - 1];
            return Some((
                IndexedHeaderFieldSource::Static(index),
                HeaderFieldRef {
                    name,
                    value,
                    sensitive: false,
                },
            ));
        }
        let offset = index - STATIC_TABLE_LEN - 1;
        let entry = self.entries.get(offset)?;
        let distance = u64::try_from(offset).ok()?.checked_add(1)?;
        let id = self.next_id.checked_sub(distance)?;
        Some((
            IndexedHeaderFieldSource::Dynamic(id),
            HeaderFieldRef {
                name: entry.name(),
                value: entry.value(),
                sensitive: false,
            },
        ))
    }

    fn indexed_field_resolver(&self) -> IndexedHeaderFieldResolver<'_> {
        IndexedHeaderFieldResolver { table: self }
    }

    fn indexed_field_evictions_for_insertion(
        &self,
        entry_size: usize,
    ) -> IndexedHeaderFieldEvictions<'_> {
        debug_assert!(entry_size <= self.max_size);
        if self.next_id.checked_add(1).is_none() {
            return self.indexed_field_evictions_for_clear();
        }
        let retained_limit = self.max_size - entry_size;
        let mut retained_size = self.size;
        let mut evicted = 0usize;
        for offset in (0..self.entries.len()).rev() {
            if retained_size <= retained_limit {
                break;
            }
            let entry = self
                .entries
                .get(offset)
                .expect("dynamic eviction offset is in range");
            retained_size -= entry.size();
            evicted += 1;
        }
        self.indexed_field_evictions(evicted)
    }

    fn indexed_field_evictions_for_clear(&self) -> IndexedHeaderFieldEvictions<'_> {
        self.indexed_field_evictions(self.entries.len())
    }

    fn indexed_field_evictions(&self, evicted: usize) -> IndexedHeaderFieldEvictions<'_> {
        let retained_len =
            u64::try_from(self.entries.len()).expect("dynamic table length fits in u64");
        let evicted = u64::try_from(evicted).expect("dynamic eviction count fits in u64");
        let oldest_id = self.next_id - retained_len;
        IndexedHeaderFieldEvictions {
            resolver: self.indexed_field_resolver(),
            first_retained_dynamic_id: oldest_id + evicted,
        }
    }

    fn uses_hash_index(&self) -> bool {
        !self.newest_name.is_empty() || !self.newest_exact.is_empty()
    }

    fn drop_hash_indexes(&mut self) {
        self.newest_name = HashMap::with_hasher(FxBuildHasher);
        self.newest_exact = HashMap::with_hasher(FxBuildHasher);
    }

    fn rebuild_hash_indexes(&mut self) {
        self.newest_name.clear();
        self.newest_exact.clear();
        let len = self.entries.len();
        for offset in (0..len).rev() {
            let id = self.next_id - 1 - offset as u64;
            let (name_hash, exact_hash, name_delta, exact_delta) = {
                let entry = self.entries.get(offset).expect("rebuild offset in range");
                let name_hash = self.hash_builder.hash_one(entry.name());
                let exact_hash = self.hash_builder.hash_one((entry.name(), entry.value()));
                let name_delta = self.collision_delta(id, self.newest_name.get(&name_hash));
                let exact_delta = self.collision_delta(id, self.newest_exact.get(&exact_hash));
                (name_hash, exact_hash, name_delta, exact_delta)
            };
            self.entries
                .get_mut(offset)
                .expect("rebuild offset in range")
                .set_collision_deltas(name_delta, exact_delta);
            self.newest_name.insert(name_hash, id);
            self.newest_exact.insert(exact_hash, id);
        }
    }

    fn find_exact_by_scan(&self, name: &[u8], value: &[u8]) -> Option<usize> {
        for offset in 0..self.entries.len() {
            let entry = self.entries.get(offset)?;
            #[cfg(test)]
            self.lookup_work
                .set(self.lookup_work.get().saturating_add(entry.bytes.len()));
            if entry.name() == name && entry.value() == value {
                return Some(STATIC_TABLE_LEN + 1 + offset);
            }
        }
        None
    }

    fn find_name_by_scan(&self, name: &[u8]) -> Option<usize> {
        for offset in 0..self.entries.len() {
            let entry = self.entries.get(offset)?;
            #[cfg(test)]
            self.lookup_work
                .set(self.lookup_work.get().saturating_add(entry.name().len()));
            if entry.name() == name {
                return Some(STATIC_TABLE_LEN + 1 + offset);
            }
        }
        None
    }

    #[cfg(test)]
    fn find_exact(&self, name: &[u8], value: &[u8]) -> Option<usize> {
        #[cfg(test)]
        self.lookup_work.set(
            self.lookup_work
                .get()
                .saturating_add(name.len())
                .saturating_add(value.len()),
        );
        if self.uses_hash_index() {
            let hash = self.hash_builder.hash_one((name, value));
            self.find_exact_with_hash(name, value, hash)
        } else {
            self.find_exact_by_scan(name, value)
        }
    }

    #[cfg(test)]
    fn find_name(&self, name: &[u8]) -> Option<usize> {
        #[cfg(test)]
        self.lookup_work
            .set(self.lookup_work.get().saturating_add(name.len()));
        if self.uses_hash_index() {
            let hash = self.hash_builder.hash_one(name);
            self.find_name_with_hash(name, hash)
        } else {
            self.find_name_by_scan(name)
        }
    }

    fn find_exact_for_plan(
        &self,
        name: &[u8],
        value: &[u8],
        hash: Option<u64>,
    ) -> Result<Option<usize>, Error> {
        #[cfg(test)]
        self.lookup_work.set(
            self.lookup_work
                .get()
                .saturating_add(name.len())
                .saturating_add(value.len()),
        );
        if self.uses_hash_index() {
            Ok(self.find_exact_with_hash(name, value, hash.ok_or(Error::StateOverflow)?))
        } else {
            Ok(self.find_exact_by_scan(name, value))
        }
    }

    fn find_name_for_plan(&self, name: &[u8], hash: Option<u64>) -> Result<Option<usize>, Error> {
        #[cfg(test)]
        self.lookup_work
            .set(self.lookup_work.get().saturating_add(name.len()));
        if self.uses_hash_index() {
            Ok(self.find_name_with_hash(name, hash.ok_or(Error::StateOverflow)?))
        } else {
            Ok(self.find_name_by_scan(name))
        }
    }

    fn find_exact_with_hash(&self, name: &[u8], value: &[u8], hash: u64) -> Option<usize> {
        let mut id = *self.newest_exact.get(&hash)?;
        loop {
            let offset = self.offset_for_id(id)?;
            let entry = self.entries.get(offset)?;
            #[cfg(test)]
            self.lookup_work
                .set(self.lookup_work.get().saturating_add(entry.bytes.len()));
            if entry.name() == name && entry.value() == value {
                return Some(STATIC_TABLE_LEN + 1 + offset);
            }
            if entry.previous_exact_delta() == 0 {
                return None;
            }
            id = id.checked_sub(u64::from(entry.previous_exact_delta()))?;
        }
    }

    fn find_name_with_hash(&self, name: &[u8], hash: u64) -> Option<usize> {
        let mut id = *self.newest_name.get(&hash)?;
        loop {
            let offset = self.offset_for_id(id)?;
            let entry = self.entries.get(offset)?;
            #[cfg(test)]
            self.lookup_work
                .set(self.lookup_work.get().saturating_add(entry.name().len()));
            if entry.name() == name {
                return Some(STATIC_TABLE_LEN + 1 + offset);
            }
            if entry.previous_name_delta() == 0 {
                return None;
            }
            id = id.checked_sub(u64::from(entry.previous_name_delta()))?;
        }
    }

    fn insert(
        &mut self,
        name: &[u8],
        value: &[u8],
        allocations: &mut AllocationGate,
    ) -> Result<(bool, usize), Error> {
        let size = name
            .len()
            .checked_add(value.len())
            .and_then(|value| value.checked_add(32))
            .ok_or(Error::FieldSizeOverflow)?;
        if size > self.max_size {
            let evictions = self.clear_releasing_storage();
            return Ok((false, evictions));
        }

        let entry = Entry::try_new(name, value, allocations)?;
        self.try_reserve_insertion_for_size(size, allocations)?;
        self.ensure_id_space(1)?;
        Ok(self.insert_prepared(entry))
    }

    fn try_reserve_insertion_for_size(
        &mut self,
        entry_size: usize,
        allocations: &mut AllocationGate,
    ) -> Result<(), Error> {
        let retained_limit = self.max_size - entry_size;
        let additional = usize::from(self.size <= retained_limit);
        self.try_reserve_insertions(additional, allocations)
    }

    fn try_reserve_insertions(
        &mut self,
        additional: usize,
        allocations: &mut AllocationGate,
    ) -> Result<(), Error> {
        self.entries.try_reserve(additional, allocations)?;
        let future_len = self.entries.len().saturating_add(additional);
        if future_len > HASH_INDEX_THRESHOLD || self.uses_hash_index() {
            let map_additional = future_len.saturating_sub(self.newest_name.len());
            allocations.try_reserve_map(&mut self.newest_name, map_additional)?;
            allocations.try_reserve_map(&mut self.newest_exact, map_additional)?;
        }
        Ok(())
    }

    fn try_reserve_retained_entries(
        &mut self,
        insertion_candidates: usize,
        effective_max_size: usize,
        allocations: &mut AllocationGate,
    ) -> Result<(), Error> {
        let maximum_entries = effective_max_size / 32;
        let additional =
            insertion_candidates.min(maximum_entries.saturating_sub(self.entries.len()));
        self.try_reserve_insertions(additional, allocations)
    }

    fn insert_prepared(&mut self, mut entry: Entry) -> (bool, usize) {
        let size = entry.size();
        let retained_limit = self.max_size - size;
        let mut evictions = 0;
        while self.size > retained_limit {
            self.evict_one();
            evictions += 1;
        }

        if self.uses_hash_index() {
            let id = self.next_id;
            let name_hash = self.hash_builder.hash_one(entry.name());
            let exact_hash = self.hash_builder.hash_one((entry.name(), entry.value()));
            let name_delta = self.collision_delta(id, self.newest_name.get(&name_hash));
            let exact_delta = self.collision_delta(id, self.newest_exact.get(&exact_hash));
            entry.set_collision_deltas(name_delta, exact_delta);
            self.entries.push_front(entry);
            self.size += size;
            self.next_id += 1;
            self.newest_name.insert(name_hash, id);
            self.newest_exact.insert(exact_hash, id);
        } else {
            self.entries.push_front(entry);
            self.size += size;
            self.next_id += 1;
            if self.entries.len() > HASH_INDEX_THRESHOLD {
                self.rebuild_hash_indexes();
            }
        }
        (true, evictions)
    }

    #[cfg(test)]
    fn insert_prepared_with_hashes(&mut self, mut entry: Entry, name_hash: u64, exact_hash: u64) {
        let id = self.next_id;
        let name_delta = self.collision_delta(id, self.newest_name.get(&name_hash));
        let exact_delta = self.collision_delta(id, self.newest_exact.get(&exact_hash));
        entry.set_collision_deltas(name_delta, exact_delta);
        self.size += entry.size();
        self.entries.push_front(entry);
        self.next_id += 1;
        self.newest_name.insert(name_hash, id);
        self.newest_exact.insert(exact_hash, id);
    }

    fn collision_delta(&self, id: u64, previous: Option<&u64>) -> u32 {
        previous.map_or(0, |previous| {
            u32::try_from(id - previous)
                .expect("retained collision chain is bounded by the local table ceiling")
        })
    }

    fn ensure_id_space(&mut self, additional: usize) -> Result<(), Error> {
        let additional = u64::try_from(additional).map_err(|_| Error::StateOverflow)?;
        if self.next_id.checked_add(additional).is_some() {
            return Ok(());
        }
        let oldest = self
            .next_id
            .checked_sub(self.entries.len() as u64)
            .ok_or(Error::StateOverflow)?;
        for id in self.newest_name.values_mut() {
            *id -= oldest;
        }
        for id in self.newest_exact.values_mut() {
            *id -= oldest;
        }
        self.next_id -= oldest;
        self.next_id
            .checked_add(additional)
            .map(|_| ())
            .ok_or(Error::StateOverflow)
    }

    fn evict_to_limit(&mut self) -> usize {
        let mut count = 0;
        while self.size > self.max_size {
            self.evict_one();
            count += 1;
        }
        count
    }

    fn clear_retaining_storage(&mut self) -> usize {
        let count = self.entries.len();
        self.entries.clear_retaining_storage();
        self.size = 0;
        self.drop_hash_indexes();
        count
    }

    fn clear_releasing_storage(&mut self) -> usize {
        let count = self.clear_retaining_storage();
        self.release_empty_storage();
        count
    }

    fn release_empty_storage(&mut self) {
        debug_assert_eq!(self.entries.len(), 0);
        self.entries.release_storage();
        self.drop_hash_indexes();
    }

    fn evict_one(&mut self) {
        let evicted_id = self.next_id - self.entries.len() as u64;
        let Some(entry) = self.entries.pop_back() else {
            return;
        };
        if self.uses_hash_index() {
            let name_hash = self.hash_builder.hash_one(entry.name());
            if self.newest_name.get(&name_hash) == Some(&evicted_id) {
                self.newest_name.remove(&name_hash);
            }
            let exact_hash = self.hash_builder.hash_one((entry.name(), entry.value()));
            if self.newest_exact.get(&exact_hash) == Some(&evicted_id) {
                self.newest_exact.remove(&exact_hash);
            }
            if self.entries.len() <= HASH_INDEX_THRESHOLD {
                self.drop_hash_indexes();
            }
        }
        self.size -= entry.size();
    }

    fn offset_for_id(&self, id: u64) -> Option<usize> {
        let newest_id = self.next_id.checked_sub(1)?;
        let offset = usize::try_from(newest_id.checked_sub(id)?).ok()?;
        (offset < self.entries.len()).then_some(offset)
    }

    #[cfg(any(test, feature = "hpack-test-support"))]
    pub(crate) fn snapshot(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        (0..self.entries.len())
            .map(|offset| {
                let entry = self.entries.get(offset).unwrap();
                (entry.name().to_vec(), entry.value().to_vec())
            })
            .collect()
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn test_snapshot(&self) -> TestTableSnapshot {
        TestTableSnapshot {
            max_size: self.max_size,
            size: self.size,
            entries: self.snapshot(),
            container_capacities: [
                self.entries.capacity(),
                self.newest_name.capacity(),
                self.newest_exact.capacity(),
            ],
        }
    }

    #[cfg(test)]
    fn take_lookup_work(&self) -> usize {
        self.lookup_work.replace(0)
    }
}

impl<'a> IndexedHeaderFieldResolver<'a> {
    pub(crate) fn resolve(self, source: IndexedHeaderFieldSource) -> Option<HeaderFieldRef<'a>> {
        match source {
            IndexedHeaderFieldSource::Static(index) => {
                let (name, value) = *STATIC_TABLE.get(index.checked_sub(1)?)?;
                Some(HeaderFieldRef {
                    name,
                    value,
                    sensitive: false,
                })
            }
            IndexedHeaderFieldSource::Dynamic(id) => {
                let entry = self.table.entries.get(self.table.offset_for_id(id)?)?;
                Some(HeaderFieldRef {
                    name: entry.name(),
                    value: entry.value(),
                    sensitive: false,
                })
            }
        }
    }
}

impl<'a> IndexedHeaderFieldEvictions<'a> {
    pub(crate) fn must_materialize(self, source: IndexedHeaderFieldSource) -> bool {
        matches!(
            source,
            IndexedHeaderFieldSource::Dynamic(id) if id < self.first_retained_dynamic_id
        )
    }

    pub(crate) fn resolve(self, source: IndexedHeaderFieldSource) -> Option<HeaderFieldRef<'a>> {
        self.resolver.resolve(source)
    }
}

pub(crate) struct Encoder {
    table: DynamicTable,
    pending_min_size: Option<usize>,
    pending_final_size: Option<usize>,
    diagnostics: Diagnostics,
    allocations: AllocationGate,
    planned_lookup_work: PlannedLookupWork,
    #[cfg(test)]
    forced_planned_lookup_hash: Option<u64>,
}

pub(crate) trait EncodeOutput {
    fn try_reserve_exact(&mut self, encoded_len: usize) -> Result<(), Error>;
    fn push(&mut self, byte: u8);
    fn extend_from_slice(&mut self, bytes: &[u8]);
    fn encoded_len(&self) -> usize;
}

pub(crate) trait EncodePreflightVisitor {
    type Output;
    type Error;
    const FINISH_AFTER_ENCODE_ERROR: bool;

    fn visit(&mut self, field: HeaderFieldRef<'_>);
    fn finish(self) -> Result<Self::Output, Self::Error>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum EncodePreflightError<VisitorError> {
    Encode(Error),
    Visitor(VisitorError),
}

struct IgnoreEncodePreflight;

impl EncodePreflightVisitor for IgnoreEncodePreflight {
    type Output = ();
    type Error = core::convert::Infallible;
    const FINISH_AFTER_ENCODE_ERROR: bool = false;

    fn visit(&mut self, _field: HeaderFieldRef<'_>) {}

    fn finish(self) -> Result<Self::Output, Self::Error> {
        Ok(())
    }
}

impl EncodeOutput for Vec<u8> {
    fn try_reserve_exact(&mut self, encoded_len: usize) -> Result<(), Error> {
        Vec::try_reserve_exact(self, encoded_len).map_err(|_| Error::AllocationFailed)
    }

    fn push(&mut self, byte: u8) {
        Vec::push(self, byte);
    }

    fn extend_from_slice(&mut self, bytes: &[u8]) {
        Vec::extend_from_slice(self, bytes);
    }

    fn encoded_len(&self) -> usize {
        self.len()
    }
}

#[derive(Clone, Copy)]
struct StagedField<'a> {
    field: HeaderFieldRef<'a>,
    planning_active: bool,
    insertion_ordinal: Option<usize>,
    // Static indexes are permanent. Dynamic indexes are cached only before
    // any pending resize or staged insertion can shift them.
    exact_index: Option<usize>,
    name_index: Option<usize>,
}

const INLINE_STAGED_FIELDS: usize = 16;
const INLINE_PLANNED_LOOKUP_BUCKETS: usize = INLINE_STAGED_FIELDS * 2;

#[derive(Clone, Default)]
struct PlannedLookupWork {
    #[cfg(test)]
    exact: Cell<usize>,
    #[cfg(test)]
    name: Cell<usize>,
    #[cfg(test)]
    eviction: Cell<usize>,
}

impl PlannedLookupWork {
    fn record_exact(&self) {
        #[cfg(test)]
        self.exact.set(self.exact.get().saturating_add(1));
    }

    fn record_name(&self) {
        #[cfg(test)]
        self.name.set(self.name.get().saturating_add(1));
    }

    fn record(&self, kind: PlannedLookupKind) {
        match kind {
            PlannedLookupKind::Name => self.record_name(),
            PlannedLookupKind::Exact => self.record_exact(),
        }
    }

    fn record_eviction(&self) {
        #[cfg(test)]
        self.eviction.set(self.eviction.get().saturating_add(1));
    }

    #[cfg(test)]
    fn take(&self) -> (usize, usize) {
        (self.exact.replace(0), self.name.replace(0))
    }

    #[cfg(test)]
    fn take_eviction(&self) -> usize {
        self.eviction.replace(0)
    }
}

#[derive(Clone, Copy)]
struct PlannedLookupSlot {
    hash: u64,
    staged_index: usize,
}

#[derive(Clone, Copy, Default)]
struct PlannedLookupBucket {
    name: Option<PlannedLookupSlot>,
    exact: Option<PlannedLookupSlot>,
}

#[derive(Clone, Copy)]
enum PlannedLookupKind {
    Name,
    Exact,
}

#[derive(Clone, Copy)]
enum PlannedLookupKey<'a> {
    Name(&'a [u8]),
    Exact(&'a [u8], &'a [u8]),
}

impl PlannedLookupKey<'_> {
    const fn kind(self) -> PlannedLookupKind {
        match self {
            Self::Name(_) => PlannedLookupKind::Name,
            Self::Exact(_, _) => PlannedLookupKind::Exact,
        }
    }

    fn matches(self, field: HeaderFieldRef<'_>) -> bool {
        match self {
            Self::Name(name) => field.name == name,
            Self::Exact(name, value) => field.name == name && field.value == value,
        }
    }
}

struct PlannedLookupTables {
    inline: [PlannedLookupBucket; INLINE_PLANNED_LOOKUP_BUCKETS],
    overflow: Vec<PlannedLookupBucket>,
    #[cfg(test)]
    forced_hash: Option<u64>,
}

impl PlannedLookupTables {
    fn new() -> Self {
        Self {
            inline: [PlannedLookupBucket::default(); INLINE_PLANNED_LOOKUP_BUCKETS],
            overflow: Vec::new(),
            #[cfg(test)]
            forced_hash: None,
        }
    }

    fn name_hash(&self, hash: u64) -> u64 {
        #[cfg(test)]
        if let Some(forced) = self.forced_hash {
            return forced;
        }
        hash
    }

    fn exact_hash(&self, hash: u64) -> u64 {
        #[cfg(test)]
        if let Some(forced) = self.forced_hash {
            return forced;
        }
        hash
    }

    #[cfg(test)]
    fn set_forced_hash(&mut self, hash: Option<u64>) {
        self.forced_hash = hash;
    }

    fn try_prepare_for_insertion(
        &mut self,
        insertion_count: usize,
        allocations: &mut AllocationGate,
    ) -> Result<(), Error> {
        let minimum = insertion_count.checked_mul(2).ok_or(Error::StateOverflow)?;
        if minimum <= self.buckets().len() {
            return Ok(());
        }
        let capacity = minimum
            .checked_next_power_of_two()
            .ok_or(Error::StateOverflow)?;
        let mut next = Vec::new();
        allocations.try_reserve_vec(&mut next, capacity, AllocationPurpose::Other)?;
        next.resize(capacity, PlannedLookupBucket::default());
        for bucket in self.buckets().iter().copied() {
            if let Some(slot) = bucket.name {
                Self::insert_raw(&mut next, PlannedLookupKind::Name, slot)?;
            }
            if let Some(slot) = bucket.exact {
                Self::insert_raw(&mut next, PlannedLookupKind::Exact, slot)?;
            }
        }
        self.overflow = next;
        Ok(())
    }

    fn buckets(&self) -> &[PlannedLookupBucket] {
        if self.overflow.is_empty() {
            &self.inline
        } else {
            &self.overflow
        }
    }

    fn buckets_mut(&mut self) -> &mut [PlannedLookupBucket] {
        if self.overflow.is_empty() {
            &mut self.inline
        } else {
            &mut self.overflow
        }
    }

    fn slot(bucket: PlannedLookupBucket, kind: PlannedLookupKind) -> Option<PlannedLookupSlot> {
        match kind {
            PlannedLookupKind::Name => bucket.name,
            PlannedLookupKind::Exact => bucket.exact,
        }
    }

    fn set_slot(
        bucket: &mut PlannedLookupBucket,
        kind: PlannedLookupKind,
        slot: PlannedLookupSlot,
    ) {
        match kind {
            PlannedLookupKind::Name => bucket.name = Some(slot),
            PlannedLookupKind::Exact => bucket.exact = Some(slot),
        }
    }

    fn insert_raw(
        buckets: &mut [PlannedLookupBucket],
        kind: PlannedLookupKind,
        slot: PlannedLookupSlot,
    ) -> Result<(), Error> {
        let mask = buckets.len().checked_sub(1).ok_or(Error::StateOverflow)?;
        let start = slot.hash as usize & mask;
        for probe in 0..buckets.len() {
            let bucket = &mut buckets[(start + probe) & mask];
            if Self::slot(*bucket, kind).is_none() {
                Self::set_slot(bucket, kind, slot);
                return Ok(());
            }
        }
        Err(Error::StateOverflow)
    }

    fn find(
        &self,
        inline: &[Option<StagedField<'_>>; INLINE_STAGED_FIELDS],
        overflow: &[StagedField<'_>],
        hash: u64,
        key: PlannedLookupKey<'_>,
        work: &PlannedLookupWork,
    ) -> Result<Option<usize>, Error> {
        let buckets = self.buckets();
        let mask = buckets.len().checked_sub(1).ok_or(Error::StateOverflow)?;
        let start = hash as usize & mask;
        for probe in 0..buckets.len() {
            work.record(key.kind());
            let bucket = buckets[(start + probe) & mask];
            let Some(slot) = Self::slot(bucket, key.kind()) else {
                return Ok(None);
            };
            if slot.hash != hash {
                continue;
            }
            let staged =
                staged_field_at(inline, overflow, slot.staged_index).ok_or(Error::StateOverflow)?;
            if key.matches(staged.field) {
                return Ok(staged.planning_active.then_some(slot.staged_index));
            }
        }
        Err(Error::StateOverflow)
    }

    fn insert(
        &mut self,
        inline: &[Option<StagedField<'_>>; INLINE_STAGED_FIELDS],
        overflow: &[StagedField<'_>],
        hash: u64,
        key: PlannedLookupKey<'_>,
        staged_index: usize,
    ) -> Result<(), Error> {
        let buckets = self.buckets_mut();
        let mask = buckets.len().checked_sub(1).ok_or(Error::StateOverflow)?;
        let start = hash as usize & mask;
        for probe in 0..buckets.len() {
            let index = (start + probe) & mask;
            if let Some(slot) = Self::slot(buckets[index], key.kind()) {
                if slot.hash != hash {
                    continue;
                }
                let staged = staged_field_at(inline, overflow, slot.staged_index)
                    .ok_or(Error::StateOverflow)?;
                if !key.matches(staged.field) {
                    continue;
                }
            }
            Self::set_slot(
                &mut buckets[index],
                key.kind(),
                PlannedLookupSlot { hash, staged_index },
            );
            return Ok(());
        }
        Err(Error::StateOverflow)
    }
}

fn staged_field_at<'s, 'a>(
    inline: &'s [Option<StagedField<'a>>; INLINE_STAGED_FIELDS],
    overflow: &'s [StagedField<'a>],
    index: usize,
) -> Option<&'s StagedField<'a>> {
    inline
        .get(index)
        .and_then(Option::as_ref)
        .or_else(|| overflow.get(index.saturating_sub(INLINE_STAGED_FIELDS)))
}

fn staged_field_at_mut<'s, 'a>(
    inline: &'s mut [Option<StagedField<'a>>; INLINE_STAGED_FIELDS],
    overflow: &'s mut [StagedField<'a>],
    index: usize,
) -> Option<&'s mut StagedField<'a>> {
    inline
        .get_mut(index)
        .and_then(Option::as_mut)
        .or_else(|| overflow.get_mut(index.saturating_sub(INLINE_STAGED_FIELDS)))
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn planned_dynamic_exact_scan(
    table: &DynamicTable,
    existing_count: usize,
    inline: &[Option<StagedField<'_>>; INLINE_STAGED_FIELDS],
    overflow: &[StagedField<'_>],
    planned_count: usize,
    planned_active_count: usize,
    name: &[u8],
    value: &[u8],
) -> Option<usize> {
    let mut offset = 0;
    for index in (0..planned_count).rev() {
        let staged = staged_field_at(inline, overflow, index)?;
        if staged.planning_active {
            if staged.field.name == name && staged.field.value == value {
                return Some(STATIC_TABLE_LEN + 1 + offset);
            }
            offset += 1;
        }
    }
    if existing_count == table.entries.len() {
        return table
            .find_exact(name, value)
            .map(|index| index + planned_active_count);
    }
    (0..existing_count)
        .find(|offset| {
            table
                .entries
                .get(*offset)
                .is_some_and(|entry| entry.name() == name && entry.value() == value)
        })
        .map(|offset| STATIC_TABLE_LEN + 1 + planned_active_count + offset)
}

fn planned_staged_index(staged: &StagedField<'_>, insertion_count: usize) -> Result<usize, Error> {
    let ordinal = staged.insertion_ordinal.ok_or(Error::StateOverflow)?;
    let after_ordinal = ordinal.checked_add(1).ok_or(Error::StateOverflow)?;
    let newer_count = insertion_count
        .checked_sub(after_ordinal)
        .ok_or(Error::StateOverflow)?;
    STATIC_TABLE_LEN
        .checked_add(1)
        .and_then(|index| index.checked_add(newer_count))
        .ok_or(Error::StateOverflow)
}

#[allow(clippy::too_many_arguments)]
fn planned_dynamic_exact(
    table: &DynamicTable,
    existing_count: usize,
    inline: &[Option<StagedField<'_>>; INLINE_STAGED_FIELDS],
    overflow: &[StagedField<'_>],
    _planned_count: usize,
    insertion_count: usize,
    planned_active_count: usize,
    lookups: Option<&PlannedLookupTables>,
    lookup_work: &PlannedLookupWork,
    hash_builder: &FxBuildHasher,
    hash: &mut Option<u64>,
    name: &[u8],
    value: &[u8],
) -> Result<Option<usize>, Error> {
    let result = (|| {
        if planned_active_count != 0 {
            let lookups = lookups.ok_or(Error::StateOverflow)?;
            let actual_hash = *hash.get_or_insert_with(|| hash_builder.hash_one((name, value)));
            if let Some(staged_index) = lookups.find(
                inline,
                overflow,
                lookups.exact_hash(actual_hash),
                PlannedLookupKey::Exact(name, value),
                lookup_work,
            )? {
                let staged =
                    staged_field_at(inline, overflow, staged_index).ok_or(Error::StateOverflow)?;
                return planned_staged_index(staged, insertion_count).map(Some);
            }
        }
        if existing_count == table.entries.len() {
            let table_hash = table
                .uses_hash_index()
                .then(|| *hash.get_or_insert_with(|| hash_builder.hash_one((name, value))));
            return table
                .find_exact_for_plan(name, value, table_hash)?
                .map(|index| {
                    index
                        .checked_add(planned_active_count)
                        .ok_or(Error::StateOverflow)
                })
                .transpose();
        }
        (0..existing_count)
            .find(|offset| {
                table
                    .entries
                    .get(*offset)
                    .is_some_and(|entry| entry.name() == name && entry.value() == value)
            })
            .map(|offset| {
                STATIC_TABLE_LEN
                    .checked_add(1)
                    .and_then(|index| index.checked_add(planned_active_count))
                    .and_then(|index| index.checked_add(offset))
                    .ok_or(Error::StateOverflow)
            })
            .transpose()
    })();
    #[cfg(test)]
    if let Ok(index) = result {
        assert_eq!(
            index,
            planned_dynamic_exact_scan(
                table,
                existing_count,
                inline,
                overflow,
                _planned_count,
                planned_active_count,
                name,
                value,
            ),
            "planned exact lookup differs from the scan oracle"
        );
    }
    result
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn planned_dynamic_name_scan(
    table: &DynamicTable,
    existing_count: usize,
    inline: &[Option<StagedField<'_>>; INLINE_STAGED_FIELDS],
    overflow: &[StagedField<'_>],
    planned_count: usize,
    planned_active_count: usize,
    name: &[u8],
) -> Option<usize> {
    let mut offset = 0;
    for index in (0..planned_count).rev() {
        let staged = staged_field_at(inline, overflow, index)?;
        if staged.planning_active {
            if staged.field.name == name {
                return Some(STATIC_TABLE_LEN + 1 + offset);
            }
            offset += 1;
        }
    }
    if existing_count == table.entries.len() {
        return table
            .find_name(name)
            .map(|index| index + planned_active_count);
    }
    (0..existing_count)
        .find(|offset| {
            table
                .entries
                .get(*offset)
                .is_some_and(|entry| entry.name() == name)
        })
        .map(|offset| STATIC_TABLE_LEN + 1 + planned_active_count + offset)
}

#[allow(clippy::too_many_arguments)]
fn planned_dynamic_name(
    table: &DynamicTable,
    existing_count: usize,
    inline: &[Option<StagedField<'_>>; INLINE_STAGED_FIELDS],
    overflow: &[StagedField<'_>],
    _planned_count: usize,
    insertion_count: usize,
    planned_active_count: usize,
    lookups: Option<&PlannedLookupTables>,
    lookup_work: &PlannedLookupWork,
    hash_builder: &FxBuildHasher,
    hash: &mut Option<u64>,
    name: &[u8],
) -> Result<Option<usize>, Error> {
    let result = (|| {
        if planned_active_count != 0 {
            let lookups = lookups.ok_or(Error::StateOverflow)?;
            let actual_hash = *hash.get_or_insert_with(|| hash_builder.hash_one(name));
            if let Some(staged_index) = lookups.find(
                inline,
                overflow,
                lookups.name_hash(actual_hash),
                PlannedLookupKey::Name(name),
                lookup_work,
            )? {
                let staged =
                    staged_field_at(inline, overflow, staged_index).ok_or(Error::StateOverflow)?;
                return planned_staged_index(staged, insertion_count).map(Some);
            }
        }
        if existing_count == table.entries.len() {
            let table_hash = table
                .uses_hash_index()
                .then(|| *hash.get_or_insert_with(|| hash_builder.hash_one(name)));
            return table
                .find_name_for_plan(name, table_hash)?
                .map(|index| {
                    index
                        .checked_add(planned_active_count)
                        .ok_or(Error::StateOverflow)
                })
                .transpose();
        }
        (0..existing_count)
            .find(|offset| {
                table
                    .entries
                    .get(*offset)
                    .is_some_and(|entry| entry.name() == name)
            })
            .map(|offset| {
                STATIC_TABLE_LEN
                    .checked_add(1)
                    .and_then(|index| index.checked_add(planned_active_count))
                    .and_then(|index| index.checked_add(offset))
                    .ok_or(Error::StateOverflow)
            })
            .transpose()
    })();
    #[cfg(test)]
    if let Ok(index) = result {
        assert_eq!(
            index,
            planned_dynamic_name_scan(
                table,
                existing_count,
                inline,
                overflow,
                _planned_count,
                planned_active_count,
                name,
            ),
            "planned name lookup differs from the scan oracle"
        );
    }
    result
}

impl Clone for Encoder {
    fn clone(&self) -> Self {
        Self {
            table: self.table.clone(),
            pending_min_size: self.pending_min_size,
            pending_final_size: self.pending_final_size,
            diagnostics: self.diagnostics,
            allocations: self.allocations.clone(),
            planned_lookup_work: self.planned_lookup_work.clone(),
            #[cfg(test)]
            forced_planned_lookup_hash: self.forced_planned_lookup_hash,
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.table.clone_from(&source.table);
        self.pending_min_size = source.pending_min_size;
        self.pending_final_size = source.pending_final_size;
        self.diagnostics = source.diagnostics;
        self.allocations.clone_from(&source.allocations);
        self.planned_lookup_work
            .clone_from(&source.planned_lookup_work);
        #[cfg(test)]
        {
            self.forced_planned_lookup_hash = source.forced_planned_lookup_hash;
        }
    }
}

impl Default for Encoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Encoder {
    pub(crate) fn new() -> Self {
        Self {
            table: DynamicTable::new(DEFAULT_TABLE_SIZE),
            pending_min_size: None,
            pending_final_size: None,
            diagnostics: Diagnostics::default(),
            allocations: AllocationGate::default(),
            planned_lookup_work: PlannedLookupWork::default(),
            #[cfg(test)]
            forced_planned_lookup_hash: None,
        }
    }

    pub(crate) fn try_clone(&self) -> Result<Self, Error> {
        let mut cloned = Self::new();
        cloned.try_clone_from(self)?;
        Ok(cloned)
    }

    pub(crate) fn try_clone_from(&mut self, source: &Self) -> Result<(), Error> {
        let mut allocations = source.allocations.clone();
        self.table.try_clone_from(&source.table, &mut allocations)?;
        self.pending_min_size = source.pending_min_size;
        self.pending_final_size = source.pending_final_size;
        self.diagnostics = source.diagnostics;
        self.allocations = allocations;
        self.planned_lookup_work
            .clone_from(&source.planned_lookup_work);
        #[cfg(test)]
        {
            self.forced_planned_lookup_hash = source.forced_planned_lookup_hash;
        }
        Ok(())
    }

    pub(crate) fn set_max_table_size(&mut self, size: usize) {
        let size = size.min(MAX_TABLE_SIZE);
        self.pending_min_size = Some(
            self.pending_min_size
                .map_or(size, |pending| pending.min(size)),
        );
        self.pending_final_size = Some(size);
    }

    #[cfg(any(test, feature = "hpack-test-support"))]
    pub(crate) fn set_allocation_failure_after(&mut self, successful_allocations: Option<usize>) {
        self.allocations.set_failure_after(successful_allocations);
    }

    pub(crate) fn diagnostics(&self) -> Diagnostics {
        self.diagnostics
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn set_diagnostics_for_testing(&mut self, diagnostics: Diagnostics) {
        self.diagnostics = diagnostics;
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn test_table_snapshot(&self) -> TestTableSnapshot {
        self.table.test_snapshot()
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn test_configured_max_size(&self) -> usize {
        self.pending_final_size.unwrap_or(self.table.max_size)
    }

    #[cfg(test)]
    pub(crate) fn encode(&mut self, fields: &[HeaderField]) -> Result<Vec<u8>, Error> {
        self.encode_by(fields.len(), |index| {
            let field = &fields[index];
            HeaderFieldRef {
                name: &field.name,
                value: &field.value,
                sensitive: field.sensitive,
            }
        })
    }

    #[cfg(test)]
    fn take_planned_lookup_work(&self) -> (usize, usize) {
        self.planned_lookup_work.take()
    }

    #[cfg(test)]
    fn set_forced_planned_lookup_hash(&mut self, hash: Option<u64>) {
        self.forced_planned_lookup_hash = hash;
    }

    pub(crate) fn encode_by<'a>(
        &mut self,
        field_count: usize,
        field_at: impl FnMut(usize) -> HeaderFieldRef<'a>,
    ) -> Result<Vec<u8>, Error> {
        let mut output = Vec::new();
        self.encode_by_into(field_count, field_at, &mut output)?;
        Ok(output)
    }

    pub(crate) fn encode_by_into<'a>(
        &mut self,
        field_count: usize,
        field_at: impl FnMut(usize) -> HeaderFieldRef<'a>,
        output: &mut impl EncodeOutput,
    ) -> Result<(), Error> {
        match self.encode_by_into_with_visitor(field_count, field_at, IgnoreEncodePreflight, output)
        {
            Ok(()) => Ok(()),
            Err(EncodePreflightError::Encode(error)) => Err(error),
            Err(EncodePreflightError::Visitor(error)) => match error {},
        }
    }

    pub(crate) fn encode_by_into_with_visitor<'a, Visitor>(
        &mut self,
        field_count: usize,
        mut field_at: impl FnMut(usize) -> HeaderFieldRef<'a>,
        mut visitor: Visitor,
        output: &mut impl EncodeOutput,
    ) -> Result<Visitor::Output, EncodePreflightError<Visitor::Error>>
    where
        Visitor: EncodePreflightVisitor,
    {
        let target_size = self.pending_final_size.unwrap_or(self.table.max_size);
        let mut output_len = 0usize;
        if let Some(minimum) = self.pending_min_size {
            output_len = output_len
                .checked_add(encoded_integer_len(minimum, 5))
                .ok_or(EncodePreflightError::Encode(Error::AllocationFailed))?;
            if self.pending_final_size != Some(minimum) {
                output_len = output_len
                    .checked_add(encoded_integer_len(target_size, 5))
                    .ok_or(EncodePreflightError::Encode(Error::AllocationFailed))?;
            }
        }

        let mut inline_staged_fields: [Option<StagedField<'a>>; INLINE_STAGED_FIELDS] =
            [None; INLINE_STAGED_FIELDS];
        let mut overflow_staged_fields = Vec::new();
        let mut planned_lookups = None;
        let mut visited_fields = 0usize;
        let preflight = (|| {
            self.allocations.try_reserve_vec(
                &mut overflow_staged_fields,
                field_count.saturating_sub(INLINE_STAGED_FIELDS),
                AllocationPurpose::Other,
            )?;
            // Simulate resize, insertion, and eviction order without mutating
            // the encoder so every staged index and byte count is exact.
            let mut existing_count = self.table.entries.len();
            let mut simulated_size = self.table.size;
            let initial_size = self.pending_min_size.unwrap_or(self.table.max_size);
            while simulated_size > initial_size && existing_count != 0 {
                existing_count -= 1;
                simulated_size -= self
                    .table
                    .entries
                    .get(existing_count)
                    .expect("retained entry count is in bounds")
                    .size();
            }
            let mut planned_active_count = 0usize;
            let mut insertion_candidates = 0usize;
            let mut oldest_active_staged_index = 0usize;
            for index in 0..field_count {
                let field = field_at(index);
                visitor.visit(field);
                visited_fields = index + 1;
                let mut exact_hash = None;
                let mut name_hash = None;
                let field_size = field
                    .name
                    .len()
                    .checked_add(field.value.len())
                    .and_then(|value| value.checked_add(32))
                    .ok_or(Error::FieldSizeOverflow)?;
                let exact_index = if field.sensitive {
                    None
                } else if let Some(index) = find_static_exact(field.name, field.value) {
                    Some(index)
                } else {
                    planned_dynamic_exact(
                        &self.table,
                        existing_count,
                        &inline_staged_fields,
                        &overflow_staged_fields,
                        index,
                        insertion_candidates,
                        planned_active_count,
                        planned_lookups.as_ref(),
                        &self.planned_lookup_work,
                        &self.table.hash_builder,
                        &mut exact_hash,
                        field.name,
                        field.value,
                    )?
                };
                let name_index = if !field.sensitive && exact_index.is_some() {
                    None
                } else if field.sensitive {
                    let mut selected = planned_dynamic_exact(
                        &self.table,
                        existing_count,
                        &inline_staged_fields,
                        &overflow_staged_fields,
                        index,
                        insertion_candidates,
                        planned_active_count,
                        planned_lookups.as_ref(),
                        &self.planned_lookup_work,
                        &self.table.hash_builder,
                        &mut exact_hash,
                        field.name,
                        field.value,
                    )?;
                    if selected.is_none() {
                        selected = find_static_exact(field.name, field.value);
                    }
                    if selected.is_none() {
                        selected = planned_dynamic_name(
                            &self.table,
                            existing_count,
                            &inline_staged_fields,
                            &overflow_staged_fields,
                            index,
                            insertion_candidates,
                            planned_active_count,
                            planned_lookups.as_ref(),
                            &self.planned_lookup_work,
                            &self.table.hash_builder,
                            &mut name_hash,
                            field.name,
                        )?;
                    }
                    selected.or_else(|| find_static_name(field.name))
                } else {
                    planned_dynamic_name(
                        &self.table,
                        existing_count,
                        &inline_staged_fields,
                        &overflow_staged_fields,
                        index,
                        insertion_candidates,
                        planned_active_count,
                        planned_lookups.as_ref(),
                        &self.planned_lookup_work,
                        &self.table.hash_builder,
                        &mut name_hash,
                        field.name,
                    )?
                    .or_else(|| find_static_name(field.name))
                };
                let needs_insertion =
                    !field.sensitive && field_size <= target_size && exact_index.is_none();
                let insertion_ordinal = if needs_insertion {
                    let ordinal = insertion_candidates;
                    insertion_candidates = insertion_candidates
                        .checked_add(1)
                        .ok_or(Error::StateOverflow)?;
                    if planned_lookups.is_none() {
                        #[cfg(not(test))]
                        let lookups = PlannedLookupTables::new();
                        #[cfg(test)]
                        let mut lookups = PlannedLookupTables::new();
                        #[cfg(test)]
                        lookups.set_forced_hash(self.forced_planned_lookup_hash);
                        planned_lookups = Some(lookups);
                    }
                    planned_lookups
                        .as_mut()
                        .ok_or(Error::StateOverflow)?
                        .try_prepare_for_insertion(insertion_candidates, &mut self.allocations)?;
                    Some(ordinal)
                } else {
                    None
                };
                let kind = if field.sensitive {
                    LiteralKind::NeverIndexed
                } else if field_size <= target_size {
                    LiteralKind::Incremental
                } else {
                    LiteralKind::WithoutIndexing
                };
                let encoded_len = if let Some(index) = exact_index {
                    encoded_integer_len(index, 7)
                } else {
                    encoded_literal_len(name_index, field.name, field.value, kind)
                };
                output_len = output_len
                    .checked_add(encoded_len)
                    .ok_or(Error::AllocationFailed)?;
                let staged_field = StagedField {
                    field,
                    planning_active: needs_insertion,
                    insertion_ordinal,
                    exact_index,
                    name_index,
                };
                if let Some(slot) = inline_staged_fields.get_mut(index) {
                    *slot = Some(staged_field);
                } else {
                    overflow_staged_fields.push(staged_field);
                }
                if needs_insertion {
                    let lookups = planned_lookups.as_mut().ok_or(Error::StateOverflow)?;
                    let exact_hash = lookups.exact_hash(*exact_hash.get_or_insert_with(|| {
                        self.table.hash_builder.hash_one((field.name, field.value))
                    }));
                    lookups.insert(
                        &inline_staged_fields,
                        &overflow_staged_fields,
                        exact_hash,
                        PlannedLookupKey::Exact(field.name, field.value),
                        index,
                    )?;
                    let name_hash = lookups.name_hash(
                        *name_hash
                            .get_or_insert_with(|| self.table.hash_builder.hash_one(field.name)),
                    );
                    lookups.insert(
                        &inline_staged_fields,
                        &overflow_staged_fields,
                        name_hash,
                        PlannedLookupKey::Name(field.name),
                        index,
                    )?;
                    planned_active_count = planned_active_count
                        .checked_add(1)
                        .ok_or(Error::StateOverflow)?;
                    simulated_size = simulated_size
                        .checked_add(field_size)
                        .ok_or(Error::StateOverflow)?;
                    while simulated_size > target_size {
                        if existing_count != 0 {
                            existing_count -= 1;
                            simulated_size -= self
                                .table
                                .entries
                                .get(existing_count)
                                .expect("retained entry count is in bounds")
                                .size();
                            continue;
                        }
                        while !staged_field_at(
                            &inline_staged_fields,
                            &overflow_staged_fields,
                            oldest_active_staged_index,
                        )
                        .is_some_and(|staged| staged.planning_active)
                        {
                            self.planned_lookup_work.record_eviction();
                            oldest_active_staged_index = oldest_active_staged_index
                                .checked_add(1)
                                .ok_or(Error::StateOverflow)?;
                        }
                        self.planned_lookup_work.record_eviction();
                        let evicted = staged_field_at_mut(
                            &mut inline_staged_fields,
                            &mut overflow_staged_fields,
                            oldest_active_staged_index,
                        )
                        .expect("the active staged entry exists");
                        evicted.planning_active = false;
                        simulated_size -= evicted
                            .field
                            .name
                            .len()
                            .saturating_add(evicted.field.value.len())
                            .saturating_add(32);
                        planned_active_count -= 1;
                        oldest_active_staged_index = oldest_active_staged_index
                            .checked_add(1)
                            .ok_or(Error::StateOverflow)?;
                    }
                }
            }
            Ok(insertion_candidates)
        })();
        let insertion_candidates = match preflight {
            Ok(insertion_candidates) => insertion_candidates,
            Err(encode_error) => {
                if !Visitor::FINISH_AFTER_ENCODE_ERROR {
                    return Err(EncodePreflightError::Encode(encode_error));
                }
                for index in visited_fields..field_count {
                    visitor.visit(field_at(index));
                }
                return match visitor.finish() {
                    Ok(_) => Err(EncodePreflightError::Encode(encode_error)),
                    Err(visitor_error) => Err(EncodePreflightError::Visitor(visitor_error)),
                };
            }
        };
        let visitor_output = visitor.finish().map_err(EncodePreflightError::Visitor)?;
        let mut prepared_entries = Vec::new();
        if insertion_candidates != 0 {
            self.allocations
                .try_reserve_vec(
                    &mut prepared_entries,
                    insertion_candidates,
                    AllocationPurpose::Other,
                )
                .map_err(EncodePreflightError::Encode)?;
            for staged_field in inline_staged_fields
                .iter()
                .filter_map(Option::as_ref)
                .chain(overflow_staged_fields.iter())
            {
                if staged_field.insertion_ordinal.is_some() {
                    prepared_entries.push(
                        Entry::try_new(
                            staged_field.field.name,
                            staged_field.field.value,
                            &mut self.allocations,
                        )
                        .map_err(EncodePreflightError::Encode)?,
                    );
                }
            }
        }
        debug_assert_eq!(prepared_entries.len(), insertion_candidates);
        self.table
            .try_reserve_retained_entries(insertion_candidates, target_size, &mut self.allocations)
            .map_err(EncodePreflightError::Encode)?;
        self.table
            .ensure_id_space(insertion_candidates)
            .map_err(EncodePreflightError::Encode)?;

        if output_len != 0 {
            self.allocations
                .before_allocation()
                .map_err(EncodePreflightError::Encode)?;
        }
        output
            .try_reserve_exact(output_len)
            .map_err(EncodePreflightError::Encode)?;
        if output_len != 0 {
            self.allocations.record_success(AllocationPurpose::Other);
        }

        let mut delta = Diagnostics {
            encoded_blocks: 1,
            ..Diagnostics::default()
        };

        if let Some(minimum) = self.pending_min_size.take() {
            push_integer(output, minimum, 5, 0x20);
            delta.table_size_updates += 1;
            delta.table_evictions += self.table.set_max_size_retaining_storage(minimum) as u64;
        }
        if let Some(final_size) = self.pending_final_size.take()
            && final_size != self.table.max_size
        {
            push_integer(output, final_size, 5, 0x20);
            delta.table_size_updates += 1;
            delta.table_evictions += self.table.set_max_size_retaining_storage(final_size) as u64;
        }

        let mut prepared_entries = prepared_entries.into_iter();
        for staged_field in inline_staged_fields
            .iter()
            .filter_map(Option::as_ref)
            .chain(overflow_staged_fields.iter())
        {
            let field = staged_field.field;
            delta.field_bytes = delta.field_bytes.saturating_add(
                u64::try_from(field.name.len().saturating_add(field.value.len()))
                    .unwrap_or(u64::MAX),
            );
            if !field.sensitive
                && let Some(index) = staged_field.exact_index
            {
                push_integer(output, index, 7, 0x80);
                delta.indexed_fields += 1;
                continue;
            }

            if field.sensitive {
                push_literal(
                    output,
                    staged_field.name_index,
                    field.name,
                    field.value,
                    LiteralKind::NeverIndexed,
                    &mut delta,
                );
                delta.never_indexed_fields += 1;
                continue;
            }

            if staged_field.insertion_ordinal.is_some() {
                push_literal(
                    output,
                    staged_field.name_index,
                    field.name,
                    field.value,
                    LiteralKind::Incremental,
                    &mut delta,
                );
                let entry = prepared_entries
                    .next()
                    .expect("fitting ordinary fields are staged before mutation");
                let (inserted, evictions) = self.table.insert_prepared(entry);
                delta.table_insertions += u64::from(inserted);
                delta.table_evictions += evictions as u64;
                delta.incremental_fields += 1;
            } else {
                push_literal(
                    output,
                    staged_field.name_index,
                    field.name,
                    field.value,
                    LiteralKind::WithoutIndexing,
                    &mut delta,
                );
                delta.without_indexing_fields += 1;
            }
        }
        debug_assert!(prepared_entries.next().is_none());

        debug_assert_eq!(output.encoded_len(), output_len);
        delta.wire_bytes = u64::try_from(output.encoded_len()).unwrap_or(u64::MAX);
        self.diagnostics.add_assign(delta);
        if self.table.entries.len() == 0 {
            self.table.release_empty_storage();
        }
        Ok(visitor_output)
    }
}

pub(crate) struct Decoder {
    table: DynamicTable,
    max_allowed_table_size: usize,
    required_min_size: Option<usize>,
    terminal_error: Option<Error>,
    diagnostics: Diagnostics,
    allocations: AllocationGate,
    #[cfg(any(test, feature = "hpack-test-support"))]
    post_limit_baseline: Option<AllocationObservations>,
    #[cfg(any(test, feature = "hpack-test-support"))]
    last_post_limit_allocations: Option<AllocationObservations>,
    #[cfg(any(test, feature = "hpack-test-support"))]
    block_allocation_baseline: AllocationObservations,
    #[cfg(any(test, feature = "hpack-test-support"))]
    last_block_allocations: Option<AllocationObservations>,
}

#[cfg(feature = "hpack-test-support")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TestPostLimitAllocationSnapshot {
    pub(crate) discarded_output_allocations: usize,
    pub(crate) dynamic_table_synchronization_allocations: usize,
}

#[cfg(feature = "hpack-test-support")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TestBlockAllocationSnapshot {
    pub(crate) decoded_output_allocations: usize,
    pub(crate) dynamic_table_synchronization_allocations: usize,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub(crate) fn new() -> Self {
        Self {
            table: DynamicTable::new(DEFAULT_TABLE_SIZE),
            max_allowed_table_size: DEFAULT_TABLE_SIZE,
            required_min_size: None,
            terminal_error: None,
            diagnostics: Diagnostics::default(),
            allocations: AllocationGate::default(),
            #[cfg(any(test, feature = "hpack-test-support"))]
            post_limit_baseline: None,
            #[cfg(any(test, feature = "hpack-test-support"))]
            last_post_limit_allocations: None,
            #[cfg(any(test, feature = "hpack-test-support"))]
            block_allocation_baseline: AllocationObservations::default(),
            #[cfg(any(test, feature = "hpack-test-support"))]
            last_block_allocations: None,
        }
    }

    pub(crate) fn set_max_allowed_table_size(&mut self, size: usize) {
        let size = size.min(MAX_TABLE_SIZE);
        if size < self.table.max_size {
            self.required_min_size = Some(
                self.required_min_size
                    .map_or(size, |required| required.min(size)),
            );
        }
        self.max_allowed_table_size = size;
    }

    #[cfg(any(test, feature = "hpack-test-support"))]
    pub(crate) fn set_allocation_failure_after(&mut self, successful_allocations: Option<usize>) {
        self.allocations.set_failure_after(successful_allocations);
    }

    pub(crate) fn diagnostics(&self) -> Diagnostics {
        self.diagnostics
    }

    pub(crate) fn indexed_header_field_resolver(&self) -> IndexedHeaderFieldResolver<'_> {
        self.table.indexed_field_resolver()
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn set_diagnostics_for_testing(&mut self, diagnostics: Diagnostics) {
        self.diagnostics = diagnostics;
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn test_table_snapshot(&self) -> TestTableSnapshot {
        self.table.test_snapshot()
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn test_configured_max_size(&self) -> usize {
        self.max_allowed_table_size
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn test_post_limit_allocations(&self) -> Option<TestPostLimitAllocationSnapshot> {
        self.last_post_limit_allocations
            .map(|observed| TestPostLimitAllocationSnapshot {
                discarded_output_allocations: observed.decoded_output_allocations,
                dynamic_table_synchronization_allocations: observed
                    .dynamic_table_synchronization_allocations,
            })
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn test_block_allocations(&self) -> Option<TestBlockAllocationSnapshot> {
        self.last_block_allocations
            .map(|observed| TestBlockAllocationSnapshot {
                decoded_output_allocations: observed.decoded_output_allocations,
                dynamic_table_synchronization_allocations: observed
                    .dynamic_table_synchronization_allocations,
            })
    }

    #[cfg(any(test, feature = "hpack-test-support"))]
    fn begin_allocation_observation(&mut self) {
        self.post_limit_baseline = None;
        self.last_post_limit_allocations = None;
        self.block_allocation_baseline = self.allocations.observations();
        self.last_block_allocations = None;
    }

    #[cfg(any(test, feature = "hpack-test-support"))]
    fn finish_allocation_observation(&mut self) {
        self.last_block_allocations = Some(
            self.allocations
                .observations()
                .since(self.block_allocation_baseline),
        );
        self.last_post_limit_allocations = self
            .post_limit_baseline
            .take()
            .map(|baseline| self.allocations.observations().since(baseline));
    }

    pub(crate) fn decode(
        &mut self,
        block: &[u8],
        max_header_list_size: usize,
    ) -> Result<Vec<HeaderField>, Error> {
        self.decode_with_visitor(block, max_header_list_size, &mut IgnoreHeaderFields)
    }

    pub(crate) fn decode_with_visitor(
        &mut self,
        block: &[u8],
        max_header_list_size: usize,
        visitor: &mut impl HeaderFieldVisitor,
    ) -> Result<Vec<HeaderField>, Error> {
        let mut fields = Vec::new();
        self.decode_with_visitor_into(block, max_header_list_size, visitor, &mut fields)?;
        Ok(fields)
    }

    pub(crate) fn decode_with_visitor_into<Output: HeaderDecodeOutput>(
        &mut self,
        block: &[u8],
        max_header_list_size: usize,
        visitor: &mut impl HeaderFieldVisitor,
        fields: &mut Output,
    ) -> Result<(), Error> {
        #[cfg(any(test, feature = "hpack-test-support"))]
        self.begin_allocation_observation();
        if let Some(error) = &self.terminal_error {
            return Err(error.clone());
        }
        fields.begin_block(max_header_list_size);
        let result = self.decode_inner(block, max_header_list_size, visitor, fields);
        #[cfg(any(test, feature = "hpack-test-support"))]
        self.finish_allocation_observation();
        match result {
            Ok((mut delta, oversized, list_size)) => {
                delta.wire_bytes = u64::try_from(block.len()).unwrap_or(u64::MAX);
                if oversized {
                    delta.header_list_too_large = 1;
                }
                self.diagnostics.add_assign(delta);
                if oversized {
                    Err(Error::HeaderListTooLarge { actual: list_size })
                } else {
                    Ok(())
                }
            }
            Err(error) => {
                if error == Error::AllocationFailed {
                    self.terminal_error = Some(Error::AllocationFailed);
                    return Err(error);
                }
                let delta = Diagnostics {
                    compression_errors: 1,
                    ..Diagnostics::default()
                };
                self.diagnostics.add_assign(delta);
                self.terminal_error = Some(Error::DecoderPoisoned);
                Err(error)
            }
        }
    }

    fn decode_inner<Output: HeaderDecodeOutput>(
        &mut self,
        block: &[u8],
        max_header_list_size: usize,
        visitor: &mut impl HeaderFieldVisitor,
        fields: &mut Output,
    ) -> Result<(Diagnostics, bool, usize), Error> {
        let mut cursor = 0;
        let mut field_count = 0usize;
        let mut list_size = 0usize;
        let mut oversized = false;
        let mut saw_field = false;
        let mut size_updates = [0usize; 2];
        let mut size_update_count = 0usize;
        let mut required_min_size = self.required_min_size;
        let mut delta = Diagnostics {
            decoded_blocks: 1,
            ..Diagnostics::default()
        };

        while cursor < block.len() {
            let first = block[cursor];
            if first & 0x80 != 0 {
                if required_min_size.is_some() {
                    return Err(Error::InvalidTableSizeUpdate);
                }
                let index = decode_integer(block, &mut cursor, 7)?;
                let (source, field) = self.table.get_indexed(index).ok_or(Error::InvalidIndex)?;
                delta.field_bytes = delta.field_bytes.saturating_add(
                    u64::try_from(field.name.len().saturating_add(field.value.len()))
                        .unwrap_or(u64::MAX),
                );
                if account_lengths(
                    field.name.len(),
                    field.value.len(),
                    max_header_list_size,
                    &mut list_size,
                    &mut oversized,
                ) {
                    field_count = 0;
                    #[cfg(any(test, feature = "hpack-test-support"))]
                    if self.post_limit_baseline.is_none() {
                        self.post_limit_baseline = Some(self.allocations.observations());
                    }
                }
                if !oversized {
                    fields.indexed_field(field_count, source, field, &mut self.allocations)?;
                    visit_field(visitor, field.name, field.value, false);
                    field_count += 1;
                } else {
                    visit_field(visitor, field.name, field.value, false);
                }
                saw_field = true;
                delta.indexed_fields += 1;
            } else if first & 0x40 != 0 {
                if required_min_size.is_some() {
                    return Err(Error::InvalidTableSizeUpdate);
                }
                let preview = self.preview_literal(block, cursor, 6)?;
                delta.huffman_strings = delta
                    .huffman_strings
                    .saturating_add(preview.huffman_strings);
                delta.plain_strings = delta.plain_strings.saturating_add(preview.plain_strings);
                delta.field_bytes = delta.field_bytes.saturating_add(
                    u64::try_from(
                        preview
                            .decoded_name_len
                            .saturating_add(preview.decoded_value_len),
                    )
                    .unwrap_or(u64::MAX),
                );
                if account_lengths(
                    preview.decoded_name_len,
                    preview.decoded_value_len,
                    max_header_list_size,
                    &mut list_size,
                    &mut oversized,
                ) {
                    field_count = 0;
                    #[cfg(any(test, feature = "hpack-test-support"))]
                    if self.post_limit_baseline.is_none() {
                        self.post_limit_baseline = Some(self.allocations.observations());
                    }
                }
                let field_size = preview
                    .decoded_name_len
                    .checked_add(preview.decoded_value_len)
                    .and_then(|size| size.checked_add(32))
                    .ok_or(Error::FieldSizeOverflow)?;
                if field_size <= self.table.max_size {
                    let (inserted, evictions) = if oversized {
                        self.table
                            .try_reserve_insertion_for_size(field_size, &mut self.allocations)?;
                        self.table.ensure_id_space(1)?;
                        let entry = self.decode_literal_entry(
                            block,
                            &mut cursor,
                            6,
                            preview.decoded_name_len,
                            preview.decoded_value_len,
                        )?;
                        visit_field(visitor, entry.name(), entry.value(), false);
                        self.table.insert_prepared(entry)
                    } else {
                        fields.begin_field(
                            field_count,
                            preview.decoded_name_len,
                            preview.decoded_value_len,
                            false,
                            &mut self.allocations,
                        )?;
                        self.visit_literal(
                            block,
                            &mut cursor,
                            6,
                            false,
                            &mut OutputVisitor {
                                output: fields,
                                visitor,
                                field_index: field_count,
                            },
                        )?;
                        debug_assert_eq!(cursor, preview.end);
                        fields.materialize_evicted_indexed_fields(
                            self.table.indexed_field_evictions_for_insertion(field_size),
                            &mut self.allocations,
                        )?;
                        let field = fields.field(field_count);
                        let inserted =
                            self.table
                                .insert(field.name, field.value, &mut self.allocations)?;
                        field_count += 1;
                        inserted
                    };
                    delta.table_insertions += u64::from(inserted);
                    delta.table_evictions += evictions as u64;
                } else if !oversized {
                    fields.begin_field(
                        field_count,
                        preview.decoded_name_len,
                        preview.decoded_value_len,
                        false,
                        &mut self.allocations,
                    )?;
                    self.visit_literal(
                        block,
                        &mut cursor,
                        6,
                        false,
                        &mut OutputVisitor {
                            output: fields,
                            visitor,
                            field_index: field_count,
                        },
                    )?;
                    debug_assert_eq!(cursor, preview.end);
                    fields.materialize_evicted_indexed_fields(
                        self.table.indexed_field_evictions_for_clear(),
                        &mut self.allocations,
                    )?;
                    delta.table_evictions += self.table.clear_releasing_storage() as u64;
                    field_count += 1;
                } else {
                    self.visit_literal(block, &mut cursor, 6, false, visitor)?;
                    debug_assert_eq!(cursor, preview.end);
                    delta.table_evictions += self.table.clear_releasing_storage() as u64;
                }
                saw_field = true;
                delta.incremental_fields += 1;
            } else if first & 0x20 != 0 {
                if saw_field {
                    return Err(Error::TableSizeUpdateAfterField);
                }
                let size = decode_integer(block, &mut cursor, 5)?;
                if size > self.max_allowed_table_size {
                    return Err(Error::InvalidMaxDynamicSize);
                }
                if size_update_count == 2 {
                    return Err(Error::InvalidTableSizeUpdate);
                }
                if let Some(required) = required_min_size {
                    if size > required {
                        return Err(Error::InvalidTableSizeUpdate);
                    }
                    required_min_size = None;
                }
                size_updates[size_update_count] = size;
                size_update_count += 1;
                if size_update_count == 2 && size_updates[0] > size_updates[1] {
                    return Err(Error::InvalidTableSizeUpdate);
                }
                delta.table_evictions += self.table.set_max_size_releasing_storage(size) as u64;
                delta.table_size_updates += 1;
            } else {
                if required_min_size.is_some() {
                    return Err(Error::InvalidTableSizeUpdate);
                }
                let never_indexed = first & 0x10 != 0;
                let preview = self.preview_literal(block, cursor, 4)?;
                delta.huffman_strings = delta
                    .huffman_strings
                    .saturating_add(preview.huffman_strings);
                delta.plain_strings = delta.plain_strings.saturating_add(preview.plain_strings);
                delta.field_bytes = delta.field_bytes.saturating_add(
                    u64::try_from(
                        preview
                            .decoded_name_len
                            .saturating_add(preview.decoded_value_len),
                    )
                    .unwrap_or(u64::MAX),
                );
                if account_lengths(
                    preview.decoded_name_len,
                    preview.decoded_value_len,
                    max_header_list_size,
                    &mut list_size,
                    &mut oversized,
                ) {
                    field_count = 0;
                    #[cfg(any(test, feature = "hpack-test-support"))]
                    if self.post_limit_baseline.is_none() {
                        self.post_limit_baseline = Some(self.allocations.observations());
                    }
                }
                if !oversized {
                    fields.begin_field(
                        field_count,
                        preview.decoded_name_len,
                        preview.decoded_value_len,
                        never_indexed,
                        &mut self.allocations,
                    )?;
                    self.visit_literal(
                        block,
                        &mut cursor,
                        4,
                        never_indexed,
                        &mut OutputVisitor {
                            output: fields,
                            visitor,
                            field_index: field_count,
                        },
                    )?;
                    debug_assert_eq!(cursor, preview.end);
                    field_count += 1;
                } else {
                    self.visit_literal(block, &mut cursor, 4, never_indexed, visitor)?;
                    debug_assert_eq!(cursor, preview.end);
                }
                saw_field = true;
                if never_indexed {
                    delta.never_indexed_fields += 1;
                } else {
                    delta.without_indexing_fields += 1;
                }
            }
        }

        if required_min_size.is_some() {
            return Err(Error::InvalidTableSizeUpdate);
        }
        self.required_min_size = None;
        fields.truncate(field_count);
        Ok((delta, oversized, list_size))
    }

    fn visit_literal(
        &self,
        block: &[u8],
        cursor: &mut usize,
        prefix_bits: u8,
        sensitive: bool,
        visitor: &mut impl HeaderFieldVisitor,
    ) -> Result<(), Error> {
        visitor.start_field(sensitive);
        let name_index = decode_integer(block, cursor, prefix_bits)?;
        if name_index == 0 {
            visit_string(block, cursor, visitor, StringPart::Name)?;
        } else {
            let name = self
                .table
                .get(name_index)
                .map(|(name, _)| name)
                .ok_or(Error::InvalidIndex)?;
            visitor.name_bytes(name);
        }
        visitor.end_name();
        visit_string(block, cursor, visitor, StringPart::Value)?;
        visitor.end_field();
        Ok(())
    }

    fn decode_literal_entry(
        &mut self,
        block: &[u8],
        cursor: &mut usize,
        prefix_bits: u8,
        name_len: usize,
        value_len: usize,
    ) -> Result<Entry, Error> {
        let name_index = decode_integer(block, cursor, prefix_bits)?;
        let total_len = name_len
            .checked_add(value_len)
            .ok_or(Error::FieldSizeOverflow)?;
        let name_len_u32 = u32::try_from(name_len).map_err(|_| Error::FieldSizeOverflow)?;
        if u64::from(name_len_u32) > Entry::NAME_MASK {
            return Err(Error::FieldSizeOverflow);
        }
        let mut bytes = Vec::new();
        self.allocations.try_reserve_vec(
            &mut bytes,
            total_len,
            AllocationPurpose::DynamicTableSynchronization,
        )?;
        if name_index == 0 {
            decode_string_into(block, cursor, &mut bytes)?;
        } else {
            let name = self
                .table
                .get(name_index)
                .map(|(name, _)| name)
                .ok_or(Error::InvalidIndex)?;
            bytes.extend_from_slice(name);
        }
        decode_string_into(block, cursor, &mut bytes)?;
        debug_assert_eq!(bytes.len(), total_len);
        Ok(Entry {
            bytes,
            metadata: u64::from(name_len_u32),
        })
    }

    fn preview_literal(
        &self,
        block: &[u8],
        mut cursor: usize,
        prefix_bits: u8,
    ) -> Result<LiteralPreview, Error> {
        let name_index = decode_integer(block, &mut cursor, prefix_bits)?;
        let mut huffman_strings = 0;
        let mut plain_strings = 0;
        let name_len = if name_index == 0 {
            preview_string(block, &mut cursor, &mut huffman_strings, &mut plain_strings)?
        } else {
            self.table
                .get(name_index)
                .map(|(name, _)| name.len())
                .ok_or(Error::InvalidIndex)?
        };
        let value_len =
            preview_string(block, &mut cursor, &mut huffman_strings, &mut plain_strings)?;
        Ok(LiteralPreview {
            end: cursor,
            decoded_name_len: name_len,
            decoded_value_len: value_len,
            huffman_strings,
            plain_strings,
        })
    }
}

fn account_lengths(
    name_len: usize,
    value_len: usize,
    limit: usize,
    total: &mut usize,
    oversized: &mut bool,
) -> bool {
    let was_oversized = *oversized;
    let Some(size) = name_len
        .checked_add(value_len)
        .and_then(|value| value.checked_add(32))
    else {
        *total = usize::MAX;
        *oversized = true;
        return !was_oversized;
    };
    let Some(next) = total.checked_add(size) else {
        *total = usize::MAX;
        *oversized = true;
        return !was_oversized;
    };
    *total = next;
    *oversized |= next > limit;
    !was_oversized && *oversized
}

impl HeaderDecodeOutput for Vec<HeaderField> {
    fn begin_block(&mut self, _max_header_list_size: usize) {}

    fn begin_field(
        &mut self,
        index: usize,
        name_len: usize,
        value_len: usize,
        sensitive: bool,
        allocations: &mut AllocationGate,
    ) -> Result<(), Error> {
        if let Some(field) = self.get_mut(index) {
            field.name.clear();
            allocations.try_reserve_decoded_output(&mut field.name, name_len)?;
            field.value.clear();
            allocations.try_reserve_decoded_output(&mut field.value, value_len)?;
            field.sensitive = sensitive;
            return Ok(());
        }

        debug_assert_eq!(index, self.len());
        allocations.try_reserve_decoded_output(self, 1)?;
        let mut name = Vec::new();
        allocations.try_reserve_decoded_output(&mut name, name_len)?;
        let mut value = Vec::new();
        allocations.try_reserve_decoded_output(&mut value, value_len)?;
        self.push(HeaderField {
            name,
            value,
            sensitive,
        });
        Ok(())
    }

    fn name_byte(&mut self, index: usize, byte: u8) {
        self[index].name.push(byte);
    }

    fn name_bytes(&mut self, index: usize, bytes: &[u8]) {
        self[index].name.extend_from_slice(bytes);
    }

    fn end_name(&mut self, _index: usize) {}

    fn value_byte(&mut self, index: usize, byte: u8) {
        self[index].value.push(byte);
    }

    fn value_bytes(&mut self, index: usize, bytes: &[u8]) {
        self[index].value.extend_from_slice(bytes);
    }

    fn end_field(&mut self, _index: usize) {}

    fn field(&self, index: usize) -> HeaderFieldRef<'_> {
        let field = &self[index];
        HeaderFieldRef {
            name: &field.name,
            value: &field.value,
            sensitive: field.sensitive,
        }
    }

    fn truncate(&mut self, len: usize) {
        Vec::truncate(self, len);
    }
}

fn encoded_integer_len(mut value: usize, prefix_bits: u8) -> usize {
    let prefix_max = (1usize << prefix_bits) - 1;
    if value < prefix_max {
        return 1;
    }
    value -= prefix_max;
    let mut length = 2;
    while value >= 128 {
        length += 1;
        value >>= 7;
    }
    length
}

#[cfg(test)]
fn maximum_string_len(value: &[u8]) -> Result<usize, Error> {
    encoded_integer_len(value.len(), 7)
        .checked_add(value.len())
        .ok_or(Error::AllocationFailed)
}

#[cfg(test)]
fn maximum_field_output_len(
    name: &[u8],
    value: &[u8],
    sensitive: bool,
    effective_max_size: usize,
) -> Result<usize, Error> {
    let name_string_len = maximum_string_len(name)?;
    let value_string_len = maximum_string_len(value)?;
    let literal_name_len = 1usize
        .checked_add(name_string_len)
        .and_then(|length| length.checked_add(value_string_len))
        .ok_or(Error::AllocationFailed)?;
    let maximum_index = STATIC_TABLE_LEN
        .checked_add(effective_max_size / 32)
        .ok_or(Error::AllocationFailed)?;
    let literal_kind = if sensitive {
        LiteralKind::NeverIndexed
    } else if name
        .len()
        .checked_add(value.len())
        .and_then(|length| length.checked_add(32))
        .ok_or(Error::FieldSizeOverflow)?
        <= effective_max_size
    {
        LiteralKind::Incremental
    } else {
        LiteralKind::WithoutIndexing
    };
    let indexed_name_len = encoded_integer_len(maximum_index, literal_kind.prefix_bits())
        .checked_add(value_string_len)
        .ok_or(Error::AllocationFailed)?;
    let indexed_field_len = encoded_integer_len(maximum_index, 7);
    Ok(literal_name_len
        .max(indexed_name_len)
        .max(indexed_field_len))
}

fn encoded_string_len(bytes: &[u8]) -> usize {
    let encoded_len = huffman_encoded_len(bytes);
    if encoded_len < bytes.len() {
        encoded_integer_len(encoded_len, 7) + encoded_len
    } else {
        encoded_integer_len(bytes.len(), 7) + bytes.len()
    }
}

fn encoded_literal_len(
    name_index: Option<usize>,
    name: &[u8],
    value: &[u8],
    kind: LiteralKind,
) -> usize {
    let header = encoded_integer_len(name_index.unwrap_or(0), kind.prefix_bits());
    let name_len = if name_index.is_none() {
        encoded_string_len(name)
    } else {
        0
    };
    header + name_len + encoded_string_len(value)
}

#[cfg(test)]
fn maximum_literal_len(name: &[u8], value: &[u8]) -> Result<usize, Error> {
    1usize
        .checked_add(encoded_integer_len(name.len(), 7))
        .and_then(|length| length.checked_add(name.len()))
        .and_then(|length| length.checked_add(encoded_integer_len(value.len(), 7)))
        .and_then(|length| length.checked_add(value.len()))
        .ok_or(Error::AllocationFailed)
}

fn push_literal(
    output: &mut impl EncodeOutput,
    name_index: Option<usize>,
    name: &[u8],
    value: &[u8],
    kind: LiteralKind,
    delta: &mut Diagnostics,
) {
    push_integer(
        output,
        name_index.unwrap_or(0),
        kind.prefix_bits(),
        kind.marker(),
    );
    if name_index.is_none() {
        push_string(output, name, delta);
    }
    push_string(output, value, delta);
}

fn push_integer(output: &mut impl EncodeOutput, mut value: usize, prefix_bits: u8, marker: u8) {
    let prefix_max = (1usize << prefix_bits) - 1;
    if value < prefix_max {
        output.push(marker | value as u8);
        return;
    }
    output.push(marker | prefix_max as u8);
    value -= prefix_max;
    while value >= 128 {
        output.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}

fn decode_integer(block: &[u8], cursor: &mut usize, prefix_bits: u8) -> Result<usize, Error> {
    let value = decode_integer_with_width(block, cursor, prefix_bits, usize::BITS)?;
    usize::try_from(value).map_err(|_| Error::IntegerOverflow)
}

fn decode_integer_with_width(
    block: &[u8],
    cursor: &mut usize,
    prefix_bits: u8,
    width: u32,
) -> Result<u128, Error> {
    let first = *block.get(*cursor).ok_or(Error::TruncatedInteger)?;
    *cursor += 1;
    let prefix_max = (1u128 << prefix_bits) - 1;
    let maximum = (1u128 << width) - 1;
    let mut value = u128::from(first) & prefix_max;
    if value < prefix_max {
        return Ok(value);
    }

    let mut shift = 0u32;
    let mut continuations = 0u32;
    let maximum_continuations = width.div_ceil(7);
    loop {
        let byte = *block.get(*cursor).ok_or(Error::TruncatedInteger)?;
        *cursor += 1;
        let payload = u128::from(byte & 0x7f);
        if shift >= width && payload != 0 {
            return Err(Error::IntegerOverflow);
        }
        if shift < width && payload > (maximum >> shift) {
            return Err(Error::IntegerOverflow);
        }
        let contribution = if shift >= width {
            0
        } else {
            payload.checked_shl(shift).ok_or(Error::IntegerOverflow)?
        };
        value = value
            .checked_add(contribution)
            .filter(|value| *value <= maximum)
            .ok_or(Error::IntegerOverflow)?;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        continuations += 1;
        if continuations > maximum_continuations {
            return Err(Error::IntegerOverflow);
        }
        shift = shift.checked_add(7).ok_or(Error::IntegerOverflow)?;
    }
}

#[cfg(feature = "hpack-test-support")]
pub(crate) fn test_decode_integer(
    block: &[u8],
    prefix_bits: u8,
    width: u32,
) -> (Result<u128, Error>, usize) {
    let mut cursor = 0;
    (
        decode_integer_with_width(block, &mut cursor, prefix_bits, width),
        cursor,
    )
}

#[cfg(feature = "hpack-test-support")]
pub(crate) fn test_account_lengths(
    name_len: usize,
    value_len: usize,
    limit: usize,
    initial_total: usize,
) -> (usize, bool) {
    let mut total = initial_total;
    let mut oversized = false;
    account_lengths(name_len, value_len, limit, &mut total, &mut oversized);
    (total, oversized)
}

#[cfg(feature = "hpack-test-support")]
pub(crate) fn test_encode_huffman(input: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(huffman_encoded_len(input));
    encode_huffman(input, &mut output);
    output
}

fn push_string(output: &mut impl EncodeOutput, bytes: &[u8], delta: &mut Diagnostics) {
    let encoded_len = huffman_encoded_len(bytes);
    if encoded_len < bytes.len() {
        push_integer(output, encoded_len, 7, 0x80);
        encode_huffman(bytes, output);
        delta.huffman_strings += 1;
    } else {
        push_integer(output, bytes.len(), 7, 0);
        output.extend_from_slice(bytes);
        delta.plain_strings += 1;
    }
}

fn decode_string_into(block: &[u8], cursor: &mut usize, output: &mut Vec<u8>) -> Result<(), Error> {
    let huffman = block.get(*cursor).ok_or(Error::TruncatedString)? & 0x80 != 0;
    let length = decode_integer(block, cursor, 7).map_err(|error| match error {
        Error::TruncatedInteger | Error::IntegerOverflow => Error::TruncatedString,
        other => other,
    })?;
    let end = cursor.checked_add(length).ok_or(Error::IntegerOverflow)?;
    let bytes = block.get(*cursor..end).ok_or(Error::TruncatedString)?;
    *cursor = end;
    if huffman {
        walk_huffman(bytes, |symbol| output.push(symbol)).map(|_| ())
    } else {
        output.extend_from_slice(bytes);
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum StringPart {
    Name,
    Value,
}

fn visit_string(
    block: &[u8],
    cursor: &mut usize,
    visitor: &mut impl HeaderFieldVisitor,
    part: StringPart,
) -> Result<(), Error> {
    let huffman = block.get(*cursor).ok_or(Error::TruncatedString)? & 0x80 != 0;
    let length = decode_integer(block, cursor, 7).map_err(|error| match error {
        Error::TruncatedInteger | Error::IntegerOverflow => Error::TruncatedString,
        other => other,
    })?;
    let end = cursor.checked_add(length).ok_or(Error::IntegerOverflow)?;
    let bytes = block.get(*cursor..end).ok_or(Error::TruncatedString)?;
    *cursor = end;
    if huffman {
        match part {
            StringPart::Name => walk_huffman(bytes, |byte| visitor.name_byte(byte)).map(|_| ()),
            StringPart::Value => walk_huffman(bytes, |byte| visitor.value_byte(byte)).map(|_| ()),
        }
    } else {
        match part {
            StringPart::Name => visitor.name_bytes(bytes),
            StringPart::Value => visitor.value_bytes(bytes),
        }
        Ok(())
    }
}

fn preview_string(
    block: &[u8],
    cursor: &mut usize,
    huffman_strings: &mut u64,
    plain_strings: &mut u64,
) -> Result<usize, Error> {
    let huffman = block.get(*cursor).ok_or(Error::TruncatedString)? & 0x80 != 0;
    let length = decode_integer(block, cursor, 7).map_err(|error| match error {
        Error::TruncatedInteger | Error::IntegerOverflow => Error::TruncatedString,
        other => other,
    })?;
    let end = cursor.checked_add(length).ok_or(Error::IntegerOverflow)?;
    let bytes = block.get(*cursor..end).ok_or(Error::TruncatedString)?;
    *cursor = end;
    if huffman {
        *huffman_strings = huffman_strings.saturating_add(1);
        validate_huffman(bytes)
    } else {
        *plain_strings = plain_strings.saturating_add(1);
        Ok(bytes.len())
    }
}

fn huffman_encoded_len(bytes: &[u8]) -> usize {
    let bits: usize = bytes
        .iter()
        .map(|byte| usize::from(HUFFMAN_CODES[usize::from(*byte)].1))
        .fold(0, usize::saturating_add);
    bits.div_ceil(8)
}

fn encode_huffman(bytes: &[u8], output: &mut impl EncodeOutput) {
    let mut accumulator = 0u64;
    let mut bit_count = 0u8;
    for byte in bytes {
        let (code, length) = HUFFMAN_CODES[usize::from(*byte)];
        accumulator = (accumulator << length) | u64::from(code);
        bit_count += length;
        while bit_count >= 8 {
            bit_count -= 8;
            output.push((accumulator >> bit_count) as u8);
            accumulator &= (1u64 << bit_count).wrapping_sub(1);
        }
    }
    if bit_count != 0 {
        let padding = 8 - bit_count;
        output.push(((accumulator << padding) | ((1u64 << padding) - 1)) as u8);
    }
}

#[derive(Clone, Copy)]
struct HuffmanNode {
    children: [Option<usize>; 2],
    symbol: Option<u16>,
}

const EMPTY_HUFFMAN_NODE: HuffmanNode = HuffmanNode {
    children: [None, None],
    symbol: None,
};
const HUFFMAN_TREE_NODE_COUNT: usize = HUFFMAN_CODES.len() * 2 - 1;
const HUFFMAN_NIBBLE_COUNT: usize = 16;
const HUFFMAN_NIBBLE_BITS: u8 = 4;
const HUFFMAN_EOS_SYMBOL: u16 = 256;
const MAX_HUFFMAN_PADDING_BITS: usize = 7;
const NO_HUFFMAN_SYMBOL: u16 = u16::MAX;
const INVALID_HUFFMAN_NODE: u16 = u16::MAX;
const _: () = assert!(HUFFMAN_TREE_NODE_COUNT < INVALID_HUFFMAN_NODE as usize);
const _: () = assert!(HUFFMAN_CODES.len() <= NO_HUFFMAN_SYMBOL as usize);

#[derive(Clone, Copy)]
struct HuffmanTree {
    nodes: [HuffmanNode; HUFFMAN_TREE_NODE_COUNT],
    len: usize,
}

const fn build_huffman_tree() -> HuffmanTree {
    let mut tree = HuffmanTree {
        nodes: [EMPTY_HUFFMAN_NODE; HUFFMAN_TREE_NODE_COUNT],
        len: 1,
    };
    let mut symbol = 0;
    while symbol < HUFFMAN_CODES.len() {
        let (code, length) = HUFFMAN_CODES[symbol];
        assert!(length > HUFFMAN_NIBBLE_BITS);
        let mut node = 0;
        let mut shift = length;
        while shift > 0 {
            shift -= 1;
            let bit = ((code >> shift) & 1) as usize;
            node = match tree.nodes[node].children[bit] {
                Some(child) => child,
                None => {
                    assert!(tree.len < HUFFMAN_TREE_NODE_COUNT);
                    let child = tree.len;
                    tree.len += 1;
                    tree.nodes[node].children[bit] = Some(child);
                    child
                }
            };
        }
        tree.nodes[node].symbol = Some(symbol as u16);
        symbol += 1;
    }
    tree
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HuffmanTransition {
    next: u16,
    symbol: u16,
}

const EMPTY_HUFFMAN_TRANSITION: HuffmanTransition = HuffmanTransition {
    next: INVALID_HUFFMAN_NODE,
    symbol: NO_HUFFMAN_SYMBOL,
};

struct HuffmanDecodeTable {
    transitions: [[HuffmanTransition; HUFFMAN_NIBBLE_COUNT]; HUFFMAN_TREE_NODE_COUNT],
    valid_padding_state: [bool; HUFFMAN_TREE_NODE_COUNT],
}

const fn build_huffman_decode_table(tree: &HuffmanTree) -> HuffmanDecodeTable {
    let mut table = HuffmanDecodeTable {
        transitions: [[EMPTY_HUFFMAN_TRANSITION; HUFFMAN_NIBBLE_COUNT]; HUFFMAN_TREE_NODE_COUNT],
        valid_padding_state: [false; HUFFMAN_TREE_NODE_COUNT],
    };
    let mut state = 0;
    while state < tree.len {
        let mut input = 0;
        while input < HUFFMAN_NIBBLE_COUNT {
            let mut node = state;
            let mut symbol = NO_HUFFMAN_SYMBOL;
            let mut valid = true;
            let mut shift = HUFFMAN_NIBBLE_BITS;
            while shift > 0 {
                shift -= 1;
                let bit = (input >> shift) & 1;
                node = match tree.nodes[node].children[bit] {
                    Some(child) => child,
                    None => {
                        valid = false;
                        0
                    }
                };
                if !valid {
                    break;
                }
                if let Some(decoded) = tree.nodes[node].symbol {
                    if decoded == HUFFMAN_EOS_SYMBOL || symbol != NO_HUFFMAN_SYMBOL {
                        valid = false;
                        break;
                    }
                    symbol = decoded;
                    node = 0;
                }
            }
            if valid {
                table.transitions[state][input] = HuffmanTransition {
                    next: node as u16,
                    symbol,
                };
            }
            input += 1;
        }
        state += 1;
    }

    let mut eos_prefix = 0;
    let mut padding_bits = 0;
    while padding_bits < MAX_HUFFMAN_PADDING_BITS {
        eos_prefix = match tree.nodes[eos_prefix].children[1] {
            Some(child) => child,
            None => return table,
        };
        table.valid_padding_state[eos_prefix] = true;
        padding_bits += 1;
    }
    table
}

static BUILT_HUFFMAN_TREE: HuffmanTree = build_huffman_tree();
static HUFFMAN_DECODE_TABLE: HuffmanDecodeTable = build_huffman_decode_table(&BUILT_HUFFMAN_TREE);

#[cfg(test)]
fn huffman_tree() -> &'static [HuffmanNode] {
    &BUILT_HUFFMAN_TREE.nodes[..BUILT_HUFFMAN_TREE.len]
}

#[cfg(test)]
fn decode_huffman(bytes: &[u8], allocations: &mut AllocationGate) -> Result<Vec<u8>, Error> {
    let decoded_len = validate_huffman(bytes)?;
    let mut output = Vec::new();
    allocations.try_reserve_vec(&mut output, decoded_len, AllocationPurpose::DecodedOutput)?;
    walk_huffman(bytes, |symbol| output.push(symbol))?;
    Ok(output)
}

fn validate_huffman(bytes: &[u8]) -> Result<usize, Error> {
    walk_huffman(bytes, |_| {})
}

fn walk_huffman(bytes: &[u8], mut emit: impl FnMut(u8)) -> Result<usize, Error> {
    let mut decoded_len = 0usize;
    let mut node = 0;

    for byte in bytes {
        for input in [byte >> 4, byte & 0x0f] {
            let transition = HUFFMAN_DECODE_TABLE.transitions[node][usize::from(input)];
            if transition.next == INVALID_HUFFMAN_NODE {
                return Err(Error::InvalidHuffman);
            }
            node = usize::from(transition.next);
            if transition.symbol != NO_HUFFMAN_SYMBOL {
                emit(transition.symbol as u8);
                decoded_len = decoded_len.checked_add(1).ok_or(Error::FieldSizeOverflow)?;
            }
        }
    }

    if node == 0 || HUFFMAN_DECODE_TABLE.valid_padding_state[node] {
        Ok(decoded_len)
    } else {
        Err(Error::InvalidHuffman)
    }
}

#[cfg(test)]
fn walk_huffman_reference(bytes: &[u8], mut emit: impl FnMut(u8)) -> Result<usize, Error> {
    let tree = huffman_tree();
    let mut decoded_len = 0usize;
    let mut node = 0;
    let mut trailing_value = 0u8;
    let mut trailing_bits = 0u8;

    for byte in bytes {
        for shift in (0..8).rev() {
            let bit = usize::from((byte >> shift) & 1);
            trailing_value = (trailing_value << 1) | bit as u8;
            trailing_bits = trailing_bits.saturating_add(1);
            node = tree[node].children[bit].ok_or(Error::InvalidHuffman)?;
            if let Some(symbol) = tree[node].symbol {
                if symbol == 256 {
                    return Err(Error::InvalidHuffman);
                }
                emit(symbol as u8);
                decoded_len = decoded_len.checked_add(1).ok_or(Error::FieldSizeOverflow)?;
                node = 0;
                trailing_value = 0;
                trailing_bits = 0;
            }
        }
    }

    if trailing_bits == 0 || (trailing_bits <= 7 && trailing_value == (1u8 << trailing_bits) - 1) {
        Ok(decoded_len)
    } else {
        Err(Error::InvalidHuffman)
    }
}

#[cfg(test)]
mod tests;
