#![allow(dead_code)]

use std::collections::VecDeque;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReferenceEntry {
    pub name: Vec<u8>,
    pub value: Vec<u8>,
}

pub struct ReferenceTable {
    entries: VecDeque<ReferenceEntry>,
    size: usize,
    capacity: usize,
}

impl ReferenceTable {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            size: 0,
            capacity,
        }
    }

    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity;
        self.evict();
    }

    pub fn insert(&mut self, name: &[u8], value: &[u8]) {
        let size = name.len() + value.len() + 32;
        if size > self.capacity {
            self.entries.clear();
            self.size = 0;
            return;
        }
        self.entries.push_front(ReferenceEntry {
            name: name.to_vec(),
            value: value.to_vec(),
        });
        self.size += size;
        self.evict();
    }

    pub fn exact_index(&self, name: &[u8], value: &[u8]) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| entry.name == name && entry.value == value)
            .map(|offset| 62 + offset)
    }

    pub fn entries(&self) -> impl Iterator<Item = &ReferenceEntry> {
        self.entries.iter()
    }

    fn evict(&mut self) {
        while self.size > self.capacity {
            let entry = self.entries.pop_back().unwrap();
            self.size -= entry.name.len() + entry.value.len() + 32;
        }
    }
}
