use std::{
    ffi::{c_char, c_int, c_void},
    fs::File,
    io::Write,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

const DEPTH: usize = 24;
const CAPACITY: usize = 2048;
pub const UNTRACKED: usize = usize::MAX;
static ENABLED: AtomicBool = AtomicBool::new(false);
static BASE: AtomicUsize = AtomicUsize::new(0);
static OUTPUT: Mutex<Option<File>> = Mutex::new(None);

#[repr(C)]
struct DlInfo {
    name: *const c_char,
    base: *mut c_void,
    symbol: *const c_char,
    address: *mut c_void,
}

#[link(name = "dl")]
unsafe extern "C" {
    fn backtrace(buffer: *mut *mut c_void, size: c_int) -> c_int;
    fn dladdr(address: *const c_void, info: *mut DlInfo) -> c_int;
}

#[derive(Clone, Copy, serde::Serialize)]
struct Site {
    stack: [usize; DEPTH],
    allocations: u64,
    reallocations: u64,
    deallocations: u64,
    live_allocations: usize,
    live_bytes: usize,
    peak_bytes: usize,
}

impl Site {
    const ZERO: Self = Self {
        stack: [0; DEPTH],
        allocations: 0,
        reallocations: 0,
        deallocations: 0,
        live_allocations: 0,
        live_bytes: 0,
        peak_bytes: 0,
    };
}

struct Sites {
    entries: [Site; CAPACITY],
    len: usize,
}
static SITES: Mutex<Sites> = Mutex::new(Sites {
    entries: [Site::ZERO; CAPACITY],
    len: 0,
});

pub fn enable(path: &str) {
    let file = File::create_new(path).expect("new retention output path");
    let mut warm = [std::ptr::null_mut(); DEPTH];
    unsafe { backtrace(warm.as_mut_ptr(), DEPTH as c_int) };
    let mut info = DlInfo {
        name: std::ptr::null(),
        base: std::ptr::null_mut(),
        symbol: std::ptr::null(),
        address: std::ptr::null_mut(),
    };
    assert_ne!(unsafe { dladdr(enable as *const c_void, &mut info) }, 0);
    BASE.store(info.base as usize, Ordering::Relaxed);
    *OUTPUT.lock().unwrap() = Some(file);
    ENABLED.store(true, Ordering::Relaxed);
}

#[inline(never)]
pub fn allocated(bytes: usize) -> usize {
    if !ENABLED.load(Ordering::Relaxed) {
        return UNTRACKED;
    }
    let mut addresses = [std::ptr::null_mut(); DEPTH];
    unsafe { backtrace(addresses.as_mut_ptr(), DEPTH as c_int) };
    let stack = addresses.map(|p| p as usize);
    let mut sites = SITES.lock().unwrap();
    let len = sites.len;
    let index = match sites.entries[..len]
        .iter()
        .position(|site| site.stack == stack)
    {
        Some(index) => index,
        None if len < CAPACITY => {
            sites.entries[len].stack = stack;
            sites.len += 1;
            len
        }
        // Never silently merge sites or allocate tracking storage in the allocator.
        None => std::process::abort(),
    };
    let site = &mut sites.entries[index];
    site.allocations += 1;
    site.live_allocations += 1;
    site.live_bytes += bytes;
    site.peak_bytes = site.peak_bytes.max(site.live_bytes);
    index
}

pub fn resized(index: usize, old: usize, new: usize) {
    if index == UNTRACKED {
        return;
    }
    let mut sites = SITES.lock().unwrap();
    let site = &mut sites.entries[index];
    site.reallocations += 1;
    site.live_bytes = site.live_bytes - old + new;
    site.peak_bytes = site.peak_bytes.max(site.live_bytes);
}

pub fn freed(index: usize, bytes: usize) {
    if index == UNTRACKED {
        return;
    }
    let mut sites = SITES.lock().unwrap();
    let site = &mut sites.entries[index];
    site.deallocations += 1;
    site.live_allocations -= 1;
    site.live_bytes -= bytes;
}

pub fn snapshot(label: &str, cohort: usize, counts: &[super::Counts; 3]) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let mut output = OUTPUT.lock().unwrap();
    let output = output.as_mut().unwrap();
    let sites = SITES.lock().unwrap();
    #[derive(serde::Serialize)]
    struct Snapshot<'a> {
        label: &'a str,
        cohort: usize,
        executable_base: usize,
        counts: &'a [super::Counts; 3],
        sites: &'a [Site],
    }
    // Serialization writes directly to the preopened file without a heap snapshot.
    let result = serde_json::to_writer(
        &mut *output,
        &Snapshot {
            label,
            cohort,
            executable_base: BASE.load(Ordering::Relaxed),
            counts,
            sites: &sites.entries[..sites.len],
        },
    )
    .and_then(|_| output.write_all(b"\n").map_err(serde_json::Error::io));
    if result.is_err() {
        std::process::abort();
    }
}
