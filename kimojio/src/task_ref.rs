// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//! Waker implementation
//!
use std::cell::Cell;
use std::task::{RawWaker, RawWakerVTable, Waker};

use crate::task::TaskState;

thread_local! {
    /// The index of the current kimojio runtime thread.
    /// Initialized to u8::MAX (sentinel for "not a kimojio thread").
    static KIMOJIO_THREAD_INDEX: Cell<u8> = const { Cell::new(u8::MAX) };
}

/// Sets the thread index for the current thread.
/// This must be called during Runtime initialization.
pub(crate) fn set_kimojio_thread_index(index: u8) {
    debug_assert!(
        index < u8::MAX,
        "thread_index must be < {} (sentinel value)",
        u8::MAX
    );
    let current = KIMOJIO_THREAD_INDEX.get();
    if current != u8::MAX && current != index {
        panic!(
            "kimojio thread index already set to {}, cannot set to {}",
            current, index
        );
    }
    KIMOJIO_THREAD_INDEX.set(index);
}

/// Resets the thread index. Used for testing/cleanup.
pub(crate) fn reset_kimojio_thread_index() {
    KIMOJIO_THREAD_INDEX.set(u8::MAX);
}

// Layout:
// usize = (thread_index as usize) << 16 | (task_index as usize)
fn pack_waker_data(thread_index: u8, task_index: u16) -> *const () {
    let val: usize = ((thread_index as usize) << 16) | (task_index as usize);
    val as *const ()
}

fn unpack_waker_data(data: *const ()) -> (u8, u16) {
    let val = data as usize;
    let thread_index = (val >> 16) as u8;
    let task_index = val as u16;
    (thread_index, task_index)
}

/// wake_task can be used when you already have a &mut TaskState reference. This avoids
/// recursive TaskState::get() calls.
pub fn wake_task(task_state: &mut TaskState, waker: &Waker) {
    // Optimization: if it's our waker AND it belongs to this thread, use direct lookup
    if waker.vtable() == &VTABLE {
        let (thread_index, task_index) = unpack_waker_data(waker.data());

        // Safety: Only perform direct lookup if waker belongs to CURRENT thread
        if thread_index == KIMOJIO_THREAD_INDEX.get() {
            if let Some(task) = task_state.get_task_by_index(task_index) {
                task_state.schedule_io(task);
            }
            return;
        }
    }
    // Fallback: cross-thread or foreign waker
    waker.wake_by_ref();
}

unsafe fn clone_waker(data: *const ()) -> RawWaker {
    // Data is packed usize (Copy), just return a new RawWaker
    RawWaker::new(data, &VTABLE)
}

unsafe fn wake_waker(data: *const ()) {
    // wake consumes the waker, but our data is Copy so it's same as wake_by_ref
    // SAFETY: wake_by_ref_waker upholds the same invariants required by wake_waker
    unsafe { wake_by_ref_waker(data) }
}

unsafe fn wake_by_ref_waker(data: *const ()) {
    let (thread_index, task_index) = unpack_waker_data(data);
    let current_thread = KIMOJIO_THREAD_INDEX.get();

    if thread_index == current_thread {
        // Local path: lock-free, atomic-free
        // We are on the owning thread, so TaskState is local
        let mut task_state = TaskState::get();
        if let Some(task) = task_state.get_task_by_index(task_index) {
            task_state.schedule_io(task);
        }
    } else {
        // Cross-thread path
        // Phase 1: Panic if cross-thread wake attempted
        // In Phase 2 this will call cross_thread_wake(thread_index, task_index)
        panic!(
            "Cross-thread wake not implemented in Phase 1 (target: {}, current: {})",
            thread_index, current_thread
        );
    }
}

unsafe fn drop_waker(_data: *const ()) {
    // No-op: no allocation to free
}

static VTABLE: RawWakerVTable =
    RawWakerVTable::new(clone_waker, wake_waker, wake_by_ref_waker, drop_waker);

pub fn create_waker(task_index: u16) -> Waker {
    let thread_index = KIMOJIO_THREAD_INDEX.get();
    debug_assert!(
        thread_index < u8::MAX,
        "create_waker called on non-kimojio thread"
    );

    let data = pack_waker_data(thread_index, task_index);
    let raw = RawWaker::new(data, &VTABLE);
    // SAFETY: RawWakerVTable contract is upheld by implementation
    unsafe { Waker::from_raw(raw) }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_waker_send_sync() {
        // This test mostly verifies that we haven't done something weird that makes
        // Waker !Send or !Sync (which shouldn't be possible for std::task::Waker,
        // but good to be sure).
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Waker>();
    }
}
