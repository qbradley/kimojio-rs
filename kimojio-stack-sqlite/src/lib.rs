//! SQLite VFS integration for `kimojio-stack`.
//!
//! The crate registers a SQLite virtual filesystem whose file callbacks route
//! through the currently executing `kimojio-stack` runtime context. The VFS is
//! intended for synchronous SQLite integrations that are already executing
//! inside stackful runtime code.

mod file;
mod path;
mod shm;
pub mod vfs;

pub mod diagnostics;

#[cfg(feature = "rusqlite")]
pub mod rusqlite;

pub use vfs::{KIMOJIO_STACK_VFS_NAME, RegisterError, register};
