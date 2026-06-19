use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use libsqlite3_sys as ffi;

pub const MAX_PATHNAME: c_int = 4096;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);

pub unsafe fn path_from_c(z_name: *const c_char) -> Option<PathBuf> {
    if z_name.is_null() {
        return None;
    }

    let bytes = unsafe { CStr::from_ptr(z_name) }.to_bytes();
    if bytes.is_empty() {
        None
    } else {
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
    }
}

pub fn full_path(path: &Path) -> Result<CString, c_int> {
    let normalized = full_pathbuf(path)?;
    let bytes = normalized.as_os_str().as_bytes();
    CString::new(bytes).map_err(|_| ffi::SQLITE_CANTOPEN_CONVPATH)
}

pub fn full_pathbuf(path: &Path) -> Result<PathBuf, c_int> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|_| ffi::SQLITE_CANTOPEN_FULLPATH)?
            .join(path)
    };
    Ok(lexical_normalize(&absolute))
}

pub unsafe fn copy_full_path(z_name: *const c_char, n_out: c_int, z_out: *mut c_char) -> c_int {
    if z_name.is_null() || z_out.is_null() || n_out <= 0 {
        return ffi::SQLITE_CANTOPEN_FULLPATH;
    }

    let Some(path) = (unsafe { path_from_c(z_name) }) else {
        return ffi::SQLITE_CANTOPEN_FULLPATH;
    };
    let Ok(full) = full_path(&path) else {
        return ffi::SQLITE_CANTOPEN_FULLPATH;
    };
    let bytes = full.as_bytes_with_nul();
    if bytes.len() > n_out as usize {
        return ffi::SQLITE_CANTOPEN_FULLPATH;
    }

    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr().cast::<c_char>(), z_out, bytes.len());
    }
    ffi::SQLITE_OK
}

pub fn temporary_path() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "kimojio-stack-sqlite-{}-{nanos}-{counter}.tmp",
        std::process::id()
    ))
}

pub fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut bytes = path.as_os_str().as_bytes().to_vec();
    bytes.extend_from_slice(suffix.as_bytes());
    PathBuf::from(std::ffi::OsString::from_vec(bytes))
}

pub fn parent_dir(path: &Path) -> Option<PathBuf> {
    path.parent().map(Path::to_path_buf)
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
            Component::RootDir | Component::Prefix(_) => out.push(component.as_os_str()),
        }
    }
    out
}
