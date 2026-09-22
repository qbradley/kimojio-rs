// Separate from the Criterion executable: allocator instrumentation must not
// enter timing/profile results. Count only allocations on this runtime thread.
#[path = "../benches/support/mod.rs"]
#[allow(dead_code)]
mod support;

use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    hint::black_box,
};
use support::*;

#[derive(Clone, Copy, Debug, Default)]
struct Counts {
    alloc: u64,
    realloc: u64,
    requested: u64,
}
thread_local! { static COUNTS: Cell<Option<Counts>> = const { Cell::new(None) }; }
fn record(size: usize, realloc: bool) {
    let _ = COUNTS.try_with(|cell| {
        if let Some(mut c) = cell.get() {
            if realloc {
                c.realloc += 1;
            } else {
                c.alloc += 1;
            }
            c.requested += size as u64;
            cell.set(Some(c));
        }
    });
}
struct Allocator;
// SAFETY: System receives exactly the original pointer/layout/size. Counter
// operations use only thread-local Cell access, with no allocation or locking.
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        record(layout.size(), false);
        ptr
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        record(layout.size(), false);
        ptr
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let ptr = unsafe { System.realloc(ptr, layout, size) };
        record(size, true);
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
    }
}
#[global_allocator]
static ALLOCATOR: Allocator = Allocator;

fn counted(fixture: &Fixture, options: Options, iterations: u64) -> Counts {
    COUNTS.with(|c| assert!(c.replace(Some(Counts::default())).is_none()));
    black_box(run::<false>(fixture, options, iterations));
    COUNTS.with(|c| c.replace(None).unwrap())
}

#[test]
fn allocation_slopes_use_the_timing_driver_without_byte_comparison_or_logging() {
    let native = Options::new(Backend::Native);
    for (name, fixture, options, n) in [
        ("empty/native", Fixture::new(0, false, IO_BYTES), native, 64),
        (
            "fixed/native",
            Fixture::new(128, false, IO_BYTES),
            native,
            64,
        ),
        (
            "fixed/stream",
            Fixture::new(128, false, IO_BYTES),
            Options::new(Backend::Stream),
            64,
        ),
        (
            "fixed/coalesced",
            Fixture::new(128, false, IO_BYTES),
            Options {
                coalesce: true,
                ..native
            },
            64,
        ),
        (
            "fixed/no_deadlines",
            Fixture::new(128, false, IO_BYTES),
            Options {
                deadlines: false,
                ..native
            },
            64,
        ),
        (
            "chunked/native",
            Fixture::new(1024 * 1024, true, IO_BYTES),
            native,
            4,
        ),
        (
            "chunked/no_deadlines",
            Fixture::new(1024 * 1024, true, IO_BYTES),
            Options {
                deadlines: false,
                ..native
            },
            4,
        ),
        (
            "chunked/forward",
            Fixture::new(1024 * 1024, true, IO_BYTES),
            Options {
                mode: Mode::Forward,
                ..native
            },
            4,
        ),
        (
            "chunked/copy_forward",
            Fixture::new(1024 * 1024, true, IO_BYTES),
            Options {
                mode: Mode::CopyForward,
                ..native
            },
            4,
        ),
    ] {
        // Initialize process-wide/runtime lazy state before either count.
        black_box(run::<false>(&fixture, options, 1));
        let small = counted(&fixture, options, n);
        let large = counted(&fixture, options, 2 * n);
        assert!(large.alloc > small.alloc);
        eprintln!(
            "{name}: n={n} small={small:?} large={large:?} alloc/exchange={:.2} realloc/exchange={:.2} requested_bytes/exchange={:.2}",
            (large.alloc as f64 - small.alloc as f64) / n as f64,
            (large.realloc as f64 - small.realloc as f64) / n as f64,
            (large.requested as f64 - small.requested as f64) / n as f64
        );
    }
}
