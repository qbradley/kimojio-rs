use std::collections::{HashMap, HashSet};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::raw::{c_int, c_void as raw_c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use kimojio_stack::{Mode, OFlags, OwnedFd as StackOwnedFd, RuntimeContext, StatxFlags};
use libsqlite3_sys as ffi;

use crate::{
    diagnostics::{self, Counter},
    path,
    shm::ShmState,
    vfs,
};

static NEXT_FILE_ID: AtomicUsize = AtomicUsize::new(1);

// SQLite's rollback-journal locking protocol uses byte-range locks near
// PENDING_BYTE. The process-local table handles same-process connections because
// POSIX advisory locks are process-scoped; these OS locks make conflicts visible
// to other processes using the same database file.
const PENDING_BYTE: libc::off_t = 0x4000_0000;
const RESERVED_BYTE: libc::off_t = PENDING_BYTE + 1;
const SHARED_FIRST: libc::off_t = PENDING_BYTE + 2;
const SHARED_SIZE: libc::off_t = 510;

#[repr(C)]
pub struct KimojioFile {
    base: ffi::sqlite3_file,
    fd: Option<OwnedFd>,
    path: Option<PathBuf>,
    delete_on_close: bool,
    readonly: bool,
    size_hint: Option<u64>,
    lock_level: c_int,
    id: usize,
    shm: ShmState,
}

fn display_path(path: Option<&Path>) -> String {
    path.map(|path| path.display().to_string())
        .unwrap_or_else(|| "<temporary>".to_owned())
}

fn cached_file_size(path: Option<&Path>) -> Option<u64> {
    let path = path?;
    file_sizes()
        .lock()
        .expect("SQLite file size cache poisoned")
        .get(path)
        .copied()
}

fn remember_write_size(file: &mut KimojioFile, offset: u64, len: u64) {
    let Some(end) = offset.checked_add(len) else {
        file.size_hint = None;
        return;
    };
    if file.size_hint.is_none_or(|size| end > size) {
        file.size_hint = Some(end);
        if let Some(path) = &file.path {
            remember_file_size(path, end);
        }
    }
}

fn remember_file_size(path: &Path, size: u64) {
    file_sizes()
        .lock()
        .expect("SQLite file size cache poisoned")
        .insert(path.to_path_buf(), size);
}

fn forget_file_size(path: &Path) {
    file_sizes()
        .lock()
        .expect("SQLite file size cache poisoned")
        .remove(path);
}

fn file_sizes() -> &'static Mutex<HashMap<PathBuf, u64>> {
    static SIZES: OnceLock<Mutex<HashMap<PathBuf, u64>>> = OnceLock::new();
    SIZES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub static IO_METHODS: ffi::sqlite3_io_methods = ffi::sqlite3_io_methods {
    iVersion: 3,
    xClose: Some(x_close),
    xRead: Some(x_read),
    xWrite: Some(x_write),
    xTruncate: Some(x_truncate),
    xSync: Some(x_sync),
    xFileSize: Some(x_file_size),
    xLock: Some(x_lock),
    xUnlock: Some(x_unlock),
    xCheckReservedLock: Some(x_check_reserved_lock),
    xFileControl: Some(x_file_control),
    xSectorSize: Some(x_sector_size),
    xDeviceCharacteristics: Some(x_device_characteristics),
    xShmMap: Some(x_shm_map),
    xShmLock: Some(x_shm_lock),
    xShmBarrier: Some(x_shm_barrier),
    xShmUnmap: Some(x_shm_unmap),
    xFetch: Some(x_fetch),
    xUnfetch: Some(x_unfetch),
};

pub unsafe fn init_file(
    sqlite_file: *mut ffi::sqlite3_file,
    fd: StackOwnedFd,
    path: Option<PathBuf>,
    delete_on_close: bool,
    readonly: bool,
    nofollow: bool,
) {
    let id = NEXT_FILE_ID.fetch_add(1, Ordering::Relaxed);
    let size_hint = cached_file_size(path.as_deref());
    let file = KimojioFile {
        base: ffi::sqlite3_file {
            pMethods: &IO_METHODS,
        },
        fd: Some(fd),
        path,
        delete_on_close,
        readonly,
        size_hint,
        lock_level: ffi::SQLITE_LOCK_NONE,
        id,
        shm: ShmState::new(id, nofollow),
    };
    unsafe {
        sqlite_file.cast::<KimojioFile>().write(file);
    }
}

pub fn open_runtime_file(path: &Path, flags: c_int) -> Result<(StackOwnedFd, bool, bool), c_int> {
    let readonly =
        flags & ffi::SQLITE_OPEN_READONLY != 0 && flags & ffi::SQLITE_OPEN_READWRITE == 0;
    let mut open_flags = OFlags::CLOEXEC;
    if readonly {
        open_flags |= OFlags::RDONLY;
    } else {
        open_flags |= OFlags::RDWR;
    }
    if flags & ffi::SQLITE_OPEN_CREATE != 0 {
        open_flags |= OFlags::CREATE;
    }
    if flags & ffi::SQLITE_OPEN_EXCLUSIVE != 0 {
        open_flags |= OFlags::EXCL;
    }
    if flags & ffi::SQLITE_OPEN_NOFOLLOW != 0 {
        open_flags |= OFlags::NOFOLLOW;
    }

    let mode = Mode::RUSR | Mode::WUSR;
    let delete_on_close = flags & ffi::SQLITE_OPEN_DELETEONCLOSE != 0;
    let result = RuntimeContext::with_current(|cx| cx.open(path, open_flags, mode));
    match result {
        Some(Ok(fd)) => Ok((fd, readonly, delete_on_close)),
        Some(Err(err)) => {
            vfs::set_last_error(format!("xOpen failed for {}: {err}", path.display()));
            Err(ffi::SQLITE_CANTOPEN)
        }
        None => {
            vfs::set_last_error(format!(
                "xOpen failed for {}: no current kimojio-stack runtime context",
                path.display()
            ));
            Err(ffi::SQLITE_IOERR)
        }
    }
}

unsafe fn file_mut(sqlite_file: *mut ffi::sqlite3_file) -> &'static mut KimojioFile {
    unsafe { &mut *sqlite_file.cast::<KimojioFile>() }
}

unsafe extern "C" fn x_close(sqlite_file: *mut ffi::sqlite3_file) -> c_int {
    diagnostics::bump(Counter::Close);
    if sqlite_file.is_null() {
        return ffi::SQLITE_OK;
    }
    let file = unsafe { file_mut(sqlite_file) };
    let mut rc = unsafe { file.shm.unmap(false) };
    release_file_locks(file);

    if let Some(fd) = file.fd.take() {
        let close = RuntimeContext::with_current(|cx| cx.close(fd));
        if close.is_none_or(|result| result.is_err()) {
            rc = ffi::SQLITE_IOERR_CLOSE;
        }
    }
    if file.delete_on_close
        && let Some(path) = &file.path
    {
        let delete = RuntimeContext::with_current(|cx| cx.unlink(path));
        if delete.is_none_or(|result| result.is_err()) && rc == ffi::SQLITE_OK {
            rc = ffi::SQLITE_IOERR_DELETE;
        }
    }

    unsafe {
        std::ptr::drop_in_place(file);
    }
    rc
}

unsafe extern "C" fn x_read(
    sqlite_file: *mut ffi::sqlite3_file,
    buf: *mut raw_c_void,
    amount: c_int,
    offset: ffi::sqlite3_int64,
) -> c_int {
    diagnostics::bump(Counter::Read);
    if buf.is_null() || amount < 0 || offset < 0 {
        return ffi::SQLITE_IOERR_READ;
    }
    if amount == 0 {
        return ffi::SQLITE_OK;
    }

    let file = unsafe { file_mut(sqlite_file) };
    let Some(fd) = file.fd.as_ref() else {
        return ffi::SQLITE_IOERR_READ;
    };
    let out = unsafe { std::slice::from_raw_parts_mut(buf.cast::<u8>(), amount as usize) };
    let result: Option<Result<bool, kimojio_stack::Errno>> = RuntimeContext::with_current(|cx| {
        let mut filled = 0;
        while filled < out.len() {
            let read = cx.pread(fd, &mut out[filled..], offset as u64 + filled as u64)?;
            if read == 0 {
                out[filled..].fill(0);
                return Ok(false);
            }
            filled += read;
        }
        Ok(true)
    });
    match result {
        Some(Ok(true)) => ffi::SQLITE_OK,
        Some(Ok(false)) => ffi::SQLITE_IOERR_SHORT_READ,
        Some(Err(err)) => {
            vfs::set_last_error(format!(
                "xRead failed for {} at offset {offset}: {err}",
                display_path(file.path.as_deref())
            ));
            ffi::SQLITE_IOERR_READ
        }
        None => {
            vfs::set_last_error(format!(
                "xRead failed for {} at offset {offset}: no current kimojio-stack runtime context",
                display_path(file.path.as_deref())
            ));
            ffi::SQLITE_IOERR_READ
        }
    }
}

unsafe extern "C" fn x_write(
    sqlite_file: *mut ffi::sqlite3_file,
    buf: *const raw_c_void,
    amount: c_int,
    offset: ffi::sqlite3_int64,
) -> c_int {
    diagnostics::bump(Counter::Write);
    if buf.is_null() || amount < 0 || offset < 0 {
        return ffi::SQLITE_IOERR_WRITE;
    }
    if amount == 0 {
        return ffi::SQLITE_OK;
    }
    let file = unsafe { file_mut(sqlite_file) };
    if file.readonly {
        return ffi::SQLITE_IOERR_WRITE;
    }
    let Some(fd) = file.fd.as_ref() else {
        return ffi::SQLITE_IOERR_WRITE;
    };
    let input = unsafe { std::slice::from_raw_parts(buf.cast::<u8>(), amount as usize) };
    let result: Option<Result<bool, kimojio_stack::Errno>> = RuntimeContext::with_current(|cx| {
        let mut written = 0;
        while written < input.len() {
            let n = cx.pwrite(fd, &input[written..], offset as u64 + written as u64)?;
            if n == 0 {
                return Ok(false);
            }
            written += n;
        }
        Ok(true)
    });
    match result {
        Some(Ok(true)) => {
            remember_write_size(file, offset as u64, input.len() as u64);
            ffi::SQLITE_OK
        }
        Some(Ok(false)) => {
            vfs::set_last_error(format!(
                "xWrite failed for {} at offset {offset}: zero-byte progress",
                display_path(file.path.as_deref())
            ));
            ffi::SQLITE_IOERR_WRITE
        }
        Some(Err(err)) => {
            vfs::set_last_error(format!(
                "xWrite failed for {} at offset {offset}: {err}",
                display_path(file.path.as_deref())
            ));
            ffi::SQLITE_IOERR_WRITE
        }
        None => {
            vfs::set_last_error(format!(
                "xWrite failed for {} at offset {offset}: no current kimojio-stack runtime context",
                display_path(file.path.as_deref())
            ));
            ffi::SQLITE_IOERR_WRITE
        }
    }
}

unsafe extern "C" fn x_truncate(
    sqlite_file: *mut ffi::sqlite3_file,
    size: ffi::sqlite3_int64,
) -> c_int {
    diagnostics::bump(Counter::Truncate);
    if size < 0 {
        return ffi::SQLITE_IOERR_TRUNCATE;
    }
    let file = unsafe { file_mut(sqlite_file) };
    let Some(fd) = file.fd.as_ref() else {
        return ffi::SQLITE_IOERR_TRUNCATE;
    };
    let result = RuntimeContext::with_current(|cx| cx.ftruncate(fd, size as u64));
    match result {
        Some(Ok(())) => {
            file.size_hint = Some(size as u64);
            if let Some(path) = &file.path {
                remember_file_size(path, size as u64);
            }
            ffi::SQLITE_OK
        }
        Some(Err(err)) => {
            vfs::set_last_error(format!(
                "xTruncate failed for {} to size {size}: {err}",
                display_path(file.path.as_deref())
            ));
            ffi::SQLITE_IOERR_TRUNCATE
        }
        None => {
            vfs::set_last_error(format!(
                "xTruncate failed for {} to size {size}: no current kimojio-stack runtime context",
                display_path(file.path.as_deref())
            ));
            ffi::SQLITE_IOERR_TRUNCATE
        }
    }
}

unsafe extern "C" fn x_sync(sqlite_file: *mut ffi::sqlite3_file, _flags: c_int) -> c_int {
    diagnostics::bump(Counter::Sync);
    let file = unsafe { file_mut(sqlite_file) };
    let Some(fd) = file.fd.as_ref() else {
        return ffi::SQLITE_IOERR_FSYNC;
    };
    let result = RuntimeContext::with_current(|cx| cx.fsync(fd));
    match result {
        Some(Ok(())) => ffi::SQLITE_OK,
        Some(Err(err)) => {
            vfs::set_last_error(format!(
                "xSync failed for {}: {err}",
                display_path(file.path.as_deref())
            ));
            ffi::SQLITE_IOERR_FSYNC
        }
        None => {
            vfs::set_last_error(format!(
                "xSync failed for {}: no current kimojio-stack runtime context",
                display_path(file.path.as_deref())
            ));
            ffi::SQLITE_IOERR_FSYNC
        }
    }
}

unsafe extern "C" fn x_file_size(
    sqlite_file: *mut ffi::sqlite3_file,
    size: *mut ffi::sqlite3_int64,
) -> c_int {
    diagnostics::bump(Counter::FileSize);
    if size.is_null() {
        return ffi::SQLITE_IOERR_FSTAT;
    }
    let file = unsafe { file_mut(sqlite_file) };
    if let Some(size_hint) = file.size_hint {
        unsafe {
            *size = size_hint as ffi::sqlite3_int64;
        }
        return ffi::SQLITE_OK;
    }
    let Some(fd) = file.fd.as_ref() else {
        return ffi::SQLITE_IOERR_FSTAT;
    };
    let result = RuntimeContext::with_current(|cx| cx.fstat(fd, StatxFlags::BASIC_STATS));
    match result {
        Some(Ok(stat)) => {
            file.size_hint = Some(stat.stx_size);
            if let Some(path) = &file.path {
                remember_file_size(path, stat.stx_size);
            }
            unsafe {
                *size = stat.stx_size as ffi::sqlite3_int64;
            }
            ffi::SQLITE_OK
        }
        Some(Err(err)) => {
            vfs::set_last_error(format!(
                "xFileSize failed for {}: {err}",
                display_path(file.path.as_deref())
            ));
            ffi::SQLITE_IOERR_FSTAT
        }
        None => {
            vfs::set_last_error(format!(
                "xFileSize failed for {}: no current kimojio-stack runtime context",
                display_path(file.path.as_deref())
            ));
            ffi::SQLITE_IOERR_FSTAT
        }
    }
}

unsafe extern "C" fn x_lock(sqlite_file: *mut ffi::sqlite3_file, lock: c_int) -> c_int {
    diagnostics::bump(Counter::Lock);
    let file = unsafe { file_mut(sqlite_file) };
    lock_file(file, lock)
}

unsafe extern "C" fn x_unlock(sqlite_file: *mut ffi::sqlite3_file, lock: c_int) -> c_int {
    diagnostics::bump(Counter::Unlock);
    let file = unsafe { file_mut(sqlite_file) };
    unlock_file(file, lock)
}

unsafe extern "C" fn x_check_reserved_lock(
    sqlite_file: *mut ffi::sqlite3_file,
    out: *mut c_int,
) -> c_int {
    diagnostics::bump(Counter::CheckReservedLock);
    if out.is_null() {
        return ffi::SQLITE_IOERR_CHECKRESERVEDLOCK;
    }
    let file = unsafe { file_mut(sqlite_file) };
    let reserved = file
        .path
        .as_ref()
        .and_then(|path| lock_table().lock().ok()?.get(path).cloned())
        .is_some_and(|state| state.has_reserved_or_exclusive());
    unsafe {
        *out = c_int::from(reserved);
    }
    ffi::SQLITE_OK
}

unsafe extern "C" fn x_file_control(
    sqlite_file: *mut ffi::sqlite3_file,
    op: c_int,
    arg: *mut raw_c_void,
) -> c_int {
    let file = unsafe { file_mut(sqlite_file) };
    match op {
        ffi::SQLITE_FCNTL_LOCKSTATE => {
            if !arg.is_null() {
                unsafe {
                    *arg.cast::<c_int>() = file.lock_level;
                }
            }
            ffi::SQLITE_OK
        }
        ffi::SQLITE_FCNTL_SIZE_HINT
        | ffi::SQLITE_FCNTL_PERSIST_WAL
        | ffi::SQLITE_FCNTL_SYNC
        | ffi::SQLITE_FCNTL_COMMIT_PHASETWO => ffi::SQLITE_OK,
        ffi::SQLITE_FCNTL_HAS_MOVED => {
            if !arg.is_null() {
                unsafe {
                    *arg.cast::<c_int>() = 0;
                }
            }
            ffi::SQLITE_OK
        }
        _ => ffi::SQLITE_NOTFOUND,
    }
}

unsafe extern "C" fn x_sector_size(_sqlite_file: *mut ffi::sqlite3_file) -> c_int {
    4096
}

unsafe extern "C" fn x_device_characteristics(_sqlite_file: *mut ffi::sqlite3_file) -> c_int {
    ffi::SQLITE_IOCAP_POWERSAFE_OVERWRITE
}

unsafe extern "C" fn x_shm_map(
    sqlite_file: *mut ffi::sqlite3_file,
    i_pg: c_int,
    page_size: c_int,
    extend: c_int,
    out: *mut *mut raw_c_void,
) -> c_int {
    diagnostics::bump(Counter::ShmMap);
    let file = unsafe { file_mut(sqlite_file) };
    unsafe {
        file.shm
            .map(file.path.as_deref(), i_pg, page_size, extend, out)
    }
}

unsafe extern "C" fn x_shm_lock(
    sqlite_file: *mut ffi::sqlite3_file,
    offset: c_int,
    n: c_int,
    flags: c_int,
) -> c_int {
    diagnostics::bump(Counter::ShmLock);
    let file = unsafe { file_mut(sqlite_file) };
    file.shm.lock(offset, n, flags)
}

unsafe extern "C" fn x_shm_barrier(sqlite_file: *mut ffi::sqlite3_file) {
    let file = unsafe { file_mut(sqlite_file) };
    file.shm.barrier();
}

unsafe extern "C" fn x_shm_unmap(sqlite_file: *mut ffi::sqlite3_file, delete_flag: c_int) -> c_int {
    diagnostics::bump(Counter::ShmUnmap);
    let file = unsafe { file_mut(sqlite_file) };
    unsafe { file.shm.unmap(delete_flag != 0) }
}

unsafe extern "C" fn x_fetch(
    _sqlite_file: *mut ffi::sqlite3_file,
    _offset: ffi::sqlite3_int64,
    _amount: c_int,
    out: *mut *mut raw_c_void,
) -> c_int {
    if !out.is_null() {
        unsafe {
            *out = std::ptr::null_mut();
        }
    }
    ffi::SQLITE_OK
}

unsafe extern "C" fn x_unfetch(
    _sqlite_file: *mut ffi::sqlite3_file,
    _offset: ffi::sqlite3_int64,
    _ptr: *mut raw_c_void,
) -> c_int {
    ffi::SQLITE_OK
}

fn lock_file(file: &mut KimojioFile, lock: c_int) -> c_int {
    if lock <= file.lock_level {
        return ffi::SQLITE_OK;
    }
    let Some(path) = &file.path else {
        file.lock_level = lock;
        return ffi::SQLITE_OK;
    };

    let mut table = lock_table()
        .lock()
        .expect("SQLite file lock table poisoned");
    let state = table.entry(path.clone()).or_default();
    if !state.can_lock(file.id, lock) {
        return ffi::SQLITE_BUSY;
    }
    if apply_os_lock(file, lock) != ffi::SQLITE_OK {
        return ffi::SQLITE_BUSY;
    }
    state.lock(file.id, lock);
    file.lock_level = lock;
    ffi::SQLITE_OK
}

fn unlock_file(file: &mut KimojioFile, lock: c_int) -> c_int {
    let Some(path) = &file.path else {
        file.lock_level = lock;
        return ffi::SQLITE_OK;
    };
    let mut table = lock_table()
        .lock()
        .expect("SQLite file lock table poisoned");
    let os_rc = apply_os_unlock(file, lock);
    if os_rc != ffi::SQLITE_OK {
        return os_rc;
    }
    if let Some(state) = table.get_mut(path) {
        state.unlock(file.id, lock);
    }
    file.lock_level = lock;
    ffi::SQLITE_OK
}

fn release_file_locks(file: &mut KimojioFile) {
    let _ = apply_os_unlock(file, ffi::SQLITE_LOCK_NONE);
    let Some(path) = &file.path else {
        return;
    };
    let mut table = lock_table()
        .lock()
        .expect("SQLite file lock table poisoned");
    if let Some(state) = table.get_mut(path) {
        state.release(file.id);
    }
}

fn apply_os_lock(file: &KimojioFile, lock: c_int) -> c_int {
    let Some(fd) = &file.fd else {
        return ffi::SQLITE_IOERR_LOCK;
    };
    let ok = match lock {
        ffi::SQLITE_LOCK_SHARED => {
            os_file_lock(fd, libc::F_RDLCK as i16, SHARED_FIRST, SHARED_SIZE)
        }
        ffi::SQLITE_LOCK_RESERVED => os_file_lock(fd, libc::F_WRLCK as i16, RESERVED_BYTE, 1),
        ffi::SQLITE_LOCK_PENDING => os_file_lock(fd, libc::F_WRLCK as i16, PENDING_BYTE, 1),
        ffi::SQLITE_LOCK_EXCLUSIVE => {
            os_file_lock(fd, libc::F_WRLCK as i16, PENDING_BYTE, 1)
                && os_file_lock(fd, libc::F_WRLCK as i16, RESERVED_BYTE, 1)
                && os_file_lock(fd, libc::F_WRLCK as i16, SHARED_FIRST, SHARED_SIZE)
        }
        _ => true,
    };
    if ok { ffi::SQLITE_OK } else { ffi::SQLITE_BUSY }
}

fn apply_os_unlock(file: &KimojioFile, lock: c_int) -> c_int {
    let Some(fd) = &file.fd else {
        return ffi::SQLITE_OK;
    };
    let ok = match lock {
        ffi::SQLITE_LOCK_NONE => {
            os_file_lock(fd, libc::F_UNLCK as i16, SHARED_FIRST, SHARED_SIZE)
                && os_file_lock(fd, libc::F_UNLCK as i16, RESERVED_BYTE, 1)
                && os_file_lock(fd, libc::F_UNLCK as i16, PENDING_BYTE, 1)
        }
        ffi::SQLITE_LOCK_SHARED => {
            os_file_lock(fd, libc::F_RDLCK as i16, SHARED_FIRST, SHARED_SIZE)
                && os_file_lock(fd, libc::F_UNLCK as i16, RESERVED_BYTE, 1)
                && os_file_lock(fd, libc::F_UNLCK as i16, PENDING_BYTE, 1)
        }
        ffi::SQLITE_LOCK_RESERVED => {
            os_file_lock(fd, libc::F_RDLCK as i16, SHARED_FIRST, SHARED_SIZE)
                && os_file_lock(fd, libc::F_WRLCK as i16, RESERVED_BYTE, 1)
                && os_file_lock(fd, libc::F_UNLCK as i16, PENDING_BYTE, 1)
        }
        _ => true,
    };
    if ok {
        ffi::SQLITE_OK
    } else {
        ffi::SQLITE_IOERR_UNLOCK
    }
}

fn os_file_lock(fd: &OwnedFd, lock_type: i16, start: libc::off_t, len: libc::off_t) -> bool {
    let mut lock = libc::flock {
        l_type: lock_type,
        l_whence: libc::SEEK_SET as i16,
        l_start: start,
        l_len: len,
        l_pid: 0,
    };
    let result = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETLK, &mut lock) };
    result == 0
}

#[derive(Clone, Default)]
struct FileLockState {
    shared: HashSet<usize>,
    reserved: Option<usize>,
    pending: Option<usize>,
    exclusive: Option<usize>,
}

impl FileLockState {
    fn can_lock(&self, owner: usize, lock: c_int) -> bool {
        match lock {
            ffi::SQLITE_LOCK_SHARED => {
                self.exclusive.is_none_or(|exclusive| exclusive == owner)
                    && self.pending.is_none_or(|pending| pending == owner)
            }
            ffi::SQLITE_LOCK_RESERVED => {
                self.exclusive.is_none_or(|exclusive| exclusive == owner)
                    && self.reserved.is_none_or(|reserved| reserved == owner)
            }
            ffi::SQLITE_LOCK_PENDING => {
                self.exclusive.is_none_or(|exclusive| exclusive == owner)
                    && self.pending.is_none_or(|pending| pending == owner)
            }
            ffi::SQLITE_LOCK_EXCLUSIVE => {
                self.exclusive.is_none_or(|exclusive| exclusive == owner)
                    && self.reserved.is_none_or(|reserved| reserved == owner)
                    && self.pending.is_none_or(|pending| pending == owner)
                    && (self.shared.is_empty()
                        || (self.shared.len() == 1 && self.shared.contains(&owner)))
            }
            _ => true,
        }
    }

    fn lock(&mut self, owner: usize, lock: c_int) {
        match lock {
            ffi::SQLITE_LOCK_SHARED => {
                self.shared.insert(owner);
            }
            ffi::SQLITE_LOCK_RESERVED => {
                self.shared.insert(owner);
                self.reserved = Some(owner);
            }
            ffi::SQLITE_LOCK_PENDING => {
                self.shared.insert(owner);
                self.pending = Some(owner);
            }
            ffi::SQLITE_LOCK_EXCLUSIVE => {
                self.shared.insert(owner);
                self.pending = Some(owner);
                self.exclusive = Some(owner);
            }
            _ => {}
        }
    }

    fn unlock(&mut self, owner: usize, lock: c_int) {
        if lock < ffi::SQLITE_LOCK_EXCLUSIVE && self.exclusive == Some(owner) {
            self.exclusive = None;
        }
        if lock < ffi::SQLITE_LOCK_PENDING && self.pending == Some(owner) {
            self.pending = None;
        }
        if lock < ffi::SQLITE_LOCK_RESERVED && self.reserved == Some(owner) {
            self.reserved = None;
        }
        if lock == ffi::SQLITE_LOCK_NONE {
            self.shared.remove(&owner);
        }
    }

    fn release(&mut self, owner: usize) {
        if self.exclusive == Some(owner) {
            self.exclusive = None;
        }
        if self.pending == Some(owner) {
            self.pending = None;
        }
        if self.reserved == Some(owner) {
            self.reserved = None;
        }
        self.shared.remove(&owner);
    }

    fn has_reserved_or_exclusive(&self) -> bool {
        self.reserved.is_some() || self.pending.is_some() || self.exclusive.is_some()
    }
}

fn lock_table() -> &'static Mutex<HashMap<PathBuf, FileLockState>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, FileLockState>>> = OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn delete_path(path: &Path) -> c_int {
    let result = RuntimeContext::with_current(|cx| cx.unlink(path));
    match result {
        Some(Ok(())) => {
            forget_file_size(path);
            ffi::SQLITE_OK
        }
        Some(Err(err)) if err == kimojio_stack::Errno::NOENT => ffi::SQLITE_IOERR_DELETE_NOENT,
        Some(Err(_)) | None => ffi::SQLITE_IOERR_DELETE,
    }
}

pub fn access_path(path: &Path, flags: c_int) -> Result<bool, c_int> {
    match flags {
        ffi::SQLITE_ACCESS_EXISTS | ffi::SQLITE_ACCESS_READ => {
            let result = RuntimeContext::with_current(|cx| {
                cx.statx(
                    path,
                    kimojio_stack::AtFlags::empty(),
                    StatxFlags::BASIC_STATS,
                )
            });
            match result {
                Some(Ok(_)) => Ok(true),
                Some(Err(err)) if err == kimojio_stack::Errno::NOENT => Ok(false),
                Some(Err(err)) => {
                    vfs::set_last_error(format!("xAccess failed for {}: {err}", path.display()));
                    Err(ffi::SQLITE_IOERR_ACCESS)
                }
                None => {
                    vfs::set_last_error(format!(
                        "xAccess failed for {}: no current kimojio-stack runtime context",
                        path.display()
                    ));
                    Err(ffi::SQLITE_IOERR_ACCESS)
                }
            }
        }
        ffi::SQLITE_ACCESS_READWRITE => {
            let result = RuntimeContext::with_current(|cx| {
                cx.open(path, OFlags::RDWR | OFlags::CLOEXEC, Mode::empty())
                    .and_then(|fd| cx.close(fd))
            });
            match result {
                Some(Ok(())) => Ok(true),
                Some(Err(err)) if err == kimojio_stack::Errno::NOENT => Ok(false),
                Some(Err(_)) => Ok(false),
                None => {
                    vfs::set_last_error(format!(
                        "xAccess read-write probe failed for {}: no current kimojio-stack runtime context",
                        path.display()
                    ));
                    Err(ffi::SQLITE_IOERR_ACCESS)
                }
            }
        }
        _ => Ok(false),
    }
}

pub fn sync_parent_dir(path: &Path) -> c_int {
    let Some(parent) = path::parent_dir(path) else {
        return ffi::SQLITE_OK;
    };
    let result = RuntimeContext::with_current(|cx| {
        cx.open(
            &parent,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .and_then(|fd| {
            let sync = cx.fsync(&fd);
            let close = cx.close(fd);
            sync.and(close)
        })
    });
    if result.is_some_and(|result| result.is_ok()) {
        ffi::SQLITE_OK
    } else {
        ffi::SQLITE_IOERR_DIR_FSYNC
    }
}
