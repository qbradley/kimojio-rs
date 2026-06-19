# Stackful SQLite VFS

`kimojio-stack-sqlite` provides a SQLite VFS named `kimojio-stack` for
rusqlite-backed code running inside `kimojio-stack` or `kimojio-stack-steal`.
The VFS routes SQLite file reads, writes, syncs, truncates, status queries, and
WAL shared-memory callbacks through the active stackful runtime context instead
of SQLite's default blocking VFS.

The first milestone targets durable local files. Normal database, journal, WAL,
shared-memory, temporary, and delete-on-close files are represented on the local
filesystem. If the VFS is used outside a compatible runtime context, SQLite gets
an I/O error instead of a blocking fallback.

## Usage

Stack runtime:

```rust
let mut runtime = kimojio_stack::Runtime::new();
runtime.block_on(|_| {
    let conn = kimojio_stack_sqlite::rusqlite::open("app.db").unwrap();
    conn.execute_batch("CREATE TABLE IF NOT EXISTS items(value TEXT);").unwrap();
});
```

Stack-steal runtime:

```rust
let mut runtime = kimojio_stack_steal::Runtime::new();
runtime.block_on(|cx| {
    cx.scope(|scope| {
        scope.spawn_local(|_| {
            let conn = kimojio_stack_sqlite::rusqlite::open("app.db").unwrap();
            conn.execute("INSERT INTO items(value) VALUES ('ok')", []).unwrap();
        }).join(cx);
    });
});
```

Use `kimojio_stack_sqlite::rusqlite::with_connection` when a connection should
be scoped to the active runtime callback. Use `open_with_flags` for custom
rusqlite `OpenFlags`, or call `kimojio_stack_sqlite::register()` and pass
`kimojio-stack` as the VFS name for direct SQLite FFI usage.

## Validation

Run the rusqlite helper and VFS validation tests:

```sh
cargo test -p kimojio-stack-sqlite --all-targets
```

Run the comparison example:

```sh
cargo run -p kimojio-stack-sqlite --example rusqlite_vfs -- --runtime stack
cargo run -p kimojio-stack-sqlite --example rusqlite_vfs -- --runtime steal
cargo run -p kimojio-stack-sqlite --example rusqlite_vfs -- --runtime default-vfs
```

The stack and steal example modes run the same workload through SQLite's default
VFS and print `ratio_vs_default`.

For many tiny SQLite callbacks, io_uring submit/wait overhead dominates. Use
runtime polling knobs when benchmarking latency-sensitive SQLite work:

```sh
cargo run --release -p kimojio-stack-sqlite --example rusqlite_vfs -- \
  --runtime stack --iterations 10000 --busy-poll --sqpoll-idle-ms 1000
cargo run --release -p kimojio-stack-sqlite --example rusqlite_vfs -- \
  --runtime steal --iterations 10000 --busy-poll --sqpoll-idle-ms 1000
```

On the local validation host, a 10,000-row single-transaction WAL/FULL workload
measured about 1.80x default-VFS latency for stack and 1.85x for stack-steal with
busy-poll plus SQPOLL. A 500-row workload remained fixed-overhead dominated at
about 3.21x for stack because it issued only 17 reads, 25 writes, and 9 syncs;
this is useful for understanding startup/small-transaction overhead but is not
representative of steady-state transaction throughput.

## Current limits

- Database file locks use OS advisory locks plus process-local checks. WAL
  shared-memory lock bookkeeping remains process-local in this milestone, so
  cross-process WAL coordination is not a supported correctness claim yet.
- Avoid symlink or hard-link aliases to the same database path until stable
  file-identity lock keys are added.
- WAL `-shm` files are memory-mapped; file creation and sizing use runtime I/O,
  while mapping and page faults are synchronous OS memory behavior.
- `--busy-poll` and `--sqpoll-idle-ms` reduce latency by spending CPU. SQPOLL
  support depends on kernel configuration and permissions.
- SQLite CPU execution and SQLite mutex waits remain synchronous while the
  current coroutine is inside SQLite.
- The VFS deliberately fails outside runtime context and does not provide a
  helper-thread fallback.
