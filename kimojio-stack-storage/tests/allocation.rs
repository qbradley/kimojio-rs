// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use bytes::Bytes;
use kimojio_stack_storage::{
    ConcurrencyLimiter, ListOptions, ObjectName, OperationClass, PageRange, ReplayBody,
    RequestCounts, RetryDecision, RetryObservation, RetryPolicy,
};

struct CountingAllocator;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ACTIVE.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

fn measure<T>(f: impl FnOnce() -> T) -> (T, RequestCounts) {
    ALLOCATIONS.store(0, Ordering::Relaxed);
    ALLOCATED_BYTES.store(0, Ordering::Relaxed);
    ACTIVE.store(true, Ordering::Relaxed);
    let output = f();
    ACTIVE.store(false, Ordering::Relaxed);
    (
        output,
        RequestCounts {
            requests: 0,
            retries: 0,
            allocations: ALLOCATIONS.load(Ordering::Relaxed),
            allocated_bytes: ALLOCATED_BYTES.load(Ordering::Relaxed),
        },
    )
}

#[test]
fn allocation_and_request_counts_compare_operation_paths() {
    let (page_request, page_counts) = measure(|| {
        let object = test_object("data/page");
        kimojio_stack_storage::upload_range_request(
            &object,
            PageRange::aligned_len(0, 512).unwrap(),
            ReplayBody::from_vec(vec![0; 512]),
            None,
        )
        .unwrap()
    });
    let (_, metadata_counts) = measure(|| {
        kimojio_stack_storage::object_properties_request(&test_object("metadata/root"), None, None)
    });
    let (_, list_counts) = measure(|| {
        kimojio_stack_storage::list_request(
            &kimojio_stack_storage::AccountId::new("acct"),
            &kimojio_stack_storage::ContainerName::new("container"),
            &ListOptions {
                prefix: Some("data/".into()),
                include_metadata: true,
                ..ListOptions::default()
            },
        )
    });
    let (_, range_counts) = measure(|| {
        kimojio_stack_storage::read_range_request(
            &test_object("data/page"),
            PageRange::new(0, 511).unwrap(),
        )
    });
    let (_, retry_counts) = measure(|| {
        RetryPolicy::default().decide(
            1,
            OperationClass::List,
            &ReplayBody::Empty,
            &kimojio_stack_storage::Error::new(
                kimojio_stack_storage::ErrorKind::Unavailable,
                "busy",
            ),
            None,
        )
    });

    let mut counts = RequestCounts::default();
    for retry in [false, false, false, true] {
        counts.record_request(retry);
    }
    counts.record_allocation(
        page_counts.allocated_bytes
            + metadata_counts.allocated_bytes
            + list_counts.allocated_bytes
            + range_counts.allocated_bytes
            + retry_counts.allocated_bytes,
    );

    assert_eq!(page_request.metadata.get("content-length"), Some("512"));
    counts.record_request(false);
    assert_eq!(counts.requests, 5);
    assert_eq!(counts.retries, 1);
    assert!(counts.allocations >= 1);
    assert!(page_counts.allocated_bytes <= 16 * 1024);
    assert!(metadata_counts.allocated_bytes <= 64 * 1024);
    assert!(range_counts.allocated_bytes <= 64 * 1024);
    assert!(list_counts.allocated_bytes <= 64 * 1024);
    assert!(retry_counts.allocated_bytes <= 1024);
}

#[test]
fn shared_page_upload_allocation_does_not_scale_with_payload_size() {
    static SMALL: [u8; 512] = [0; 512];
    static LARGE: [u8; 4096] = [0; 4096];
    let object = test_object("data/page");
    let (_, small) = measure(|| {
        kimojio_stack_storage::upload_range_request(
            &object,
            PageRange::aligned_len(0, 512).unwrap(),
            ReplayBody::from_bytes(Bytes::from_static(&SMALL)),
            None,
        )
        .unwrap()
    });
    let (_, large) = measure(|| {
        kimojio_stack_storage::upload_range_request(
            &object,
            PageRange::aligned_len(0, 4096).unwrap(),
            ReplayBody::from_bytes(Bytes::from_static(&LARGE)),
            None,
        )
        .unwrap()
    });

    assert!(large.allocated_bytes.saturating_sub(small.allocated_bytes) < 2048);
}

#[test]
fn bounded_concurrency_limiter_models_caller_and_operation_limits() {
    let caller = ConcurrencyLimiter::new(1);
    let operation = ConcurrencyLimiter::new(1);
    let caller_permit = caller.try_acquire().unwrap();
    let operation_permit = operation.try_acquire().unwrap();

    assert!(caller.try_acquire().is_none());
    assert!(operation.try_acquire().is_none());
    drop(operation_permit);
    drop(caller_permit);
    assert!(caller.try_acquire().is_some());
    assert!(operation.try_acquire().is_some());
}

#[test]
fn retry_observation_records_request_loop_decision() {
    let observation = RetryObservation {
        attempt: 1,
        decision: RetryDecision::DoNotRetry,
        error_kind: kimojio_stack_storage::ErrorKind::Authorization,
    };

    assert_eq!(observation.attempt, 1);
}

fn test_object(name: &str) -> kimojio_stack_storage::ObjectRef {
    kimojio_stack_storage::ObjectRef {
        account: kimojio_stack_storage::AccountId::new("acct"),
        container: kimojio_stack_storage::ContainerName::new("container"),
        name: ObjectName::new(name),
        kind: kimojio_stack_storage::ObjectKind::Data,
    }
}
