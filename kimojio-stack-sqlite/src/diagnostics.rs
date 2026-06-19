use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VfsCounters {
    pub open: u64,
    pub close: u64,
    pub read: u64,
    pub write: u64,
    pub truncate: u64,
    pub sync: u64,
    pub file_size: u64,
    pub lock: u64,
    pub unlock: u64,
    pub check_reserved_lock: u64,
    pub access: u64,
    pub delete: u64,
    pub shm_map: u64,
    pub shm_lock: u64,
    pub shm_unmap: u64,
}

pub fn counters() -> VfsCounters {
    VfsCounters {
        open: OPEN.load(Ordering::Relaxed),
        close: CLOSE.load(Ordering::Relaxed),
        read: READ.load(Ordering::Relaxed),
        write: WRITE.load(Ordering::Relaxed),
        truncate: TRUNCATE.load(Ordering::Relaxed),
        sync: SYNC.load(Ordering::Relaxed),
        file_size: FILE_SIZE.load(Ordering::Relaxed),
        lock: LOCK.load(Ordering::Relaxed),
        unlock: UNLOCK.load(Ordering::Relaxed),
        check_reserved_lock: CHECK_RESERVED_LOCK.load(Ordering::Relaxed),
        access: ACCESS.load(Ordering::Relaxed),
        delete: DELETE.load(Ordering::Relaxed),
        shm_map: SHM_MAP.load(Ordering::Relaxed),
        shm_lock: SHM_LOCK.load(Ordering::Relaxed),
        shm_unmap: SHM_UNMAP.load(Ordering::Relaxed),
    }
}

pub fn reset_counters() {
    OPEN.store(0, Ordering::Relaxed);
    CLOSE.store(0, Ordering::Relaxed);
    READ.store(0, Ordering::Relaxed);
    WRITE.store(0, Ordering::Relaxed);
    TRUNCATE.store(0, Ordering::Relaxed);
    SYNC.store(0, Ordering::Relaxed);
    FILE_SIZE.store(0, Ordering::Relaxed);
    LOCK.store(0, Ordering::Relaxed);
    UNLOCK.store(0, Ordering::Relaxed);
    CHECK_RESERVED_LOCK.store(0, Ordering::Relaxed);
    ACCESS.store(0, Ordering::Relaxed);
    DELETE.store(0, Ordering::Relaxed);
    SHM_MAP.store(0, Ordering::Relaxed);
    SHM_LOCK.store(0, Ordering::Relaxed);
    SHM_UNMAP.store(0, Ordering::Relaxed);
}

pub fn set_counters_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

#[derive(Clone, Copy)]
pub(crate) enum Counter {
    Open,
    Close,
    Read,
    Write,
    Truncate,
    Sync,
    FileSize,
    Lock,
    Unlock,
    CheckReservedLock,
    Access,
    Delete,
    ShmMap,
    ShmLock,
    ShmUnmap,
}

pub(crate) fn bump(counter: Counter) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }

    match counter {
        Counter::Open => &OPEN,
        Counter::Close => &CLOSE,
        Counter::Read => &READ,
        Counter::Write => &WRITE,
        Counter::Truncate => &TRUNCATE,
        Counter::Sync => &SYNC,
        Counter::FileSize => &FILE_SIZE,
        Counter::Lock => &LOCK,
        Counter::Unlock => &UNLOCK,
        Counter::CheckReservedLock => &CHECK_RESERVED_LOCK,
        Counter::Access => &ACCESS,
        Counter::Delete => &DELETE,
        Counter::ShmMap => &SHM_MAP,
        Counter::ShmLock => &SHM_LOCK,
        Counter::ShmUnmap => &SHM_UNMAP,
    }
    .fetch_add(1, Ordering::Relaxed);
}

static ENABLED: AtomicBool = AtomicBool::new(false);
static OPEN: AtomicU64 = AtomicU64::new(0);
static CLOSE: AtomicU64 = AtomicU64::new(0);
static READ: AtomicU64 = AtomicU64::new(0);
static WRITE: AtomicU64 = AtomicU64::new(0);
static TRUNCATE: AtomicU64 = AtomicU64::new(0);
static SYNC: AtomicU64 = AtomicU64::new(0);
static FILE_SIZE: AtomicU64 = AtomicU64::new(0);
static LOCK: AtomicU64 = AtomicU64::new(0);
static UNLOCK: AtomicU64 = AtomicU64::new(0);
static CHECK_RESERVED_LOCK: AtomicU64 = AtomicU64::new(0);
static ACCESS: AtomicU64 = AtomicU64::new(0);
static DELETE: AtomicU64 = AtomicU64::new(0);
static SHM_MAP: AtomicU64 = AtomicU64::new(0);
static SHM_LOCK: AtomicU64 = AtomicU64::new(0);
static SHM_UNMAP: AtomicU64 = AtomicU64::new(0);
