use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use kimojio_stack_steal::{Runtime, RuntimeConfig, SchedulerConfig, SchedulerMode, StealPolicy};

#[global_allocator]
static TEST_ALLOCATOR: CountingAllocator = CountingAllocator;

thread_local! {
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
}

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static REALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static MEASUREMENT_LOCK: Mutex<()> = Mutex::new(());

struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ACTIVE.with(|active| {
            if active.get() {
                ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            }
        });
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ACTIVE.with(|active| {
            if active.get() {
                REALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            }
        });
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Counts {
    allocations: usize,
    reallocations: usize,
}

impl Counts {
    fn operations(self) -> usize {
        self.allocations + self.reallocations
    }
}

fn measure(f: impl FnOnce()) -> Counts {
    let _guard = MEASUREMENT_LOCK.lock().unwrap();
    ACTIVE.with(|active| assert!(!active.get(), "nested allocation measurement"));
    ALLOCATIONS.store(0, Ordering::Relaxed);
    REALLOCATIONS.store(0, Ordering::Relaxed);
    ACTIVE.with(|active| active.set(true));
    f();
    ACTIVE.with(|active| active.set(false));
    Counts {
        allocations: ALLOCATIONS.load(Ordering::Relaxed),
        reallocations: REALLOCATIONS.load(Ordering::Relaxed),
    }
}

fn spawn_batch(runtime: &mut Runtime) {
    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let handles = (0..32)
                .map(|_| scope.spawn_stealable(|_| 1_usize))
                .collect::<Vec<_>>();
            assert_eq!(
                handles
                    .into_iter()
                    .map(|handle| handle.join(cx))
                    .sum::<usize>(),
                32
            );
        });
    });
}

#[test]
fn sfq_allocation_warmed_spawn_batch_stays_near_current_scheduler() {
    let current_config = RuntimeConfig {
        workers: NonZeroUsize::new(4).unwrap(),
        steal_policy: StealPolicy::steal_one(),
        ..RuntimeConfig::default()
    };
    let sfq_config = RuntimeConfig {
        workers: NonZeroUsize::new(4).unwrap(),
        steal_policy: StealPolicy::Disabled,
        scheduler: SchedulerConfig {
            mode: SchedulerMode::StochasticFair,
            ready_partitions: NonZeroUsize::new(16).unwrap(),
            ..SchedulerConfig::default()
        },
        ..RuntimeConfig::default()
    };

    let mut current = Runtime::with_config(current_config);
    let mut sfq = Runtime::with_config(sfq_config);
    spawn_batch(&mut current);
    spawn_batch(&mut sfq);

    let current_counts = measure(|| spawn_batch(&mut current));
    let sfq_counts = measure(|| spawn_batch(&mut sfq));

    assert!(
        sfq_counts.operations() <= current_counts.operations() + 16,
        "sfq allocations {:?} exceeded current scheduler {:?}",
        sfq_counts,
        current_counts
    );
}
