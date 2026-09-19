use std::{
    alloc::{GlobalAlloc, Layout, System},
    io::Write,
    process::{ExitCode, Termination},
    sync::atomic::{AtomicU64, Ordering::Relaxed},
};

#[derive(Default)]
struct Counts {
    alloc: AtomicU64,
    zeroed: AtomicU64,
    realloc: AtomicU64,
    dealloc: AtomicU64,
    failed: AtomicU64,
    requested: AtomicU64,
    live: AtomicU64,
    peak: AtomicU64,
}

impl Counts {
    const fn new() -> Self {
        Self {
            alloc: AtomicU64::new(0),
            zeroed: AtomicU64::new(0),
            realloc: AtomicU64::new(0),
            dealloc: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            requested: AtomicU64::new(0),
            live: AtomicU64::new(0),
            peak: AtomicU64::new(0),
        }
    }

    fn allocation_result(&self, old: usize, new: usize, success: bool) {
        if !success {
            self.failed.fetch_add(1, Relaxed);
            return;
        }
        self.requested.fetch_add(new as u64, Relaxed);
        if new >= old {
            let live = self.live.fetch_add((new - old) as u64, Relaxed) + (new - old) as u64;
            self.peak.fetch_max(live, Relaxed);
        } else {
            self.live.fetch_sub((old - new) as u64, Relaxed);
        }
    }

    fn release(&self, size: usize) {
        self.dealloc.fetch_add(1, Relaxed);
        self.live.fetch_sub(size as u64, Relaxed);
    }
}

static COUNTS: Counts = Counts::new();

struct CountingAllocator;

// Each operation forwards its pointer and layout unchanged to the system allocator.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        COUNTS.alloc.fetch_add(1, Relaxed);
        COUNTS.allocation_result(0, layout.size(), !pointer.is_null());
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        COUNTS.zeroed.fetch_add(1, Relaxed);
        COUNTS.allocation_result(0, layout.size(), !pointer.is_null());
        pointer
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let replacement = unsafe { System.realloc(pointer, layout, new_size) };
        COUNTS.realloc.fetch_add(1, Relaxed);
        COUNTS.allocation_result(layout.size(), new_size, !replacement.is_null());
        replacement
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
        COUNTS.release(layout.size());
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Reports process-wide Rust allocator calls after the original executable returns.
pub fn finish(result: impl Termination) -> ExitCode {
    let code = result.report();
    let alloc = COUNTS.alloc.load(Relaxed);
    let zeroed = COUNTS.zeroed.load(Relaxed);
    let realloc = COUNTS.realloc.load(Relaxed);
    let dealloc = COUNTS.dealloc.load(Relaxed);
    let failed = COUNTS.failed.load(Relaxed);
    let requested = COUNTS.requested.load(Relaxed);
    let live = COUNTS.live.load(Relaxed);
    let peak = COUNTS.peak.load(Relaxed);
    let result = writeln!(
        std::io::stderr().lock(),
        "ALLOC_STATS {{\"alloc_calls\":{alloc},\"zeroed_calls\":{zeroed},\"realloc_calls\":{realloc},\"dealloc_calls\":{dealloc},\"failed_calls\":{failed},\"requested_bytes\":{requested},\"live_bytes\":{live},\"peak_live_bytes\":{peak}}}"
    );
    if let Err(error) = result {
        eprintln!("allocation report failed: {error}");
        return ExitCode::FAILURE;
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_sizes_follow_successful_growth_shrink_and_release() {
        let counts = Counts::new();
        counts.allocation_result(0, 64, true);
        counts.allocation_result(0, 32, true);
        counts.allocation_result(64, 128, true);
        assert_eq!(counts.live.load(Relaxed), 160);
        assert_eq!(counts.peak.load(Relaxed), 160);
        counts.allocation_result(128, 16, true);
        counts.release(16);
        counts.release(32);
        assert_eq!(counts.live.load(Relaxed), 0);
        assert_eq!(counts.peak.load(Relaxed), 160);
        assert_eq!(counts.requested.load(Relaxed), 240);
        assert_eq!(counts.dealloc.load(Relaxed), 2);
    }

    #[test]
    fn failed_reallocation_preserves_the_old_allocation() {
        let counts = Counts::new();
        counts.allocation_result(0, 64, true);
        counts.allocation_result(64, 1024, false);
        assert_eq!(counts.failed.load(Relaxed), 1);
        assert_eq!(counts.live.load(Relaxed), 64);
        assert_eq!(counts.requested.load(Relaxed), 64);
        counts.release(64);
        assert_eq!(counts.live.load(Relaxed), 0);
    }
}
