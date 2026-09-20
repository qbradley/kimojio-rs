use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    ptr,
    sync::Mutex,
};

use crate::support::{Meter, Tag};
use serde_json::{Value, json};
#[path = "retention.rs"]
mod retention;

thread_local! {
    static CONTEXT: Cell<usize> = const { Cell::new(0) };
}

struct Restore(usize);
impl Drop for Restore {
    fn drop(&mut self) {
        CONTEXT.with(|context| context.set(self.0));
    }
}

fn in_context<T>(tag: usize, action: impl FnOnce() -> T) -> T {
    let previous = CONTEXT.with(|context| context.replace(tag));
    let _restore = Restore(previous);
    action()
}

#[derive(Clone, Copy, Default, serde::Serialize)]
pub struct Counts {
    pub allocations: u64,
    pub reallocations: u64,
    pub deallocations: u64,
    pub live: usize,
    pub peak: usize,
}

struct State {
    counts: [Counts; 3],
    before: Option<[Counts; 3]>,
    invalid: bool,
    peak_total: usize,
}

pub struct CountingAllocator(Mutex<State>);

#[repr(C)]
struct Header {
    origin: usize,
    site: usize,
}

fn storage(layout: Layout) -> Option<(Layout, usize)> {
    Layout::new::<Header>()
        .extend(layout)
        .ok()
        .map(|(layout, offset)| (layout.pad_to_align(), offset))
}

impl CountingAllocator {
    pub fn retention_enable(&self, path: &str) {
        retention::enable(path);
        self.snapshot("pre_runtime", 0);
    }

    pub fn snapshot(&self, label: &str, cohort: usize) {
        let counts = self.0.lock().unwrap_or_else(|e| e.into_inner()).counts;
        retention::snapshot(label, cohort, &counts);
    }

    pub async fn retention_control(&'static self, kind: String) {
        assert!(
            ["wait-scoped", "wait-unscoped", "nop-scoped", "nop-unscoped"].contains(&kind.as_str())
        );
        let scoped = kind.ends_with("-scoped");
        let waits = kind.starts_with("wait-");
        let work = async {
            for count in 1..=4096 {
                if waits {
                    let event = kimojio::AsyncEvent::new();
                    let wait = event.wait();
                    let mut wait = std::pin::pin!(wait);
                    futures::future::poll_fn(|cx| {
                        assert!(std::future::Future::poll(wait.as_mut(), cx).is_pending());
                        std::task::Poll::Ready(())
                    })
                    .await;
                } else {
                    kimojio::operations::nop().await.unwrap();
                }
                if [1, 2, 8, 32, 128, 512, 4096].contains(&count) {
                    self.snapshot("control_live", count);
                }
            }
        };
        if scoped {
            kimojio::operations::io_scope(async move || work.await).await;
        } else {
            work.await;
        }
        self.snapshot("control_finished", 4096);
    }

    pub const fn new() -> Self {
        const ZERO: Counts = Counts {
            allocations: 0,
            reallocations: 0,
            deallocations: 0,
            live: 0,
            peak: 0,
        };
        Self(Mutex::new(State {
            counts: [ZERO; 3],
            before: None,
            invalid: false,
            peak_total: 0,
        }))
    }

    fn record(state: &mut State, origin: usize, removed: usize, added: usize, operation: usize) {
        let count = &mut state.counts[origin];
        let counter = match operation {
            0 => &mut count.allocations,
            1 => &mut count.reallocations,
            _ => &mut count.deallocations,
        };
        if let Some(next) = counter.checked_add(1) {
            *counter = next;
        } else {
            state.invalid = true;
        }
        if let Some(live) = count
            .live
            .checked_sub(removed)
            .and_then(|v| v.checked_add(added))
        {
            count.live = live;
            count.peak = count.peak.max(live);
        } else {
            state.invalid = true;
        }
        if let Some(total) = state
            .counts
            .iter()
            .try_fold(0usize, |sum, c| sum.checked_add(c.live))
        {
            state.peak_total = state.peak_total.max(total);
        } else {
            state.invalid = true;
        }
    }

    pub fn begin(&self) {
        let active = {
            let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
            let active = state.before.is_some();
            if !active {
                state.before = Some(state.counts);
                for count in &mut state.counts {
                    count.peak = count.live;
                }
                state.peak_total = state.counts.iter().map(|c| c.live).sum();
            }
            active
        };
        assert!(!active, "allocation windows cannot overlap");
    }

    pub fn finish(&self) -> ([Counts; 3], [Counts; 3], usize) {
        let (before, after, invalid, peak) = {
            let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
            (
                state.before.take(),
                state.counts,
                state.invalid,
                state.peak_total,
            )
        };
        assert!(!invalid, "allocation ledger overflow or unmatched free");
        (before.expect("active allocation window"), after, peak)
    }
}

// The prefix preserves allocation origin across task, context, and window boundaries.
// System receives the expanded layout. The ledger counts only caller-requested bytes.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let Some((backing, offset)) = storage(layout) else {
            return ptr::null_mut();
        };
        let origin = CONTEXT.try_with(Cell::get).unwrap_or(0);
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let base = unsafe { System.alloc(backing) };
        if base.is_null() {
            return base;
        }
        unsafe {
            base.cast::<Header>().write(Header {
                origin,
                site: retention::allocated(layout.size()),
            });
        }
        Self::record(&mut state, origin, 0, layout.size(), 0);
        unsafe { base.add(offset) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let Some((backing, offset)) = storage(layout) else {
            return ptr::null_mut();
        };
        let origin = CONTEXT.try_with(Cell::get).unwrap_or(0);
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let base = unsafe { System.alloc_zeroed(backing) };
        if base.is_null() {
            return base;
        }
        unsafe {
            base.cast::<Header>().write(Header {
                origin,
                site: retention::allocated(layout.size()),
            });
        }
        Self::record(&mut state, origin, 0, layout.size(), 0);
        unsafe { base.add(offset) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        let Some((backing, offset)) = storage(layout) else {
            std::process::abort()
        };
        let base = unsafe { pointer.sub(offset) };
        let origin = unsafe { (*base.cast::<Header>()).origin };
        retention::freed(unsafe { (*base.cast::<Header>()).site }, layout.size());
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            System.dealloc(base, backing);
        }
        Self::record(&mut state, origin, layout.size(), 0, 2);
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let Some((old, offset)) = storage(layout) else {
            return ptr::null_mut();
        };
        let Ok(new) = Layout::from_size_align(size, layout.align()) else {
            return ptr::null_mut();
        };
        let Some((new, new_offset)) = storage(new) else {
            return ptr::null_mut();
        };
        if offset != new_offset {
            return ptr::null_mut();
        }
        let base = unsafe { pointer.sub(offset) };
        let origin = unsafe { (*base.cast::<Header>()).origin };
        let site = unsafe { (*base.cast::<Header>()).site };
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let next = unsafe { System.realloc(base, old, new.size()) };
        if next.is_null() {
            return next;
        }
        retention::resized(site, layout.size(), size);
        Self::record(&mut state, origin, layout.size(), size, 1);
        unsafe { next.add(offset) }
    }
}

#[derive(Clone, Copy)]
pub struct AllocationMeter(pub &'static CountingAllocator);

impl Meter for AllocationMeter {
    fn checkpoint(self, label: &str, cohort: usize) {
        self.0.snapshot(label, cohort);
    }
    fn begin(self) {
        self.0.begin();
    }
    fn cancel(self) {
        self.0.0.lock().unwrap_or_else(|e| e.into_inner()).before = None;
    }
    fn end(self) -> Value {
        let (before, after, peak) = self.0.finish();
        let buckets: Vec<_> = before.iter().zip(after).enumerate().map(|(index, (old, new))| json!({
            "origin": (["runtime_and_unattributed_wrapper_workers", "application_facing_polls", "connection_driver_polls"][index]),
            "allocations": new.allocations - old.allocations,
            "reallocations": new.reallocations - old.reallocations,
            "deallocations": new.deallocations - old.deallocations,
            "requested_live_start": old.live, "requested_live_end": new.live,
            "requested_peak_live": new.peak,
            "requested_live_change": new.live as i128 - old.live as i128,
        })).collect();
        json!({
            "origin_buckets": buckets,
            "requested_live_start": before.iter().map(|c| c.live).sum::<usize>(),
            "requested_live_end": after.iter().map(|c| c.live).sum::<usize>(),
            "requested_peak_live": peak,
            "origin_peak_sum_is_not_simultaneous_peak": true,
            "tracking_header_bytes_excluded": std::mem::size_of::<Header>(),
            "tracking_alignment_padding_excluded": true,
            "allocation_probe_not_timing_evidence": true,
        })
    }
    fn enter<T>(self, tag: Tag, action: impl FnOnce() -> T) -> T {
        in_context(tag as usize, action)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn alignment_zeroing_reallocation_and_origin_survive_boundary() {
        use super::*;
        let allocator = CountingAllocator::new();
        let first = Layout::from_size_align(32, 64).unwrap();
        let pointer = in_context(1, || unsafe { allocator.alloc_zeroed(first) });
        assert!(!pointer.is_null());
        assert_eq!(pointer as usize % 64, 0);
        assert_eq!(unsafe { std::slice::from_raw_parts(pointer, 32) }, &[0; 32]);
        allocator.begin();
        assert!(unsafe { allocator.realloc(pointer, first, usize::MAX) }.is_null());
        let pointer = in_context(2, || unsafe { allocator.realloc(pointer, first, 96) });
        assert!(!pointer.is_null());
        assert_eq!(pointer as usize % 64, 0);
        assert_eq!(unsafe { std::slice::from_raw_parts(pointer, 32) }, &[0; 32]);
        let pointer =
            unsafe { allocator.realloc(pointer, Layout::from_size_align(96, 64).unwrap(), 16) };
        assert!(!pointer.is_null());
        unsafe {
            allocator.dealloc(pointer, Layout::from_size_align(16, 64).unwrap());
        }
        let (before, after, peak) = allocator.finish();
        assert_eq!(before[1].live, 32);
        assert_eq!(after[1].live, 0);
        assert_eq!(after[1].peak, 96);
        assert_eq!(after[1].allocations - before[1].allocations, 0);
        assert_eq!(after[1].reallocations - before[1].reallocations, 2);
        assert_eq!(after[1].deallocations - before[1].deallocations, 1);
        assert_eq!(after[2].reallocations, 0);
        assert_eq!(peak, 96);
    }

    #[test]
    fn free_before_window_and_zero_growth_do_not_hide_activity() {
        use super::*;
        let allocator = CountingAllocator::new();
        let large = Layout::from_size_align(64, 8).unwrap();
        let small = Layout::from_size_align(16, 8).unwrap();
        let old = unsafe { allocator.alloc(large) };
        assert!(!old.is_null());
        allocator.begin();
        unsafe {
            allocator.dealloc(old, large);
        }
        let pointer = unsafe { allocator.alloc(small) };
        assert!(!pointer.is_null());
        unsafe {
            allocator.dealloc(pointer, small);
        }
        let (before, after, peak) = allocator.finish();
        assert_eq!((before[0].live, after[0].live, after[0].peak), (64, 0, 64));
        assert_eq!(after[0].allocations - before[0].allocations, 1);
        assert_eq!(after[0].deallocations - before[0].deallocations, 2);
        assert_eq!(peak, 64);
    }

    #[test]
    fn disjoint_origin_peaks_are_not_added() {
        use super::*;
        let allocator = CountingAllocator::new();
        let first = Layout::from_size_align(64, 8).unwrap();
        let second = Layout::from_size_align(32, 8).unwrap();
        allocator.begin();
        let pointer = in_context(1, || unsafe { allocator.alloc(first) });
        assert!(!pointer.is_null());
        unsafe { allocator.dealloc(pointer, first) };
        let pointer = in_context(2, || unsafe { allocator.alloc(second) });
        assert!(!pointer.is_null());
        unsafe { allocator.dealloc(pointer, second) };
        let (_, after, peak) = allocator.finish();
        assert_eq!(after[1].peak + after[2].peak, 96);
        assert_eq!(peak, 64);
        assert_eq!(after.iter().map(|c| c.live).sum::<usize>(), 0);
    }
}
