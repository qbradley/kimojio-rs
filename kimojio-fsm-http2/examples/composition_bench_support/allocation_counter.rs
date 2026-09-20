use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::{Mutex, MutexGuard};

use super::composition_bench_support::allocation::Meter;

#[derive(Clone, Copy, Default)]
struct Totals {
    allocations: u64,
    zeroed_allocations: u64,
    reallocations: u64,
    deallocations: u64,
}

struct Window {
    before: Totals,
    live_start: usize,
    peak: usize,
}

struct State {
    totals: Totals,
    live: usize,
    window: Option<Window>,
    invalid: bool,
}

impl State {
    fn adjust_live(&mut self, removed: usize, added: usize) {
        if let Some(live) = self
            .live
            .checked_sub(removed)
            .and_then(|n| n.checked_add(added))
        {
            self.live = live;
            if let Some(window) = &mut self.window {
                window.peak = window.peak.max(live);
            }
        } else {
            self.invalid = true;
        }
    }
}

fn increment(value: &mut u64, invalid: &mut bool) {
    if let Some(next) = value.checked_add(1) {
        *value = next;
    } else {
        *invalid = true;
    }
}

pub struct CountingAllocator {
    state: Mutex<State>,
}

impl CountingAllocator {
    pub const fn new() -> Self {
        Self {
            state: Mutex::new(State {
                totals: Totals {
                    allocations: 0,
                    zeroed_allocations: 0,
                    reallocations: 0,
                    deallocations: 0,
                },
                live: 0,
                window: None,
                invalid: false,
            }),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }
}

// The lock also covers System calls. No allocation or panic occurs in bookkeeping.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut state = self.state();
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            let State {
                totals, invalid, ..
            } = &mut *state;
            increment(&mut totals.allocations, invalid);
            state.adjust_live(0, layout.size());
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let mut state = self.state();
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            let State {
                totals, invalid, ..
            } = &mut *state;
            increment(&mut totals.allocations, invalid);
            increment(&mut totals.zeroed_allocations, invalid);
            state.adjust_live(0, layout.size());
        }
        pointer
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let mut state = self.state();
        let next = unsafe { System.realloc(pointer, layout, size) };
        if !next.is_null() {
            let State {
                totals, invalid, ..
            } = &mut *state;
            increment(&mut totals.reallocations, invalid);
            state.adjust_live(layout.size(), size);
        }
        next
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        let mut state = self.state();
        unsafe { System.dealloc(pointer, layout) };
        let State {
            totals, invalid, ..
        } = &mut *state;
        increment(&mut totals.deallocations, invalid);
        state.adjust_live(layout.size(), 0);
    }
}

#[derive(Debug, serde::Serialize)]
pub struct Sample {
    pub allocations: u64,
    pub zeroed_allocations: u64,
    pub reallocations: u64,
    pub deallocations: u64,
    pub requested_live_start: usize,
    pub requested_live_end: usize,
    pub requested_peak_live: usize,
    pub requested_peak_growth: usize,
    pub requested_live_change: i128,
}

impl Meter for CountingAllocator {
    type Sample = Sample;

    fn begin(&self) {
        let active = {
            let mut state = self.state();
            let active = state.window.is_some();
            if !active {
                state.window = Some(Window {
                    before: state.totals,
                    live_start: state.live,
                    peak: state.live,
                });
            }
            active
        };
        assert!(!active, "allocation windows cannot nest");
    }

    fn end(&self) -> Sample {
        let (window, totals, live, invalid) = {
            let mut state = self.state();
            (state.window.take(), state.totals, state.live, state.invalid)
        };
        assert!(
            !invalid,
            "allocation ledger overflow or unmatched deallocation"
        );
        let window = window.expect("allocation window must be active");
        Sample {
            allocations: totals.allocations - window.before.allocations,
            zeroed_allocations: totals.zeroed_allocations - window.before.zeroed_allocations,
            reallocations: totals.reallocations - window.before.reallocations,
            deallocations: totals.deallocations - window.before.deallocations,
            requested_live_start: window.live_start,
            requested_live_end: live,
            requested_peak_live: window.peak,
            requested_peak_growth: window.peak - window.live_start,
            requested_live_change: live as i128 - window.live_start as i128,
        }
    }

    fn live_bytes(&self) -> usize {
        let (live, invalid) = {
            let state = self.state();
            (state.live, state.invalid)
        };
        assert!(!invalid, "invalid allocation ledger");
        live
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_alloc_zeroed_realloc_grow_shrink_and_free() {
        let allocator = CountingAllocator::new();
        allocator.begin();
        unsafe {
            let layout = Layout::from_size_align(32, 8).unwrap();
            let pointer = allocator.alloc_zeroed(layout);
            assert!(!pointer.is_null());
            assert_eq!(std::slice::from_raw_parts(pointer, 32), &[0; 32]);
            let pointer = allocator.realloc(pointer, layout, 80);
            assert!(!pointer.is_null());
            let pointer = allocator.realloc(pointer, Layout::from_size_align(80, 8).unwrap(), 16);
            assert!(!pointer.is_null());
            allocator.dealloc(pointer, Layout::from_size_align(16, 8).unwrap());
        }
        let sample = allocator.end();
        assert_eq!((sample.allocations, sample.zeroed_allocations), (1, 1));
        assert_eq!((sample.reallocations, sample.deallocations), (2, 1));
        assert_eq!(
            (sample.requested_live_start, sample.requested_live_end),
            (0, 0)
        );
        assert_eq!(
            (sample.requested_peak_live, sample.requested_peak_growth),
            (80, 80)
        );
    }

    #[test]
    fn pre_window_free_does_not_underflow_or_hide_allocations() {
        let allocator = CountingAllocator::new();
        unsafe {
            let layout = Layout::from_size_align(64, 8).unwrap();
            let old = allocator.alloc(layout);
            assert!(!old.is_null());
            allocator.begin();
            allocator.dealloc(old, layout);
            let small = Layout::from_size_align(16, 8).unwrap();
            let pointer = allocator.alloc(small);
            assert!(!pointer.is_null());
            allocator.dealloc(pointer, small);
        }
        let sample = allocator.end();
        assert_eq!((sample.allocations, sample.deallocations), (1, 2));
        assert_eq!(
            (sample.requested_live_start, sample.requested_live_end),
            (64, 0)
        );
        assert_eq!(
            (sample.requested_peak_live, sample.requested_peak_growth),
            (64, 0)
        );
        assert_eq!(sample.requested_live_change, -64);
    }

    #[test]
    fn surviving_allocations_and_reallocations_cross_windows() {
        let allocator = CountingAllocator::new();
        unsafe {
            allocator.begin();
            let layout = Layout::from_size_align(24, 8).unwrap();
            let pointer = allocator.alloc(layout);
            assert!(!pointer.is_null());
            let first = allocator.end();
            assert_eq!((first.allocations, first.requested_live_end), (1, 24));
            allocator.begin();
            let pointer = allocator.realloc(pointer, layout, 48);
            assert!(!pointer.is_null());
            allocator.dealloc(pointer, Layout::from_size_align(48, 8).unwrap());
            let second = allocator.end();
            assert_eq!(
                (
                    second.allocations,
                    second.reallocations,
                    second.deallocations
                ),
                (0, 1, 1)
            );
            assert_eq!(
                (second.requested_live_start, second.requested_live_end),
                (24, 0)
            );
            assert_eq!(second.requested_peak_live, 48);
        }
    }

    #[test]
    fn zero_events_do_not_mean_zero_live_storage() {
        let allocator = CountingAllocator::new();
        unsafe {
            let layout = Layout::from_size_align(24, 8).unwrap();
            let pointer = allocator.alloc(layout);
            assert!(!pointer.is_null());
            allocator.begin();
            let sample = allocator.end();
            assert_eq!(
                (
                    sample.allocations,
                    sample.reallocations,
                    sample.deallocations
                ),
                (0, 0, 0)
            );
            assert_eq!(
                (sample.requested_live_start, sample.requested_live_end),
                (24, 24)
            );
            assert_eq!(
                (sample.requested_peak_live, sample.requested_peak_growth),
                (24, 0)
            );
            allocator.dealloc(pointer, layout);
        }
        assert_eq!(allocator.live_bytes(), 0);
    }
}
