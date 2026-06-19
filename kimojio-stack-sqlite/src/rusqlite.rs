//! Helpers for opening rusqlite connections through the kimojio-stack VFS.

use std::path::Path;

use ::rusqlite::{Connection, OpenFlags};

use crate::{KIMOJIO_STACK_VFS_NAME, RegisterError, register};

/// Returns the default open flags used by [`open`].
///
/// The helper opts into SQLite's full-mutex connection mode so callers using
/// thread-safe SQLite builds get the same conservative connection semantics in
/// examples and validation.
pub fn default_open_flags() -> OpenFlags {
    OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_FULL_MUTEX
}

/// Registers the kimojio-stack VFS, returning a rusqlite-compatible error.
pub fn register_vfs() -> ::rusqlite::Result<()> {
    register().map_err(register_error_to_rusqlite)
}

/// Opens a rusqlite connection through the kimojio-stack VFS.
pub fn open(path: impl AsRef<Path>) -> ::rusqlite::Result<Connection> {
    open_with_flags(path, default_open_flags())
}

/// Opens a kimojio VFS connection, runs `f`, and closes before returning.
///
/// Prefer this helper when a connection should not escape the active runtime
/// callback/task.
pub fn with_connection<T>(
    path: impl AsRef<Path>,
    f: impl FnOnce(&Connection) -> ::rusqlite::Result<T>,
) -> ::rusqlite::Result<T> {
    let connection = open(path)?;
    f(&connection)
}

/// Opens a rusqlite connection through the kimojio-stack VFS using custom flags.
pub fn open_with_flags(path: impl AsRef<Path>, flags: OpenFlags) -> ::rusqlite::Result<Connection> {
    register_vfs()?;
    Connection::open_with_flags_and_vfs(path, flags, KIMOJIO_STACK_VFS_NAME)
}

/// Opens a kimojio VFS connection with custom flags, runs `f`, and closes before returning.
pub fn with_connection_flags<T>(
    path: impl AsRef<Path>,
    flags: OpenFlags,
    f: impl FnOnce(&Connection) -> ::rusqlite::Result<T>,
) -> ::rusqlite::Result<T> {
    let connection = open_with_flags(path, flags)?;
    f(&connection)
}

/// Opens a read-only rusqlite connection through the kimojio-stack VFS.
pub fn open_read_only(path: impl AsRef<Path>) -> ::rusqlite::Result<Connection> {
    open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_FULL_MUTEX,
    )
}

fn register_error_to_rusqlite(error: RegisterError) -> ::rusqlite::Error {
    ::rusqlite::Error::SqliteFailure(
        ::rusqlite::ffi::Error::new(error.code()),
        Some(error.to_string()),
    )
}
