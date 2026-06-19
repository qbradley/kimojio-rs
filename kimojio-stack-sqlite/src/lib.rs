//! SQLite VFS integration for `kimojio-stack`.
//!
//! The crate registers a SQLite virtual filesystem whose file callbacks route
//! through the currently executing `kimojio-stack` runtime context. The VFS is
//! intended for synchronous SQLite integrations that are already executing
//! inside stackful runtime code.
//!
//! Use this crate when rusqlite or direct SQLite FFI code needs durable local
//! database files without letting SQLite's default VFS block the scheduler
//! thread for database file I/O. The VFS name is [`KIMOJIO_STACK_VFS_NAME`].
//! The default `rusqlite` feature provides helpers that register the VFS and
//! open connections with the right VFS name.
//!
//! # Quick rusqlite example
//!
//! ```no_run
//! # fn example() -> rusqlite::Result<()> {
//! let mut runtime = kimojio_stack::Runtime::new();
//! runtime.block_on(|_| {
//!     kimojio_stack_sqlite::rusqlite::with_connection("app.db", |conn| {
//!         conn.execute_batch(
//!             "CREATE TABLE IF NOT EXISTS items(value TEXT);
//!              INSERT INTO items(value) VALUES ('hello');",
//!         )
//!     })
//! });
//! # Ok(())
//! # }
//! ```
//!
//! # Runtime requirements
//!
//! SQLite callbacks are synchronous, but this VFS uses
//! `kimojio-stack` positioned I/O internally. Calls must happen while a
//! [`kimojio_stack::RuntimeContext`] is the current executing context. Using the
//! VFS outside a compatible runtime returns SQLite I/O errors instead of falling
//! back to blocking filesystem calls.
//!
//! # Performance and limitations
//!
//! Very small transactions are dominated by fixed callback overhead because each
//! SQLite file callback crosses into the runtime. For latency-sensitive
//! steady-state workloads, benchmark with runtime polling knobs such as busy-poll
//! and SQPOLL. Database file locks use OS advisory locks plus process-local
//! checks; WAL shared-memory lock bookkeeping is still process-local in this
//! milestone. SQLite CPU execution, mutex waits, and WAL mmap page faults remain
//! synchronous while the current coroutine is inside SQLite.

mod file;
mod path;
mod shm;
pub mod vfs;

pub mod diagnostics;

#[cfg(feature = "rusqlite")]
pub mod rusqlite;

pub use vfs::{KIMOJIO_STACK_VFS_NAME, RegisterError, register};
