#[cfg(test)]
use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::hash::{BuildHasher, RandomState};

use crate::huffman_table::HUFFMAN_CODES;

const STATIC_TABLE_LEN: usize = 61;
const DEFAULT_TABLE_SIZE: usize = 4096;
pub(crate) const MAX_TABLE_SIZE: usize = 1_048_576;

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

pub(crate) trait HeaderFieldVisitor {
    fn start_field(&mut self, sensitive: bool);
    fn name_byte(&mut self, byte: u8);
    fn end_name(&mut self);
    fn value_byte(&mut self, byte: u8);
    fn end_field(&mut self);
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
    for &byte in name {
        visitor.name_byte(byte);
    }
    visitor.end_name();
    for &byte in value {
        visitor.value_byte(byte);
    }
    visitor.end_field();
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
struct AllocationGate {
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
    hash_builder: RandomState,
    newest_name: HashMap<u64, u64>,
    newest_exact: HashMap<u64, u64>,
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
            hash_builder: self.hash_builder.clone(),
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
            hash_builder: RandomState::new(),
            newest_name: HashMap::new(),
            newest_exact: HashMap::new(),
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
            self.newest_name = HashMap::new();
            self.newest_exact = HashMap::new();
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

    fn find_exact(&self, name: &[u8], value: &[u8]) -> Option<usize> {
        #[cfg(test)]
        self.lookup_work.set(
            self.lookup_work
                .get()
                .saturating_add(name.len())
                .saturating_add(value.len()),
        );
        let hash = self.hash_builder.hash_one((name, value));
        self.find_exact_with_hash(name, value, hash)
    }

    fn find_name(&self, name: &[u8]) -> Option<usize> {
        #[cfg(test)]
        self.lookup_work
            .set(self.lookup_work.get().saturating_add(name.len()));
        let hash = self.hash_builder.hash_one(name);
        self.find_name_with_hash(name, hash)
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
        allocations.try_reserve_map(&mut self.newest_name, additional)?;
        allocations.try_reserve_map(&mut self.newest_exact, additional)
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
        self.newest_name.clear();
        self.newest_exact.clear();
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
        self.newest_name = HashMap::new();
        self.newest_exact = HashMap::new();
    }

    fn evict_one(&mut self) {
        let evicted_id = self.next_id - self.entries.len() as u64;
        let Some(entry) = self.entries.pop_back() else {
            return;
        };
        let name_hash = self.hash_builder.hash_one(entry.name());
        if self.newest_name.get(&name_hash) == Some(&evicted_id) {
            self.newest_name.remove(&name_hash);
        }
        let exact_hash = self.hash_builder.hash_one((entry.name(), entry.value()));
        if self.newest_exact.get(&exact_hash) == Some(&evicted_id) {
            self.newest_exact.remove(&exact_hash);
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

pub(crate) struct Encoder {
    table: DynamicTable,
    pending_min_size: Option<usize>,
    pending_final_size: Option<usize>,
    diagnostics: Diagnostics,
    allocations: AllocationGate,
    staged_fields: Vec<StagedField>,
}

#[derive(Default)]
struct StagedField {
    entry: Option<Entry>,
    // Static indexes are permanent. Dynamic indexes are cached only before
    // any pending resize or staged insertion can shift them.
    exact_index: Option<usize>,
}

impl Clone for Encoder {
    fn clone(&self) -> Self {
        Self {
            table: self.table.clone(),
            pending_min_size: self.pending_min_size,
            pending_final_size: self.pending_final_size,
            diagnostics: self.diagnostics,
            allocations: self.allocations.clone(),
            staged_fields: Vec::new(),
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.table.clone_from(&source.table);
        self.pending_min_size = source.pending_min_size;
        self.pending_final_size = source.pending_final_size;
        self.diagnostics = source.diagnostics;
        self.allocations.clone_from(&source.allocations);
        self.staged_fields.clear();
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
            staged_fields: Vec::new(),
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
        self.staged_fields.clear();
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

    pub(crate) fn encode_by<'a>(
        &mut self,
        field_count: usize,
        field_at: impl Fn(usize) -> HeaderFieldRef<'a>,
    ) -> Result<Vec<u8>, Error> {
        let target_size = self.pending_final_size.unwrap_or(self.table.max_size);
        let mut maximum_output = 0usize;
        if let Some(minimum) = self.pending_min_size {
            maximum_output = maximum_output
                .checked_add(encoded_integer_len(minimum, 5))
                .ok_or(Error::AllocationFailed)?;
            if self.pending_final_size != Some(minimum) {
                maximum_output = maximum_output
                    .checked_add(encoded_integer_len(target_size, 5))
                    .ok_or(Error::AllocationFailed)?;
            }
        }

        let mut insertion_candidates = 0usize;
        for index in 0..field_count {
            let field = field_at(index);
            let field_size = field
                .name
                .len()
                .checked_add(field.value.len())
                .and_then(|value| value.checked_add(32))
                .ok_or(Error::FieldSizeOverflow)?;
            maximum_output = maximum_output
                .checked_add(maximum_field_output_len(
                    field.name,
                    field.value,
                    field.sensitive,
                    target_size,
                )?)
                .ok_or(Error::AllocationFailed)?;
            if !field.sensitive && field_size <= target_size {
                insertion_candidates = insertion_candidates
                    .checked_add(1)
                    .ok_or(Error::StateOverflow)?;
            }
        }

        let mut output = Vec::new();
        self.allocations
            .try_reserve_vec(&mut output, maximum_output, AllocationPurpose::Other)?;
        let mut staged_fields = std::mem::take(&mut self.staged_fields);
        staged_fields.clear();
        let preflight = (|| {
            self.allocations.try_reserve_vec(
                &mut staged_fields,
                field_count,
                AllocationPurpose::Other,
            )?;
            // Repeated field_at calls must return the same field. Once a
            // mutation is possible, later dynamic indexes must be recomputed.
            let mut table_unchanged = self.pending_min_size.is_none();
            for index in 0..field_count {
                let field = field_at(index);
                let field_size = field.name.len() + field.value.len() + 32;
                let exact_index = if field.sensitive {
                    None
                } else {
                    find_static_exact(field.name, field.value).or_else(|| {
                        table_unchanged
                            .then(|| self.table.find_exact(field.name, field.value))
                            .flatten()
                    })
                };
                let needs_insertion =
                    !field.sensitive && field_size <= target_size && exact_index.is_none();
                let entry = if needs_insertion {
                    Some(Entry::try_new(
                        field.name,
                        field.value,
                        &mut self.allocations,
                    )?)
                } else {
                    None
                };
                staged_fields.push(StagedField { entry, exact_index });
                if needs_insertion {
                    table_unchanged = false;
                }
            }
            self.table.try_reserve_retained_entries(
                insertion_candidates,
                target_size,
                &mut self.allocations,
            )?;
            self.table.ensure_id_space(insertion_candidates)
        })();
        if let Err(error) = preflight {
            staged_fields.clear();
            self.staged_fields = staged_fields;
            return Err(error);
        }

        let mut delta = Diagnostics {
            encoded_blocks: 1,
            ..Diagnostics::default()
        };

        if let Some(minimum) = self.pending_min_size.take() {
            push_integer(&mut output, minimum, 5, 0x20);
            delta.table_size_updates += 1;
            delta.table_evictions += self.table.set_max_size_retaining_storage(minimum) as u64;
        }
        if let Some(final_size) = self.pending_final_size.take()
            && final_size != self.table.max_size
        {
            push_integer(&mut output, final_size, 5, 0x20);
            delta.table_size_updates += 1;
            delta.table_evictions += self.table.set_max_size_retaining_storage(final_size) as u64;
        }

        for (index, staged_field) in staged_fields.iter_mut().enumerate() {
            let field = field_at(index);
            delta.field_bytes = delta.field_bytes.saturating_add(
                u64::try_from(field.name.len().saturating_add(field.value.len()))
                    .unwrap_or(u64::MAX),
            );
            if !field.sensitive
                && let Some(index) = staged_field.exact_index.or_else(|| {
                    find_static_exact(field.name, field.value)
                        .or_else(|| self.table.find_exact(field.name, field.value))
                })
            {
                push_integer(&mut output, index, 7, 0x80);
                delta.indexed_fields += 1;
                continue;
            }

            if field.sensitive {
                let name_index = self
                    .table
                    .find_exact(field.name, field.value)
                    .or_else(|| find_static_exact(field.name, field.value))
                    .or_else(|| self.table.find_name(field.name))
                    .or_else(|| find_static_name(field.name));
                push_literal(
                    &mut output,
                    name_index,
                    field.name,
                    field.value,
                    LiteralKind::NeverIndexed,
                    &mut delta,
                );
                delta.never_indexed_fields += 1;
                continue;
            }

            let name_index = self
                .table
                .find_name(field.name)
                .or_else(|| find_static_name(field.name));
            let field_size = field
                .name
                .len()
                .checked_add(field.value.len())
                .and_then(|value| value.checked_add(32))
                .ok_or(Error::FieldSizeOverflow)?;
            if field_size <= self.table.max_size {
                push_literal(
                    &mut output,
                    name_index,
                    field.name,
                    field.value,
                    LiteralKind::Incremental,
                    &mut delta,
                );
                let entry = staged_field
                    .entry
                    .take()
                    .expect("fitting ordinary fields are staged before mutation");
                let (inserted, evictions) = self.table.insert_prepared(entry);
                delta.table_insertions += u64::from(inserted);
                delta.table_evictions += evictions as u64;
                delta.incremental_fields += 1;
            } else {
                push_literal(
                    &mut output,
                    name_index,
                    field.name,
                    field.value,
                    LiteralKind::WithoutIndexing,
                    &mut delta,
                );
                delta.without_indexing_fields += 1;
            }
        }

        delta.wire_bytes = u64::try_from(output.len()).unwrap_or(u64::MAX);
        self.diagnostics.add_assign(delta);
        if self.table.entries.len() == 0 {
            self.table.release_empty_storage();
        }
        staged_fields.clear();
        self.staged_fields = staged_fields;
        Ok(output)
    }

    pub(crate) fn maximum_output_len_by<'a>(
        &self,
        field_count: usize,
        mut field_at: impl FnMut(usize) -> HeaderFieldRef<'a>,
    ) -> Result<usize, Error> {
        let target_size = self.pending_final_size.unwrap_or(self.table.max_size);
        let mut maximum_output = 0usize;
        if let Some(minimum) = self.pending_min_size {
            maximum_output = maximum_output
                .checked_add(encoded_integer_len(minimum, 5))
                .ok_or(Error::AllocationFailed)?;
            if self.pending_final_size != Some(minimum) {
                maximum_output = maximum_output
                    .checked_add(encoded_integer_len(target_size, 5))
                    .ok_or(Error::AllocationFailed)?;
            }
        }
        for index in 0..field_count {
            let field = field_at(index);
            maximum_output = maximum_output
                .checked_add(maximum_field_output_len(
                    field.name,
                    field.value,
                    field.sensitive,
                    target_size,
                )?)
                .ok_or(Error::AllocationFailed)?;
        }
        Ok(maximum_output)
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
}

#[cfg(feature = "hpack-test-support")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TestPostLimitAllocationSnapshot {
    pub(crate) discarded_output_allocations: usize,
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

    #[cfg(any(test, feature = "hpack-test-support"))]
    fn begin_allocation_observation(&mut self) {
        self.post_limit_baseline = None;
        self.last_post_limit_allocations = None;
    }

    #[cfg(any(test, feature = "hpack-test-support"))]
    fn finish_allocation_observation(&mut self) {
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
        #[cfg(any(test, feature = "hpack-test-support"))]
        self.begin_allocation_observation();
        if let Some(error) = &self.terminal_error {
            return Err(error.clone());
        }
        let result = self.decode_inner(block, max_header_list_size, visitor);
        #[cfg(any(test, feature = "hpack-test-support"))]
        self.finish_allocation_observation();
        match result {
            Ok((fields, mut delta, oversized, list_size)) => {
                delta.wire_bytes = u64::try_from(block.len()).unwrap_or(u64::MAX);
                if oversized {
                    delta.header_list_too_large = 1;
                }
                self.diagnostics.add_assign(delta);
                if oversized {
                    Err(Error::HeaderListTooLarge { actual: list_size })
                } else {
                    Ok(fields)
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

    fn decode_inner(
        &mut self,
        block: &[u8],
        max_header_list_size: usize,
        visitor: &mut impl HeaderFieldVisitor,
    ) -> Result<(Vec<HeaderField>, Diagnostics, bool, usize), Error> {
        let mut cursor = 0;
        let mut fields = Vec::new();
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
                let (name, value) = self.table.get(index).ok_or(Error::InvalidIndex)?;
                visit_field(visitor, name, value, false);
                delta.field_bytes = delta.field_bytes.saturating_add(
                    u64::try_from(name.len().saturating_add(value.len())).unwrap_or(u64::MAX),
                );
                if account_lengths(
                    name.len(),
                    value.len(),
                    max_header_list_size,
                    &mut list_size,
                    &mut oversized,
                ) {
                    fields = Vec::new();
                    #[cfg(any(test, feature = "hpack-test-support"))]
                    if self.post_limit_baseline.is_none() {
                        self.post_limit_baseline = Some(self.allocations.observations());
                    }
                }
                if !oversized {
                    self.allocations.try_reserve_vec(
                        &mut fields,
                        1,
                        AllocationPurpose::DecodedOutput,
                    )?;
                    fields.push(try_clone_field(name, value, false, &mut self.allocations)?);
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
                    fields = Vec::new();
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
                    if !oversized {
                        self.allocations.try_reserve_vec(
                            &mut fields,
                            1,
                            AllocationPurpose::DecodedOutput,
                        )?;
                    }
                    let (inserted, evictions, field) = if oversized {
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
                        let (inserted, evictions) = self.table.insert_prepared(entry);
                        (inserted, evictions, None)
                    } else {
                        let (name, value) = self.decode_literal(
                            block,
                            &mut cursor,
                            6,
                            preview.decoded_name_len,
                            preview.decoded_value_len,
                        )?;
                        visit_field(visitor, &name, &value, false);
                        let (inserted, evictions) =
                            self.table.insert(&name, &value, &mut self.allocations)?;
                        (
                            inserted,
                            evictions,
                            Some(HeaderField {
                                name,
                                value,
                                sensitive: false,
                            }),
                        )
                    };
                    delta.table_insertions += u64::from(inserted);
                    delta.table_evictions += evictions as u64;
                    if let Some(field) = field {
                        fields.push(field);
                    }
                } else if !oversized {
                    self.allocations.try_reserve_vec(
                        &mut fields,
                        1,
                        AllocationPurpose::DecodedOutput,
                    )?;
                    let (name, value) = self.decode_literal(
                        block,
                        &mut cursor,
                        6,
                        preview.decoded_name_len,
                        preview.decoded_value_len,
                    )?;
                    visit_field(visitor, &name, &value, false);
                    delta.table_evictions += self.table.clear_releasing_storage() as u64;
                    fields.push(HeaderField {
                        name,
                        value,
                        sensitive: false,
                    });
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
                    fields = Vec::new();
                    #[cfg(any(test, feature = "hpack-test-support"))]
                    if self.post_limit_baseline.is_none() {
                        self.post_limit_baseline = Some(self.allocations.observations());
                    }
                }
                if !oversized {
                    self.allocations.try_reserve_vec(
                        &mut fields,
                        1,
                        AllocationPurpose::DecodedOutput,
                    )?;
                    let (name, value) = self.decode_literal(
                        block,
                        &mut cursor,
                        4,
                        preview.decoded_name_len,
                        preview.decoded_value_len,
                    )?;
                    visit_field(visitor, &name, &value, never_indexed);
                    fields.push(HeaderField {
                        name,
                        value,
                        sensitive: never_indexed,
                    });
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
        Ok((fields, delta, oversized, list_size))
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
            visit_string(block, cursor, |byte| visitor.name_byte(byte))?;
        } else {
            let name = self
                .table
                .get(name_index)
                .map(|(name, _)| name)
                .ok_or(Error::InvalidIndex)?;
            for &byte in name {
                visitor.name_byte(byte);
            }
        }
        visitor.end_name();
        visit_string(block, cursor, |byte| visitor.value_byte(byte))?;
        visitor.end_field();
        Ok(())
    }

    fn decode_literal(
        &mut self,
        block: &[u8],
        cursor: &mut usize,
        prefix_bits: u8,
        name_len: usize,
        value_len: usize,
    ) -> Result<(Vec<u8>, Vec<u8>), Error> {
        let name_index = decode_integer(block, cursor, prefix_bits)?;
        let name = if name_index == 0 {
            decode_string(block, cursor, name_len, &mut self.allocations)?
        } else {
            let source = self
                .table
                .get(name_index)
                .map(|(name, _)| name)
                .ok_or(Error::InvalidIndex)?;
            try_clone_bytes(source, &mut self.allocations)?
        };
        let value = decode_string(block, cursor, value_len, &mut self.allocations)?;
        Ok((name, value))
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

fn try_clone_bytes(bytes: &[u8], allocations: &mut AllocationGate) -> Result<Vec<u8>, Error> {
    let mut owned = Vec::new();
    allocations.try_reserve_vec(&mut owned, bytes.len(), AllocationPurpose::DecodedOutput)?;
    owned.extend_from_slice(bytes);
    Ok(owned)
}

fn try_clone_field(
    name: &[u8],
    value: &[u8],
    sensitive: bool,
    allocations: &mut AllocationGate,
) -> Result<HeaderField, Error> {
    Ok(HeaderField {
        name: try_clone_bytes(name, allocations)?,
        value: try_clone_bytes(value, allocations)?,
        sensitive,
    })
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

fn maximum_string_len(value: &[u8]) -> Result<usize, Error> {
    encoded_integer_len(value.len(), 7)
        .checked_add(value.len())
        .ok_or(Error::AllocationFailed)
}

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
    output: &mut Vec<u8>,
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

fn push_integer(output: &mut Vec<u8>, mut value: usize, prefix_bits: u8, marker: u8) {
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

fn push_string(output: &mut Vec<u8>, bytes: &[u8], delta: &mut Diagnostics) {
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

fn decode_string(
    block: &[u8],
    cursor: &mut usize,
    validated_decoded_len: usize,
    allocations: &mut AllocationGate,
) -> Result<Vec<u8>, Error> {
    let huffman = block.get(*cursor).ok_or(Error::TruncatedString)? & 0x80 != 0;
    let length = decode_integer(block, cursor, 7).map_err(|error| match error {
        Error::TruncatedInteger | Error::IntegerOverflow => Error::TruncatedString,
        other => other,
    })?;
    let end = cursor.checked_add(length).ok_or(Error::IntegerOverflow)?;
    let bytes = block.get(*cursor..end).ok_or(Error::TruncatedString)?;
    *cursor = end;
    if huffman {
        let mut output = Vec::new();
        allocations.try_reserve_vec(
            &mut output,
            validated_decoded_len,
            AllocationPurpose::DecodedOutput,
        )?;
        let actual_len = walk_huffman(bytes, |symbol| output.push(symbol))?;
        debug_assert_eq!(actual_len, validated_decoded_len);
        Ok(output)
    } else {
        debug_assert_eq!(bytes.len(), validated_decoded_len);
        try_clone_bytes(bytes, allocations)
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

fn visit_string(block: &[u8], cursor: &mut usize, mut visit: impl FnMut(u8)) -> Result<(), Error> {
    let huffman = block.get(*cursor).ok_or(Error::TruncatedString)? & 0x80 != 0;
    let length = decode_integer(block, cursor, 7).map_err(|error| match error {
        Error::TruncatedInteger | Error::IntegerOverflow => Error::TruncatedString,
        other => other,
    })?;
    let end = cursor.checked_add(length).ok_or(Error::IntegerOverflow)?;
    let bytes = block.get(*cursor..end).ok_or(Error::TruncatedString)?;
    *cursor = end;
    if huffman {
        walk_huffman(bytes, visit).map(|_| ())
    } else {
        for &byte in bytes {
            visit(byte);
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

fn encode_huffman(bytes: &[u8], output: &mut Vec<u8>) {
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

const BUILT_HUFFMAN_TREE: HuffmanTree = build_huffman_tree();
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

fn find_static_exact(name: &[u8], value: &[u8]) -> Option<usize> {
    STATIC_TABLE
        .iter()
        .position(|candidate| candidate.0 == name && candidate.1 == value)
        .map(|index| index + 1)
}

fn find_static_name(name: &[u8]) -> Option<usize> {
    STATIC_TABLE
        .iter()
        .position(|candidate| candidate.0 == name)
        .map(|index| index + 1)
}

// RFC 7541 Appendix A.
const STATIC_TABLE: [(&[u8], &[u8]); STATIC_TABLE_LEN] = [
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
    use super::*;

    #[test]
    fn rfc_integer_examples() {
        let mut encoded = Vec::new();
        push_integer(&mut encoded, 10, 5, 0);
        assert_eq!(encoded, [10]);
        encoded.clear();
        push_integer(&mut encoded, 1337, 5, 0);
        assert_eq!(encoded, [31, 154, 10]);
        let mut cursor = 0;
        assert_eq!(decode_integer(&encoded, &mut cursor, 5), Ok(1337));
    }

    #[test]
    fn rfc_huffman_example() {
        let mut encoded = Vec::new();
        encode_huffman(b"www.example.com", &mut encoded);
        assert_eq!(
            encoded,
            [
                0xf1, 0xe3, 0xc2, 0xe5, 0xf2, 0x3a, 0x6b, 0xa0, 0xab, 0x90, 0xf4, 0xff
            ]
        );
        assert_eq!(
            decode_huffman(&encoded, &mut AllocationGate::default()),
            Ok(b"www.example.com".to_vec())
        );
    }

    #[test]
    fn nibble_huffman_decoder_matches_bitwise_reference() {
        fn decode_with(
            bytes: &[u8],
            walk: impl FnOnce(&[u8], &mut dyn FnMut(u8)) -> Result<usize, Error>,
        ) -> Result<(usize, Vec<u8>), Error> {
            let mut output = Vec::new();
            let decoded_len = walk(bytes, &mut |symbol| output.push(symbol))?;
            assert_eq!(decoded_len, output.len());
            Ok((decoded_len, output))
        }

        let optimized = |bytes: &[u8]| decode_with(bytes, |input, emit| walk_huffman(input, emit));
        let reference =
            |bytes: &[u8]| decode_with(bytes, |input, emit| walk_huffman_reference(input, emit));

        assert_eq!(optimized(&[]), reference(&[]));
        for first in 0..=u8::MAX {
            assert_eq!(optimized(&[first]), reference(&[first]));
        }
        for input in 0..=u16::MAX {
            let bytes = input.to_be_bytes();
            assert_eq!(
                optimized(&bytes),
                reference(&bytes),
                "mismatch for {bytes:02x?}"
            );
        }
    }

    #[test]
    fn nibble_huffman_table_matches_each_bitwise_transition() {
        let tree = huffman_tree();
        for state in 0..tree.len() {
            for input in 0..HUFFMAN_NIBBLE_COUNT {
                let mut node = state;
                let mut symbol = NO_HUFFMAN_SYMBOL;
                let mut valid = true;
                for shift in (0..HUFFMAN_NIBBLE_BITS).rev() {
                    let bit = (input >> shift) & 1;
                    let Some(child) = tree[node].children[bit] else {
                        valid = false;
                        break;
                    };
                    node = child;
                    if let Some(decoded) = tree[node].symbol {
                        if decoded == HUFFMAN_EOS_SYMBOL || symbol != NO_HUFFMAN_SYMBOL {
                            valid = false;
                            break;
                        }
                        symbol = decoded;
                        node = 0;
                    }
                }
                let expected = if valid {
                    HuffmanTransition {
                        next: node as u16,
                        symbol,
                    }
                } else {
                    EMPTY_HUFFMAN_TRANSITION
                };
                assert_eq!(
                    HUFFMAN_DECODE_TABLE.transitions[state][input], expected,
                    "mismatch for state {state}, nibble {input:#x}"
                );
            }

            let mut eos_prefix = 0;
            let mut valid_padding_state = false;
            for _ in 0..MAX_HUFFMAN_PADDING_BITS {
                eos_prefix = tree[eos_prefix].children[1].unwrap();
                valid_padding_state |= eos_prefix == state;
            }
            assert_eq!(
                HUFFMAN_DECODE_TABLE.valid_padding_state[state], valid_padding_state,
                "padding classification mismatch for state {state}"
            );
        }
    }

    #[test]
    fn empty_huffman_value_decodes_without_poisoning_or_table_mutation() {
        let mut decoder = Decoder::new();
        assert_eq!(
            decoder.decode(&[0x11, 0x80], usize::MAX),
            Ok(vec![HeaderField::sensitive(":authority", "")])
        );
        assert_eq!(decoder.table.entries.len(), 0);
        assert_eq!(
            decoder.decode(&[0x82], usize::MAX),
            Ok(vec![HeaderField::new(":method", "GET")])
        );
    }

    #[test]
    fn rfc_request_examples_with_huffman() {
        let mut decoder = Decoder::new();
        let first = [
            0x82, 0x86, 0x84, 0x41, 0x8c, 0xf1, 0xe3, 0xc2, 0xe5, 0xf2, 0x3a, 0x6b, 0xa0, 0xab,
            0x90, 0xf4, 0xff,
        ];
        let fields = decoder.decode(&first, usize::MAX).unwrap();
        assert_eq!(
            fields,
            [
                HeaderField::new(":method", "GET"),
                HeaderField::new(":scheme", "http"),
                HeaderField::new(":path", "/"),
                HeaderField::new(":authority", "www.example.com"),
            ]
        );

        let second = [
            0x82, 0x86, 0x84, 0xbe, 0x58, 0x86, 0xa8, 0xeb, 0x10, 0x64, 0x9c, 0xbf,
        ];
        let fields = decoder.decode(&second, usize::MAX).unwrap();
        assert_eq!(fields[3], HeaderField::new(":authority", "www.example.com"));
        assert_eq!(fields[4], HeaderField::new("cache-control", "no-cache"));
    }

    #[test]
    fn encoder_reuses_dynamic_entries_and_preserves_sensitivity() {
        let mut encoder = Encoder::new();
        let mut decoder = Decoder::new();
        let fields = [
            HeaderField::new("x-repeat", "repeat-value"),
            HeaderField::sensitive("authorization", "secret"),
        ];
        let first = encoder.encode(&fields).unwrap();
        let decoded = decoder.decode(&first, usize::MAX).unwrap();
        assert_eq!(decoded, fields);
        let second = encoder.encode(&fields).unwrap();
        assert!(second.len() < first.len());
        assert_eq!(decoder.decode(&second, usize::MAX).unwrap(), fields);
        assert_eq!(encoder.diagnostics().never_indexed_fields, 2);
    }

    #[test]
    fn encoder_revalidates_exact_indexes_after_mutation_and_pending_resize() {
        let mut encoder = Encoder::new();
        let mut decoder = Decoder::new();
        encoder.set_max_table_size(80);
        decoder.set_max_allowed_table_size(80);

        let a = HeaderField::new("x-a", "1");
        let b = HeaderField::new("x-b", "2");
        let c = HeaderField::new("x-c", "3");
        let setup = [a.clone(), c];
        let setup_block = encoder.encode(&setup).unwrap();
        assert_eq!(decoder.decode(&setup_block, usize::MAX), Ok(setup.to_vec()));

        // A's first index is safe to cache. Inserting B evicts A, so the
        // second A must be looked up again and emitted as a literal.
        let mixed = [a.clone(), b, a.clone()];
        let mixed_block = encoder.encode(&mixed).unwrap();
        assert_eq!(decoder.decode(&mixed_block, usize::MAX), Ok(mixed.to_vec()));

        // The pending minimum clears both histories before A is encoded.
        // Preflight must not retain the exact index from the old table.
        encoder.set_max_table_size(0);
        encoder.set_max_table_size(80);
        decoder.set_max_allowed_table_size(0);
        decoder.set_max_allowed_table_size(80);
        let resized_block = encoder.encode(std::slice::from_ref(&a)).unwrap();
        assert_eq!(
            decoder.decode(&resized_block, usize::MAX),
            Ok(vec![a.clone()])
        );
        assert_eq!(encoder.table.snapshot(), decoder.table.snapshot());
    }

    #[test]
    fn table_updates_must_lead_and_obey_limit() {
        let mut decoder = Decoder::new();
        decoder.set_max_allowed_table_size(64);
        assert_eq!(decoder.decode(&[0x3f, 0x21], usize::MAX), Ok(Vec::new()));
        assert_eq!(
            decoder.decode(&[0x82, 0x20], usize::MAX),
            Err(Error::TableSizeUpdateAfterField)
        );
        assert_eq!(
            decoder.decode(&[0x3f, 0x62], usize::MAX),
            Err(Error::DecoderPoisoned)
        );

        let mut decoder = Decoder::new();
        decoder.set_max_allowed_table_size(64);
        assert_eq!(
            decoder.decode(&[0x3f, 0x62], usize::MAX),
            Err(Error::InvalidMaxDynamicSize)
        );
    }

    #[test]
    fn oversized_incremental_field_reuses_dynamic_name_before_clearing() {
        let mut decoder = Decoder::new();
        decoder.set_max_allowed_table_size(64);
        assert_eq!(
            decoder
                .decode(&[0x3f, 0x21, 0x40, 0x01, b'x', 0x01, b'a'], usize::MAX)
                .unwrap(),
            [HeaderField::new("x", "a")]
        );

        let mut oversized = vec![0x7e, 40];
        oversized.extend(std::iter::repeat_n(b'v', 40));
        assert_eq!(
            decoder.decode(&oversized, usize::MAX).unwrap(),
            [HeaderField::new(b"x", vec![b'v'; 40])]
        );
        assert!(decoder.table.snapshot().is_empty());
    }

    #[test]
    fn oversized_lists_advance_compression_state() {
        let mut encoder = Encoder::new();
        let first = encoder
            .encode(&[HeaderField::new("x-dynamic", "a-value")])
            .unwrap();
        let second = encoder
            .encode(&[HeaderField::new("x-dynamic", "a-value")])
            .unwrap();
        let mut decoder = Decoder::new();
        assert_eq!(
            decoder.decode(&first, 1),
            Err(Error::HeaderListTooLarge { actual: 48 })
        );
        assert_eq!(
            decoder.decode(&second, usize::MAX).unwrap(),
            [HeaderField::new("x-dynamic", "a-value")]
        );
    }

    #[test]
    fn rejects_eos_and_invalid_padding() {
        assert_eq!(
            decode_huffman(&[0xff, 0xff, 0xff, 0xff], &mut AllocationGate::default()),
            Err(Error::InvalidHuffman)
        );
        assert_eq!(
            decode_huffman(&[0x00], &mut AllocationGate::default()),
            Err(Error::InvalidHuffman)
        );
    }

    #[test]
    fn huffman_round_trips_every_octet() {
        let input = (0..=u8::MAX).collect::<Vec<_>>();
        let mut encoded = Vec::new();
        encode_huffman(&input, &mut encoded);
        assert_eq!(
            decode_huffman(&encoded, &mut AllocationGate::default()),
            Ok(input)
        );
    }

    #[test]
    fn decodes_every_representation_and_preserves_never_indexed() {
        let block = [
            0x20, // table size update to zero
            0x82, // indexed :method GET
            0x40, 0x01, b'a', 0x01, b'b', // incremental
            0x00, 0x01, b'c', 0x01, b'd', // without indexing
            0x10, 0x01, b'e', 0x01, b'f', // never indexed
        ];
        let mut decoder = Decoder::new();
        let fields = decoder.decode(&block, usize::MAX).unwrap();
        assert_eq!(
            fields,
            [
                HeaderField::new(":method", "GET"),
                HeaderField::new("a", "b"),
                HeaderField::new("c", "d"),
                HeaderField::sensitive("e", "f"),
            ]
        );
        let diagnostics = decoder.diagnostics();
        assert_eq!(diagnostics.table_size_updates, 1);
        assert_eq!(diagnostics.indexed_fields, 1);
        assert_eq!(diagnostics.incremental_fields, 1);
        assert_eq!(diagnostics.without_indexing_fields, 1);
        assert_eq!(diagnostics.never_indexed_fields, 1);
    }

    #[test]
    fn huffman_is_selected_only_when_shorter() {
        let mut encoder = Encoder::new();
        let encoded = encoder
            .encode(&[
                HeaderField::sensitive("x", "www.example.com"),
                HeaderField::sensitive("x", "\0"),
            ])
            .unwrap();
        assert_eq!(encoder.diagnostics().huffman_strings, 1);
        assert_eq!(encoder.diagnostics().plain_strings, 3);
        let mut decoder = Decoder::new();
        assert_eq!(
            decoder.decode(&encoded, usize::MAX).unwrap(),
            [
                HeaderField::sensitive("x", "www.example.com"),
                HeaderField::sensitive("x", "\0"),
            ]
        );
    }

    #[test]
    fn accepts_non_minimal_and_rejects_overflowing_integers() {
        let mut cursor = 0;
        assert_eq!(decode_integer(&[0x1f, 0x80, 0x00], &mut cursor, 5), Ok(31));
        let mut cursor = 0;
        assert_eq!(
            decode_integer(
                &[
                    0x1f, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x02
                ],
                &mut cursor,
                5,
            ),
            Err(Error::IntegerOverflow)
        );
        let mut decoder = Decoder::new();
        assert_eq!(
            decoder.decode(
                &[
                    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f
                ],
                usize::MAX,
            ),
            Err(Error::IntegerOverflow)
        );
    }

    #[test]
    fn accepts_non_minimal_integer_forms_for_every_prefix_width() {
        for prefix_bits in [4, 5, 6, 7] {
            let prefix_max = (1usize << prefix_bits) - 1;
            for offset in 0..12 {
                let expected = prefix_max + offset * 127;
                let mut encoded = Vec::new();
                push_integer(&mut encoded, expected, prefix_bits, 0);
                let final_octet = encoded.last_mut().unwrap();
                *final_octet |= 0x80;
                encoded.push(0);
                let mut cursor = 0;
                assert_eq!(
                    decode_integer(&encoded, &mut cursor, prefix_bits),
                    Ok(expected)
                );
                assert_eq!(cursor, encoded.len());
            }
        }
    }

    #[test]
    fn integer_zero_continuations_cover_platform_maximum_and_truncations() {
        let maximum_groups = usize::BITS.div_ceil(7) as usize;
        for groups in 1..=maximum_groups {
            let mut encoded = vec![0x1f];
            encoded.extend(std::iter::repeat_n(0x80, groups));
            encoded.push(0);
            let mut cursor = 0;
            assert_eq!(decode_integer(&encoded, &mut cursor, 5), Ok(31));
            assert_eq!(cursor, encoded.len());

            encoded.pop();
            let mut cursor = 0;
            assert_eq!(
                decode_integer(&encoded, &mut cursor, 5),
                Err(Error::TruncatedInteger)
            );
        }

        let mut overflowing = vec![0x1f];
        overflowing.extend(std::iter::repeat_n(0x80, maximum_groups + 1));
        let mut cursor = 0;
        assert_eq!(
            decode_integer(&overflowing, &mut cursor, 5),
            Err(Error::IntegerOverflow)
        );
    }

    #[test]
    fn decoded_limit_accounting_overflow_is_a_local_limit_outcome() {
        let mut total = usize::MAX - 1;
        let mut oversized = false;
        assert!(account_lengths(
            1,
            0,
            usize::MAX,
            &mut total,
            &mut oversized
        ));
        assert!(oversized);
        assert_eq!(total, usize::MAX);
    }

    #[test]
    fn reduced_decoder_limit_requires_a_leading_minimum_update() {
        let mut decoder = Decoder::new();
        decoder.set_max_allowed_table_size(0);
        assert_eq!(
            decoder.decode(&[], usize::MAX),
            Err(Error::InvalidTableSizeUpdate)
        );
        assert_eq!(
            decoder.decode(&[0x20], usize::MAX),
            Err(Error::DecoderPoisoned)
        );

        let mut decoder = Decoder::new();
        decoder.set_max_allowed_table_size(0);
        assert_eq!(decoder.decode(&[0x20], usize::MAX), Ok(Vec::new()));

        let mut decoder = Decoder::new();
        decoder.set_max_allowed_table_size(0);
        decoder.set_max_allowed_table_size(128);
        assert_eq!(
            decoder.decode(&[0x3f, 0x61], usize::MAX),
            Err(Error::InvalidTableSizeUpdate)
        );
        let mut decoder = Decoder::new();
        decoder.set_max_allowed_table_size(0);
        decoder.set_max_allowed_table_size(128);
        assert_eq!(
            decoder.decode(&[0x20, 0x3f, 0x61], usize::MAX),
            Ok(Vec::new())
        );
    }

    #[test]
    fn table_eviction_removes_lookup_indexes() {
        let mut encoder = Encoder::new();
        encoder.set_max_table_size(64);
        let first = encoder
            .encode(&[HeaderField::new("first", "first-value")])
            .unwrap();
        let second = encoder
            .encode(&[HeaderField::new("second", "second-value")])
            .unwrap();
        let third = encoder
            .encode(&[HeaderField::new("first", "first-value")])
            .unwrap();
        assert!(second.len() > 1);
        assert!(third.len() > 1);

        let mut decoder = Decoder::new();
        decoder.set_max_allowed_table_size(64);
        assert_eq!(
            decoder.decode(&first, usize::MAX).unwrap(),
            [HeaderField::new("first", "first-value")]
        );
        assert_eq!(
            decoder.decode(&second, usize::MAX).unwrap(),
            [HeaderField::new("second", "second-value")]
        );
        assert_eq!(
            decoder.decode(&third, usize::MAX).unwrap(),
            [HeaderField::new("first", "first-value")]
        );
        assert_eq!(
            encoder.table.snapshot(),
            [(b"first".to_vec(), b"first-value".to_vec())]
        );
        assert_eq!(
            decoder.table.snapshot(),
            [(b"first".to_vec(), b"first-value".to_vec())]
        );
    }

    #[test]
    fn empty_blocks_have_exact_zero_raw_effects() {
        let mut encoder = Encoder::new();
        let mut decoder = Decoder::new();
        let block = encoder.encode(&[]).unwrap();
        assert!(block.is_empty());
        assert_eq!(decoder.decode(&block, usize::MAX), Ok(Vec::new()));
        assert_eq!(encoder.table.snapshot(), []);
        assert_eq!(decoder.table.snapshot(), []);
        assert_eq!(encoder.diagnostics().encoded_blocks, 1);
        assert_eq!(decoder.diagnostics().decoded_blocks, 1);
        assert_eq!(encoder.diagnostics().field_bytes, 0);
        assert_eq!(decoder.diagnostics().field_bytes, 0);
    }

    #[test]
    fn encoder_persistent_preflight_is_bounded_by_effective_capacity() {
        let mut encoder = Encoder::new();
        encoder.set_max_table_size(64);
        let field = HeaderField::new("x", "value");
        encoder.encode(std::slice::from_ref(&field)).unwrap();
        let repeated = vec![field; 4096];
        encoder.encode(&repeated).unwrap();

        assert!(encoder.table.entries.capacity() <= 4);
        assert!(encoder.table.newest_name.capacity() <= 7);
        assert!(encoder.table.newest_exact.capacity() <= 7);
    }

    #[test]
    fn output_preflight_covers_worst_case_dynamic_name_index() {
        assert_eq!(maximum_literal_len(b"", b"").unwrap(), 3);
        assert_eq!(
            maximum_field_output_len(b"", b"", false, MAX_TABLE_SIZE).unwrap(),
            5
        );
    }

    #[test]
    fn crossed_limit_incremental_literal_allocates_only_retained_state() {
        let block = [0x40, 0x01, b'x', 0x01, b'v'];
        let mut decoder = Decoder::new();
        decoder.set_allocation_failure_after(Some(4));

        assert_eq!(
            decoder.decode(&block, 0),
            Err(Error::HeaderListTooLarge { actual: 34 })
        );
        assert_eq!(decoder.table.snapshot(), [(b"x".to_vec(), b"v".to_vec())]);
    }

    #[test]
    fn entry_metadata_fits_hpack_per_entry_overhead() {
        assert!(std::mem::size_of::<Entry>() <= 32);
        assert!(std::mem::size_of::<Option<Entry>>() <= 32);
    }

    #[test]
    fn chunked_table_preserves_order_across_chunk_boundaries() {
        let mut table = DynamicTable::new(65_536);
        let mut allocations = AllocationGate::default();
        for index in 0..70 {
            table
                .insert(
                    format!("x-{index:02}").as_bytes(),
                    b"value",
                    &mut allocations,
                )
                .unwrap();
        }
        assert_eq!(table.entries.len(), 70);
        assert_eq!(
            table.get(62).unwrap(),
            (b"x-69".as_slice(), b"value".as_slice())
        );
        assert_eq!(
            table.get(131).unwrap(),
            (b"x-00".as_slice(), b"value".as_slice())
        );
        assert_eq!(table.find_exact(b"x-69", b"value"), Some(62));
        assert_eq!(table.set_max_size_releasing_storage(0), 70);
        assert_eq!(table.entries.len(), 0);
        assert_eq!(table.entries.capacity(), 0);
    }

    #[test]
    fn lookup_is_complete_and_work_is_bounded_by_input_size() {
        let mut table = DynamicTable::new(usize::MAX);
        let mut allocations = AllocationGate::default();
        for index in 0..2000_u16 {
            table
                .insert(b"x", &index.to_be_bytes(), &mut allocations)
                .unwrap();
        }
        assert_eq!(table.find_exact(b"x", &0_u16.to_be_bytes()), Some(2061));
        assert_eq!(table.find_exact(b"x", b"missing"), None);
        let work = table.take_lookup_work();
        assert!(work <= 2 * (b"x".len() + b"missing".len() + 2));
        assert_eq!(table.set_max_size_releasing_storage(0), 2000);
    }

    #[test]
    fn lookup_collision_chains_are_complete_and_equality_safe() {
        let mut table = DynamicTable::new(4096);
        let mut allocations = AllocationGate::default();
        table.try_reserve_insertions(3, &mut allocations).unwrap();
        for (name, value) in [
            (b"a".as_slice(), b"1".as_slice()),
            (b"b", b"2"),
            (b"a", b"3"),
        ] {
            let entry = Entry::try_new(name, value, &mut allocations).unwrap();
            table.insert_prepared_with_hashes(entry, 7, 11);
        }

        assert_eq!(table.find_exact_with_hash(b"a", b"3", 11), Some(62));
        assert_eq!(table.find_exact_with_hash(b"b", b"2", 11), Some(63));
        assert_eq!(table.find_exact_with_hash(b"a", b"1", 11), Some(64));
        assert_eq!(table.find_exact_with_hash(b"a", b"2", 11), None);
        assert_eq!(table.find_name_with_hash(b"a", 7), Some(62));
        assert_eq!(table.find_name_with_hash(b"b", 7), Some(63));
        assert_eq!(table.find_name_with_hash(b"c", 7), None);
    }

    #[test]
    fn lookup_work_does_not_scale_with_retained_entries_at_required_capacities() {
        for capacity in [0, 4096, 65_535, 65_536, 65_537, MAX_TABLE_SIZE] {
            let mut table = DynamicTable::new(capacity);
            let mut allocations = AllocationGate::default();
            for _ in 0..capacity / 32 {
                table.insert(b"", b"", &mut allocations).unwrap();
            }
            assert_eq!(table.entries.len(), capacity / 32);
            if capacity != 0 {
                assert_eq!(table.find_exact(b"", b""), Some(62));
                assert_eq!(table.find_name(b""), Some(62));
            }
            assert_eq!(table.find_exact(b"not-retained", b"value"), None);
            let work = table.take_lookup_work();
            assert!(
                work <= b"not-retained".len() + b"value".len(),
                "lookup work {work} scaled at capacity {capacity}"
            );
        }
    }

    #[test]
    fn clear_and_zero_capacity_release_history_storage() {
        let mut table = DynamicTable::new(65_536);
        let mut allocations = AllocationGate::default();
        for index in 0..512_u16 {
            table
                .insert(b"x", &index.to_be_bytes(), &mut allocations)
                .unwrap();
        }
        assert!(table.entries.capacity() >= table.entries.len());
        assert!(table.newest_exact.capacity() > 0);
        let retained = table.entries.len();
        assert_eq!(table.set_max_size_releasing_storage(0), retained);
        assert_eq!(table.entries.capacity(), 0);
        assert_eq!(table.newest_name.capacity(), 0);
        assert_eq!(table.newest_exact.capacity(), 0);

        table.set_max_size_releasing_storage(4096);
        for index in 0..32_u8 {
            table.insert(&[index], b"v", &mut allocations).unwrap();
        }
        assert!(table.entries.capacity() > 0);
        assert!(
            !table
                .insert(&vec![b'x'; 4097], b"", &mut allocations)
                .unwrap()
                .0
        );
        assert_eq!(table.entries.capacity(), 0);
        assert_eq!(table.newest_name.capacity(), 0);
        assert_eq!(table.newest_exact.capacity(), 0);
    }

    #[test]
    fn sensitive_name_selection_uses_exact_matches_in_required_order() {
        let mut encoder = Encoder::new();
        let mut allocations = AllocationGate::default();
        encoder
            .table
            .try_reserve_insertions(2, &mut allocations)
            .unwrap();
        encoder.table.ensure_id_space(2).unwrap();
        let dynamic_name = Entry::try_new(b":method", b"PATCH", &mut allocations).unwrap();
        encoder.table.insert_prepared(dynamic_name);
        let dynamic_exact = Entry::try_new(b":method", b"GET", &mut allocations).unwrap();
        encoder.table.insert_prepared(dynamic_exact);

        let block = encoder
            .encode(&[HeaderField::sensitive(":method", "GET")])
            .unwrap();
        assert_eq!(&block[..2], &[0x1f, 0x2f]);

        let mut static_before_name = Encoder::new();
        static_before_name
            .table
            .try_reserve_insertions(1, &mut allocations)
            .unwrap();
        static_before_name.table.ensure_id_space(1).unwrap();
        static_before_name
            .table
            .insert_prepared(Entry::try_new(b":method", b"PATCH", &mut allocations).unwrap());
        let block = static_before_name
            .encode(&[HeaderField::sensitive(":method", "GET")])
            .unwrap();
        assert_eq!(block[0], 0x12);
    }

    #[test]
    fn encoder_allocation_failures_leave_logical_state_unchanged() {
        for successful_allocations in 0..5 {
            let mut encoder = Encoder::new();
            encoder.set_max_table_size(0);
            encoder.set_max_table_size(128);
            let table_before = encoder.table.snapshot();
            let diagnostics_before = encoder.diagnostics();
            let minimum_before = encoder.pending_min_size;
            let final_before = encoder.pending_final_size;
            encoder.set_allocation_failure_after(Some(successful_allocations));

            assert_eq!(
                encoder.encode(&[HeaderField::new("x-next", "value")]),
                Err(Error::AllocationFailed)
            );
            assert_eq!(encoder.table.snapshot(), table_before);
            assert_eq!(encoder.diagnostics(), diagnostics_before);
            assert_eq!(encoder.pending_min_size, minimum_before);
            assert_eq!(encoder.pending_final_size, final_before);
        }

        for successful_allocations in 0..2 {
            let mut encoder = Encoder::new();
            encoder
                .encode(&[HeaderField::new("x-prime", "value")])
                .unwrap();
            let table_before = encoder.table.snapshot();
            let diagnostics_before = encoder.diagnostics();
            encoder.set_allocation_failure_after(Some(successful_allocations));
            assert_eq!(
                encoder.encode(&[HeaderField::new("x-next", "value")]),
                Err(Error::AllocationFailed)
            );
            assert_eq!(encoder.table.snapshot(), table_before);
            assert_eq!(encoder.diagnostics(), diagnostics_before);
        }
    }
}
