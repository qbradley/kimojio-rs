use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kimojio_stack::RuntimeContext;
use libsqlite3_sys as ffi;

use crate::{
    diagnostics::{self, Counter},
    file, path,
};

pub const KIMOJIO_STACK_VFS_NAME: &str = "kimojio-stack";
const KIMOJIO_STACK_VFS_NAME_C: &CStr = c"kimojio-stack";

static REGISTER_RESULT: OnceLock<c_int> = OnceLock::new();

thread_local! {
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct RegisterError {
    code: c_int,
}

impl RegisterError {
    pub fn code(self) -> c_int {
        self.code
    }
}

impl std::fmt::Display for RegisterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sqlite3_vfs_register failed with code {}", self.code)
    }
}

impl std::error::Error for RegisterError {}

pub fn register() -> Result<(), RegisterError> {
    let code = *REGISTER_RESULT.get_or_init(|| unsafe {
        let vfs = Box::leak(Box::new(sqlite_vfs()));
        ffi::sqlite3_vfs_register(vfs, 0)
    });
    if code == ffi::SQLITE_OK {
        Ok(())
    } else {
        Err(RegisterError { code })
    }
}

pub(crate) fn set_last_error(message: impl Into<String>) {
    let message = message.into();
    let sanitized = message.replace('\0', "\\0");
    LAST_ERROR.with(|last_error| {
        *last_error.borrow_mut() = CString::new(sanitized).ok();
    });
}

pub(crate) fn clear_last_error() {
    LAST_ERROR.with(|last_error| {
        *last_error.borrow_mut() = None;
    });
}

fn sqlite_vfs() -> ffi::sqlite3_vfs {
    ffi::sqlite3_vfs {
        iVersion: 3,
        szOsFile: std::mem::size_of::<file::KimojioFile>() as c_int,
        mxPathname: path::MAX_PATHNAME,
        pNext: std::ptr::null_mut(),
        zName: KIMOJIO_STACK_VFS_NAME_C.as_ptr(),
        pAppData: std::ptr::null_mut(),
        xOpen: Some(x_open),
        xDelete: Some(x_delete),
        xAccess: Some(x_access),
        xFullPathname: Some(x_full_pathname),
        xDlOpen: Some(x_dl_open),
        xDlError: Some(x_dl_error),
        xDlSym: Some(x_dl_sym),
        xDlClose: Some(x_dl_close),
        xRandomness: Some(x_randomness),
        xSleep: Some(x_sleep),
        xCurrentTime: Some(x_current_time),
        xGetLastError: Some(x_get_last_error),
        xCurrentTimeInt64: Some(x_current_time_int64),
        xSetSystemCall: None,
        xGetSystemCall: None,
        xNextSystemCall: None,
    }
}

unsafe extern "C" fn x_open(
    _vfs: *mut ffi::sqlite3_vfs,
    z_name: *const c_char,
    sqlite_file: *mut ffi::sqlite3_file,
    flags: c_int,
    out_flags: *mut c_int,
) -> c_int {
    diagnostics::bump(Counter::Open);
    clear_last_error();
    if sqlite_file.is_null() {
        set_last_error("xOpen failed: SQLite provided a null sqlite3_file pointer");
        return ffi::SQLITE_CANTOPEN;
    }
    unsafe {
        (*sqlite_file).pMethods = std::ptr::null();
    }
    if flags & ffi::SQLITE_OPEN_MEMORY != 0 {
        set_last_error("xOpen failed: in-memory databases are not supported by kimojio-stack VFS");
        return ffi::SQLITE_CANTOPEN;
    }

    let (path, delete_on_close) = match unsafe { path::path_from_c(z_name) } {
        Some(path) => (path, flags & ffi::SQLITE_OPEN_DELETEONCLOSE != 0),
        None => (path::temporary_path(), true),
    };
    let full_path = match path::full_pathbuf(&path) {
        Ok(path) => path,
        Err(code) => return code,
    };
    let open_flags = if delete_on_close {
        flags | ffi::SQLITE_OPEN_DELETEONCLOSE
    } else {
        flags
    };
    let (fd, readonly, runtime_delete_on_close) =
        match file::open_runtime_file(&full_path, open_flags) {
            Ok(opened) => opened,
            Err(code) => return code,
        };

    unsafe {
        file::init_file(
            sqlite_file,
            fd,
            Some(full_path),
            delete_on_close || runtime_delete_on_close,
            readonly,
            flags & ffi::SQLITE_OPEN_NOFOLLOW != 0,
        );
        if !out_flags.is_null() {
            *out_flags = if readonly {
                ffi::SQLITE_OPEN_READONLY
            } else {
                ffi::SQLITE_OPEN_READWRITE
            };
        }
    }
    ffi::SQLITE_OK
}

unsafe extern "C" fn x_delete(
    _vfs: *mut ffi::sqlite3_vfs,
    z_name: *const c_char,
    sync_dir: c_int,
) -> c_int {
    diagnostics::bump(Counter::Delete);
    clear_last_error();
    let Some(path) = (unsafe { path::path_from_c(z_name) }) else {
        set_last_error("xDelete failed: SQLite provided an empty path");
        return ffi::SQLITE_IOERR_DELETE;
    };
    let full_path = match path::full_pathbuf(&path) {
        Ok(path) => path,
        Err(_) => return ffi::SQLITE_IOERR_DELETE,
    };
    let rc = file::delete_path(&full_path);
    if rc != ffi::SQLITE_OK && rc != ffi::SQLITE_IOERR_DELETE_NOENT {
        return rc;
    }
    if sync_dir != 0 {
        let sync_rc = file::sync_parent_dir(&full_path);
        if sync_rc != ffi::SQLITE_OK {
            return sync_rc;
        }
    }
    rc
}

unsafe extern "C" fn x_access(
    _vfs: *mut ffi::sqlite3_vfs,
    z_name: *const c_char,
    _flags: c_int,
    out: *mut c_int,
) -> c_int {
    diagnostics::bump(Counter::Access);
    clear_last_error();
    if out.is_null() {
        set_last_error("xAccess failed: SQLite provided a null output pointer");
        return ffi::SQLITE_IOERR_ACCESS;
    }
    let Some(path) = (unsafe { path::path_from_c(z_name) }) else {
        set_last_error("xAccess failed: SQLite provided an empty path");
        return ffi::SQLITE_IOERR_ACCESS;
    };
    let full_path = match path::full_pathbuf(&path) {
        Ok(path) => path,
        Err(_) => return ffi::SQLITE_IOERR_ACCESS,
    };
    match file::access_path(&full_path, _flags) {
        Ok(exists) => {
            unsafe {
                *out = c_int::from(exists);
            }
            ffi::SQLITE_OK
        }
        Err(code) => code,
    }
}

unsafe extern "C" fn x_full_pathname(
    _vfs: *mut ffi::sqlite3_vfs,
    z_name: *const c_char,
    n_out: c_int,
    z_out: *mut c_char,
) -> c_int {
    unsafe { path::copy_full_path(z_name, n_out, z_out) }
}

unsafe extern "C" fn x_dl_open(
    _vfs: *mut ffi::sqlite3_vfs,
    _z_filename: *const c_char,
) -> *mut c_void {
    std::ptr::null_mut()
}

unsafe extern "C" fn x_dl_error(
    _vfs: *mut ffi::sqlite3_vfs,
    n_byte: c_int,
    z_err_msg: *mut c_char,
) {
    if z_err_msg.is_null() || n_byte <= 0 {
        return;
    }
    let message = b"dynamic extension loading is not supported by kimojio-stack SQLite VFS\0";
    let copy_len = message.len().min(n_byte as usize);
    unsafe {
        std::ptr::copy_nonoverlapping(message.as_ptr().cast::<c_char>(), z_err_msg, copy_len);
        *z_err_msg.add(copy_len - 1) = 0;
    }
}

unsafe extern "C" fn x_dl_sym(
    _vfs: *mut ffi::sqlite3_vfs,
    _handle: *mut c_void,
    _z_symbol: *const c_char,
) -> Option<unsafe extern "C" fn(*mut ffi::sqlite3_vfs, *mut c_void, *const c_char)> {
    None
}

unsafe extern "C" fn x_dl_close(_vfs: *mut ffi::sqlite3_vfs, _handle: *mut c_void) {}

unsafe extern "C" fn x_randomness(
    _vfs: *mut ffi::sqlite3_vfs,
    n_byte: c_int,
    z_out: *mut c_char,
) -> c_int {
    if z_out.is_null() || n_byte <= 0 {
        return 0;
    }
    let mut filled = 0;
    let random = rustix::rand::getrandom(
        unsafe { std::slice::from_raw_parts_mut(z_out.cast::<u8>(), n_byte as usize) },
        rustix::rand::GetRandomFlags::empty(),
    );
    if random.is_ok() {
        return n_byte;
    }

    let mut seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or_default()
        ^ u64::from(std::process::id());
    while filled < n_byte as usize {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let bytes = seed.to_ne_bytes();
        let copy = bytes.len().min(n_byte as usize - filled);
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr().cast::<c_char>(), z_out.add(filled), copy);
        }
        filled += copy;
    }
    n_byte
}

unsafe extern "C" fn x_sleep(_vfs: *mut ffi::sqlite3_vfs, microseconds: c_int) -> c_int {
    let duration = Duration::from_micros(microseconds.max(0) as u64);
    if RuntimeContext::with_current(|cx| cx.sleep(duration)).is_none_or(|result| result.is_err()) {
        std::thread::sleep(duration);
    }
    microseconds
}

unsafe extern "C" fn x_current_time(_vfs: *mut ffi::sqlite3_vfs, out: *mut f64) -> c_int {
    if out.is_null() {
        return ffi::SQLITE_IOERR;
    }
    let millis = current_time_millis();
    unsafe {
        *out = millis as f64 / 86_400_000.0;
    }
    ffi::SQLITE_OK
}

unsafe extern "C" fn x_get_last_error(
    _vfs: *mut ffi::sqlite3_vfs,
    n_byte: c_int,
    z_err_msg: *mut c_char,
) -> c_int {
    if z_err_msg.is_null() || n_byte <= 0 {
        return 0;
    }
    LAST_ERROR.with(|last_error| {
        let borrowed = last_error.borrow();
        let bytes = borrowed
            .as_ref()
            .map(|message| message.as_bytes_with_nul())
            .unwrap_or(b"kimojio-stack SQLite VFS: no additional error context\0");
        let copy_len = bytes.len().min(n_byte as usize);
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr().cast::<c_char>(), z_err_msg, copy_len);
            *z_err_msg.add(copy_len - 1) = 0;
        }
    });
    0
}

unsafe extern "C" fn x_current_time_int64(
    _vfs: *mut ffi::sqlite3_vfs,
    out: *mut ffi::sqlite3_int64,
) -> c_int {
    if out.is_null() {
        return ffi::SQLITE_IOERR;
    }
    unsafe {
        *out = current_time_millis() as ffi::sqlite3_int64;
    }
    ffi::SQLITE_OK
}

fn current_time_millis() -> i64 {
    const UNIX_EPOCH_JULIAN_MILLIS: i64 = 210_866_760_000_000;
    let unix_millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default();
    UNIX_EPOCH_JULIAN_MILLIS.saturating_add(unix_millis)
}
