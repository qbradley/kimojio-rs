use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::ptr;

use kimojio_stack::Runtime;
use kimojio_stack_sqlite::{KIMOJIO_STACK_VFS_NAME, register};
use libsqlite3_sys as ffi;

#[test]
fn registration_is_idempotent() {
    register().unwrap();
    register().unwrap();

    let name = CString::new(KIMOJIO_STACK_VFS_NAME).unwrap();
    let vfs = unsafe { ffi::sqlite3_vfs_find(name.as_ptr()) };
    assert!(!vfs.is_null());
}

#[test]
fn open_fails_outside_runtime_context() {
    register().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("outside.db");

    let mut db = ptr::null_mut();
    let rc = unsafe { open_raw(&path, &mut db) };
    assert_ne!(rc, ffi::SQLITE_OK);
    if !db.is_null() {
        unsafe {
            ffi::sqlite3_close(db);
        }
    }

    let name = CString::new(KIMOJIO_STACK_VFS_NAME).unwrap();
    let vfs = unsafe { ffi::sqlite3_vfs_find(name.as_ptr()) };
    assert!(!vfs.is_null());
    let mut message = [0_i8; 256];
    unsafe {
        ((*vfs).xGetLastError.unwrap())(vfs, message.len() as c_int, message.as_mut_ptr());
    }
    let message = unsafe { CStr::from_ptr(message.as_ptr()) }.to_string_lossy();
    assert!(message.contains("no current kimojio-stack runtime context"));
}

#[test]
fn access_callback_reports_exists_and_readwrite_modes() {
    register().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("access.db");
    std::fs::write(&path, b"test").unwrap();
    let missing = dir.path().join("missing.db");

    let mut runtime = Runtime::new();
    runtime.block_on(|_| {
        assert_eq!(vfs_access(&path, ffi::SQLITE_ACCESS_EXISTS), 1);
        assert_eq!(vfs_access(&path, ffi::SQLITE_ACCESS_READ), 1);
        assert_eq!(vfs_access(&path, ffi::SQLITE_ACCESS_READWRITE), 1);
        assert_eq!(vfs_access(&missing, ffi::SQLITE_ACCESS_EXISTS), 0);
        assert_eq!(vfs_access(&missing, ffi::SQLITE_ACCESS_READWRITE), 0);
    });
}

#[test]
fn rollback_journal_transaction_is_durable() {
    register().unwrap();
    assert_ne!(unsafe { ffi::sqlite3_threadsafe() }, 0);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    let mut runtime = Runtime::new();

    runtime.block_on(|_| {
        let db = open_db(&path);
        exec(
            db,
            "PRAGMA journal_mode=DELETE;
             CREATE TABLE items(id INTEGER PRIMARY KEY, value TEXT NOT NULL);
             BEGIN IMMEDIATE;
             INSERT INTO items(value) VALUES ('alpha'), ('beta');
             COMMIT;",
        );
        assert_eq!(scalar(db, "PRAGMA integrity_check;"), "ok");
        close_db(db);

        let db = open_db(&path);
        assert_eq!(scalar(db, "SELECT COUNT(*) FROM items;"), "2");
        assert_eq!(scalar(db, "SELECT value FROM items WHERE id = 2;"), "beta");
        close_db(db);
    });
}

#[test]
fn wal_transaction_reopens_with_integrity() {
    register().unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wal.db");
    let mut runtime = Runtime::new();

    runtime.block_on(|_| {
        let db = open_db(&path);
        assert_eq!(scalar(db, "PRAGMA journal_mode=WAL;"), "wal");
        exec(
            db,
            "CREATE TABLE items(id INTEGER PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO items(value) VALUES ('one'), ('two'), ('three');",
        );
        assert_eq!(scalar(db, "PRAGMA integrity_check;"), "ok");
        close_db(db);

        let db = open_db(&path);
        assert_eq!(scalar(db, "PRAGMA integrity_check;"), "ok");
        assert_eq!(scalar(db, "SELECT COUNT(*) FROM items;"), "3");
        close_db(db);
    });
}

fn open_db(path: &Path) -> *mut ffi::sqlite3 {
    let mut db = ptr::null_mut();
    let rc = unsafe { open_raw(path, &mut db) };
    if rc != ffi::SQLITE_OK {
        let message = if db.is_null() {
            format!("sqlite3_open_v2 failed with code {rc}")
        } else {
            unsafe { error_message(db) }
        };
        if !db.is_null() {
            unsafe {
                ffi::sqlite3_close(db);
            }
        }
        panic!("{message}");
    }
    db
}

unsafe fn open_raw(path: &Path, db: *mut *mut ffi::sqlite3) -> c_int {
    let filename = CString::new(path.as_os_str().as_bytes()).unwrap();
    let vfs = CString::new(KIMOJIO_STACK_VFS_NAME).unwrap();
    unsafe {
        ffi::sqlite3_open_v2(
            filename.as_ptr(),
            db,
            ffi::SQLITE_OPEN_READWRITE | ffi::SQLITE_OPEN_CREATE | ffi::SQLITE_OPEN_FULLMUTEX,
            vfs.as_ptr(),
        )
    }
}

fn vfs_access(path: &Path, flags: c_int) -> c_int {
    let name = CString::new(KIMOJIO_STACK_VFS_NAME).unwrap();
    let vfs = unsafe { ffi::sqlite3_vfs_find(name.as_ptr()) };
    assert!(!vfs.is_null());
    let path = CString::new(path.as_os_str().as_bytes()).unwrap();
    let mut out = -1;
    let rc = unsafe { ((*vfs).xAccess.unwrap())(vfs, path.as_ptr(), flags, &mut out) };
    assert_eq!(rc, ffi::SQLITE_OK);
    out
}

fn exec(db: *mut ffi::sqlite3, sql: &str) {
    let sql = CString::new(sql).unwrap();
    let mut error = ptr::null_mut();
    let rc = unsafe { ffi::sqlite3_exec(db, sql.as_ptr(), None, ptr::null_mut(), &mut error) };
    if rc != ffi::SQLITE_OK {
        let message = unsafe { take_exec_error(db, error) };
        panic!("sqlite3_exec failed: {message}");
    }
}

fn scalar(db: *mut ffi::sqlite3, sql: &str) -> String {
    unsafe extern "C" fn callback(
        data: *mut c_void,
        argc: c_int,
        argv: *mut *mut c_char,
        _columns: *mut *mut c_char,
    ) -> c_int {
        if argc > 0 && !argv.is_null() {
            let value = unsafe { *argv };
            if !value.is_null() {
                let out = unsafe { &mut *data.cast::<Option<String>>() };
                *out = Some(
                    unsafe { CStr::from_ptr(value) }
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
        0
    }

    let sql = CString::new(sql).unwrap();
    let mut out = None;
    let mut error = ptr::null_mut();
    let rc = unsafe {
        ffi::sqlite3_exec(
            db,
            sql.as_ptr(),
            Some(callback),
            (&mut out as *mut Option<String>).cast::<c_void>(),
            &mut error,
        )
    };
    if rc != ffi::SQLITE_OK {
        let message = unsafe { take_exec_error(db, error) };
        panic!("sqlite3_exec scalar failed: {message}");
    }
    out.expect("scalar query did not return a row")
}

fn close_db(db: *mut ffi::sqlite3) {
    let rc = unsafe { ffi::sqlite3_close(db) };
    assert_eq!(rc, ffi::SQLITE_OK);
}

unsafe fn take_exec_error(db: *mut ffi::sqlite3, error: *mut c_char) -> String {
    if error.is_null() {
        return unsafe { error_message(db) };
    }
    let message = unsafe { CStr::from_ptr(error) }
        .to_string_lossy()
        .into_owned();
    unsafe {
        ffi::sqlite3_free(error.cast::<c_void>());
    }
    message
}

unsafe fn error_message(db: *mut ffi::sqlite3) -> String {
    let message = unsafe { ffi::sqlite3_errmsg(db) };
    if message.is_null() {
        "unknown SQLite error".to_owned()
    } else {
        unsafe { CStr::from_ptr(message) }
            .to_string_lossy()
            .into_owned()
    }
}
