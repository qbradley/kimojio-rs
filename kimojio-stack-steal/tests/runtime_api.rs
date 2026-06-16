// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::num::NonZeroUsize;
use std::time::Duration;

use kimojio_stack::{
    Errno, IoRuntime, Runtime as StackCoreRuntime, RuntimeCapabilities, RuntimeCapability,
    RuntimeIoError, RuntimeIoErrorKind, RuntimeReadResult, RuntimeWaitable, RuntimeWriteResult,
    SocketIoRuntime, StackRuntime as StackRuntimeTrait, StackRuntimeContext, StackfulWaitContext,
};
use kimojio_stack_steal::{
    RingError, RingFd, RingMode, Runtime as StealRuntime, RuntimeConfig, StealPolicy,
};
use rustix::pipe::pipe;

#[global_allocator]
static TEST_ALLOCATOR: allocation_tracking::CountingAllocator =
    allocation_tracking::CountingAllocator;

mod allocation_tracking {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    thread_local! {
        static ACTIVE: Cell<bool> = const { Cell::new(false) };
    }

    static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
    static ALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);
    static REALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
    static REALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);
    static MEASUREMENT_LOCK: Mutex<()> = Mutex::new(());

    pub struct CountingAllocator;

    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct AllocationCounts {
        pub allocations: usize,
        pub allocated_bytes: usize,
        pub reallocations: usize,
        pub reallocated_bytes: usize,
    }

    impl AllocationCounts {
        pub fn allocating_operations(self) -> usize {
            self.allocations + self.reallocations
        }

        pub fn allocated_or_reallocated_bytes(self) -> usize {
            self.allocated_bytes + self.reallocated_bytes
        }
    }

    pub fn measure<T>(f: impl FnOnce() -> T) -> (T, AllocationCounts) {
        let _measurement = MEASUREMENT_LOCK.lock().unwrap();

        ACTIVE.with(|active| {
            assert!(!active.get(), "nested allocation measurement");
        });
        reset();

        ACTIVE.with(|active| active.set(true));
        let guard = MeasurementGuard;
        let output = f();
        drop(guard);

        (output, current())
    }

    fn reset() {
        ALLOCATIONS.store(0, Ordering::Relaxed);
        ALLOCATED_BYTES.store(0, Ordering::Relaxed);
        REALLOCATIONS.store(0, Ordering::Relaxed);
        REALLOCATED_BYTES.store(0, Ordering::Relaxed);
    }

    fn current() -> AllocationCounts {
        AllocationCounts {
            allocations: ALLOCATIONS.load(Ordering::Relaxed),
            allocated_bytes: ALLOCATED_BYTES.load(Ordering::Relaxed),
            reallocations: REALLOCATIONS.load(Ordering::Relaxed),
            reallocated_bytes: REALLOCATED_BYTES.load(Ordering::Relaxed),
        }
    }

    fn record_allocation(bytes: usize) {
        ACTIVE.with(|active| {
            if active.get() {
                ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
                ALLOCATED_BYTES.fetch_add(bytes, Ordering::Relaxed);
            }
        });
    }

    fn record_reallocation(bytes: usize) {
        ACTIVE.with(|active| {
            if active.get() {
                REALLOCATIONS.fetch_add(1, Ordering::Relaxed);
                REALLOCATED_BYTES.fetch_add(bytes, Ordering::Relaxed);
            }
        });
    }

    struct MeasurementGuard;

    impl Drop for MeasurementGuard {
        fn drop(&mut self) {
            ACTIVE.with(|active| active.set(false));
        }
    }

    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            record_allocation(layout.size());
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            record_allocation(layout.size());
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            record_reallocation(new_size);
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }
}

fn runtime_agnostic_component<R>(runtime: &mut R, explicit_ring_io_supported: bool)
where
    R: StackRuntimeTrait,
    for<'cx> R::Context<'cx>: SocketIoRuntime,
{
    runtime.block_on(|cx| {
        assert!(cx.supports(RuntimeCapability::StackfulWait));
        cx.yield_now();
        let borrowed = 41;
        assert_eq!(
            cx.spawn_scoped(|child| {
                child.yield_now();
                child.sleep_for(Duration::from_millis(0)).unwrap();
                borrowed + 1
            }),
            42
        );
        assert_eq!(
            cx.spawn_stealable_scoped(|child| {
                child.yield_now();
                7
            }),
            7
        );
        let ring_capability = cx.require_explicit_ring_io();
        assert_eq!(ring_capability.is_ok(), explicit_ring_io_supported);
        if !explicit_ring_io_supported {
            let error = ring_capability.unwrap_err();
            assert_eq!(error.capability(), "explicit-ring-io");
        }

        cx.require_socket_io().unwrap();
        if !explicit_ring_io_supported {
            cx.sleep_for(Duration::from_millis(0)).unwrap();

            let (read_fd, write_fd) = pipe().unwrap();
            let read_fd = cx.socket_from_owned_fd(read_fd).unwrap();
            let write_fd = cx.socket_from_owned_fd(write_fd).unwrap();
            assert_eq!(cx.write(&write_fd, b"io").unwrap(), 2);
            let mut buffer = [0_u8; 2];
            assert_eq!(cx.read(&read_fd, &mut buffer).unwrap(), 2);
            assert_eq!(&buffer, b"io");

            let mut read = cx.read_async(&read_fd, vec![0_u8; 2]).unwrap();
            let mut write = cx.write_async(&write_fd, b"ok".to_vec()).unwrap();
            let mut read_done = false;
            let mut write_done = false;
            for _ in 0..128 {
                if !write_done && let Some(result) = RuntimeWriteResult::try_get(&mut write) {
                    result.unwrap();
                    write_done = true;
                }
                if !read_done && let Some(result) = RuntimeReadResult::try_get(&mut read) {
                    result.unwrap();
                    read_done = true;
                }
                if read_done && write_done {
                    break;
                }
                cx.sleep_for(Duration::from_millis(0)).unwrap();
            }
            assert!(read_done);
            assert!(write_done);
            SocketIoRuntime::close(cx, read_fd).unwrap();
            SocketIoRuntime::close(cx, write_fd).unwrap();
        }
    });
}

#[test]
fn runtime_agnostic_component_runs_on_stack_runtime() {
    let mut runtime = StackCoreRuntime::new();

    runtime_agnostic_component(&mut runtime, false);
}

#[test]
fn runtime_agnostic_component_runs_on_stealing_runtime() {
    let mut runtime = StealRuntime::with_config(RuntimeConfig {
        workers: NonZeroUsize::new(2).unwrap(),
        steal_policy: StealPolicy::steal_one(),
        ..RuntimeConfig::default()
    });

    runtime_agnostic_component(&mut runtime, true);
}

fn assert_runtime_io_kind(error: RingError, kind: RuntimeIoErrorKind) {
    let runtime_error = RuntimeIoError::from(error);
    assert_eq!(runtime_error.runtime_kind(), Some(kind));
    assert_eq!(runtime_error, RuntimeIoError::Runtime(kind));
    assert_eq!(runtime_error.to_string(), kind.as_str());
}

#[test]
fn stealing_runtime_maps_ring_diagnostics_to_runtime_io_categories() {
    let worker_ring = {
        let mut runtime = StealRuntime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });

        runtime.block_on(|cx| {
            assert_runtime_io_kind(
                cx.create_ring(RingMode::WorkerLocal).unwrap_err(),
                RuntimeIoErrorKind::NoCurrentWorker,
            );
            cx.scope(|scope| {
                let handle = scope.spawn_stealable(|cx| cx.create_worker_ring());
                handle.join(cx)
            })
        })
    };

    let mut same_runtime = StealRuntime::with_config(RuntimeConfig {
        workers: NonZeroUsize::new(2).unwrap(),
        steal_policy: StealPolicy::steal_one(),
        ..RuntimeConfig::default()
    });
    same_runtime.block_on(|cx| {
        assert_runtime_io_kind(
            worker_ring.nop(cx).unwrap_err(),
            RuntimeIoErrorKind::WrongRuntime,
        );
    });

    let mut wrong_worker_runtime = StealRuntime::with_config(RuntimeConfig {
        workers: NonZeroUsize::new(2).unwrap(),
        steal_policy: StealPolicy::steal_one(),
        ..RuntimeConfig::default()
    });
    wrong_worker_runtime.block_on(|cx| {
        let ring = cx.scope(|scope| {
            let handle = scope.spawn_stealable(|cx| cx.create_worker_ring());
            handle.join(cx)
        });
        assert_runtime_io_kind(ring.nop(cx).unwrap_err(), RuntimeIoErrorKind::WrongWorker);
    });

    let mut full_queue_runtime = StealRuntime::with_config(RuntimeConfig {
        max_shared_ring_queue_len: 0,
        ..RuntimeConfig::default()
    });
    full_queue_runtime.block_on(|cx| {
        let ring = cx.create_shared_ring();
        assert_runtime_io_kind(ring.nop(cx).unwrap_err(), RuntimeIoErrorKind::QueueFull);
    });

    let mut fd_in_use_runtime = StealRuntime::new();
    fd_in_use_runtime.block_on(|cx| {
        let ring = cx.create_worker_ring();
        let (read_fd, _write_fd) = pipe().unwrap();
        let fd = RingFd::from_owned(read_fd);
        let fd_clone = fd.clone();
        assert_runtime_io_kind(ring.close(cx, fd).unwrap_err(), RuntimeIoErrorKind::FdInUse);
        ring.close(cx, fd_clone).unwrap();
    });

    let mut no_stackful_context_runtime = StealRuntime::with_config(RuntimeConfig {
        workers: NonZeroUsize::new(2).unwrap(),
        steal_policy: StealPolicy::steal_one(),
        ..RuntimeConfig::default()
    });
    no_stackful_context_runtime.block_on(|cx| {
        let ring = cx.create_shared_ring();
        let timeout = ring.timeout(cx, Duration::from_secs(60)).unwrap();
        assert_runtime_io_kind(
            timeout.wait(cx).unwrap_err(),
            RuntimeIoErrorKind::NoStackfulContext,
        );
    });

    assert_runtime_io_kind(RingError::Canceled, RuntimeIoErrorKind::Canceled);
}

#[test]
fn stealing_runtime_neutral_waiter_creation_after_registration_is_allocation_free() {
    let mut runtime = StealRuntime::with_config(RuntimeConfig {
        workers: NonZeroUsize::new(2).unwrap(),
        steal_policy: StealPolicy::steal_one(),
        ..RuntimeConfig::default()
    });

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let worker = scope.spawn_stealable(|cx| {
                let registration = cx
                    .stackful_wait_registration()
                    .expect("stealable task should provide stackful wait registration");

                let (_, counts) = allocation_tracking::measure(|| {
                    for _ in 0..2 {
                        let waiter = registration.waiter();
                        std::hint::black_box(waiter.is_active());
                    }
                });

                assert_eq!(counts.allocating_operations(), 0, "{counts:?}");
                assert_eq!(counts.allocated_or_reallocated_bytes(), 0, "{counts:?}");
            });

            worker.join(cx);
        });
    });
}

#[test]
fn direct_result_cancel_detaches_stealing_runtime_handle() {
    let mut runtime = StealRuntime::with_config(RuntimeConfig {
        workers: NonZeroUsize::new(2).unwrap(),
        steal_policy: StealPolicy::steal_one(),
        ..RuntimeConfig::default()
    });

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let worker = scope.spawn_stealable(|cx| {
                let (read_fd, _write_fd) = pipe().unwrap();
                let read_fd = cx.socket_from_owned_fd(read_fd).unwrap();
                let mut read = SocketIoRuntime::read_async(cx, &read_fd, vec![0_u8; 1]).unwrap();

                RuntimeReadResult::cancel(&mut read).unwrap();

                assert!(RuntimeWaitable::is_ready(&read));
                assert!(RuntimeReadResult::try_get(&mut read).is_none());
            });

            worker.join(cx);
        });
    });
}

#[test]
fn context_cancel_keeps_stealing_runtime_result_drainable_before_close() {
    let mut runtime = StealRuntime::with_config(RuntimeConfig {
        workers: NonZeroUsize::new(2).unwrap(),
        steal_policy: StealPolicy::steal_one(),
        ..RuntimeConfig::default()
    });

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let worker = scope.spawn_stealable(|cx| {
                let (read_fd, write_fd) = pipe().unwrap();
                let read_fd = cx.socket_from_owned_fd(read_fd).unwrap();
                let write_fd = cx.socket_from_owned_fd(write_fd).unwrap();
                let mut read = SocketIoRuntime::read_async(cx, &read_fd, vec![0_u8; 1]).unwrap();

                cx.cancel_read(&mut read).unwrap();
                cx.wait_stackful(&read).unwrap();
                let error = RuntimeReadResult::try_get(&mut read)
                    .expect("canceled read should drain")
                    .unwrap_err();
                assert!(
                    matches!(error, RuntimeIoError::Runtime(RuntimeIoErrorKind::Canceled))
                        || matches!(error, RuntimeIoError::Io(Errno::CANCELED))
                );

                SocketIoRuntime::close(cx, read_fd).unwrap();
                SocketIoRuntime::close(cx, write_fd).unwrap();
            });

            worker.join(cx);
        });
    });
}

#[test]
fn stealing_runtime_waits_on_runtime_neutral_socket_results() {
    let mut runtime = StealRuntime::with_config(RuntimeConfig {
        workers: NonZeroUsize::new(2).unwrap(),
        steal_policy: StealPolicy::steal_one(),
        ..RuntimeConfig::default()
    });

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let worker = scope.spawn_stealable(|cx| {
                let (read_fd, write_fd) = pipe().unwrap();
                let read_fd = cx.socket_from_owned_fd(read_fd).unwrap();
                let write_fd = cx.socket_from_owned_fd(write_fd).unwrap();

                let mut read = SocketIoRuntime::read_async(cx, &read_fd, vec![0_u8; 2]).unwrap();
                let mut write =
                    SocketIoRuntime::write_async(cx, &write_fd, b"ok".to_vec()).unwrap();
                let waitables: [&dyn RuntimeWaitable; 2] = [&read, &write];

                cx.wait_all_stackful(&waitables).unwrap();

                let written = RuntimeWriteResult::try_get(&mut write)
                    .expect("write should be ready")
                    .unwrap();
                assert_eq!(written.bytes, 2);
                assert_eq!(written.buffer, b"ok");

                let read = RuntimeReadResult::try_get(&mut read)
                    .expect("read should be ready")
                    .unwrap();
                assert_eq!(read.bytes, 2);
                assert_eq!(&read.buffer[..read.bytes], b"ok");

                SocketIoRuntime::close(cx, read_fd).unwrap();
                SocketIoRuntime::close(cx, write_fd).unwrap();
                42
            });

            assert_eq!(worker.join(cx), 42);
        });
    });
}
