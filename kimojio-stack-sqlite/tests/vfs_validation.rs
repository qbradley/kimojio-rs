use std::path::Path;

use kimojio_stack::Runtime as StackRuntime;
use kimojio_stack_sqlite::rusqlite as stack_sqlite;
use kimojio_stack_steal::Runtime as StealRuntime;
use rusqlite::ErrorCode;

#[test]
fn validation_stack_journal_wal_reopen_and_integrity() {
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = StackRuntime::new();

    runtime.block_on(|_| {
        validate_journal_mode(&dir.path().join("stack-delete.db"), "DELETE");
        validate_journal_mode(&dir.path().join("stack-wal.db"), "WAL");
    });
}

#[test]
fn validation_stack_steal_journal_wal_reopen_and_integrity() {
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = StealRuntime::new();

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let delete_path = dir.path().join("steal-delete.db");
            let wal_path = dir.path().join("steal-wal.db");
            scope
                .spawn_local(move |_| {
                    validate_journal_mode(&delete_path, "DELETE");
                    validate_journal_mode(&wal_path, "WAL");
                })
                .join(cx);
        });
    });
}

#[test]
fn validation_outside_runtime_rusqlite_helper_fails() {
    let dir = tempfile::tempdir().unwrap();
    let error = stack_sqlite::open(dir.path().join("outside.db")).expect_err("open should fail");
    assert!(matches!(
        error,
        rusqlite::Error::SqliteFailure(_, Some(_)) | rusqlite::Error::SqliteFailure(_, None)
    ));
}

#[test]
fn validation_multiple_connections_serialize_transactions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("multi.db");
    let mut runtime = StackRuntime::new();

    runtime.block_on(|_| {
        let first = stack_sqlite::open(&path).unwrap();
        let second = stack_sqlite::open(&path).unwrap();
        first
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 CREATE TABLE items(id INTEGER PRIMARY KEY, value TEXT NOT NULL);",
            )
            .unwrap();
        first
            .execute("INSERT INTO items(value) VALUES ('first')", [])
            .unwrap();
        let seen: i64 = second
            .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(seen, 1);
        second
            .execute("INSERT INTO items(value) VALUES ('second')", [])
            .unwrap();
        let values: String = first
            .query_row("SELECT group_concat(value, ',') FROM items", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(values, "first,second");
        assert_eq!(integrity(&first), "ok");
    });
}

#[test]
fn validation_lock_contention_reports_busy_and_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("busy.db");
    let mut runtime = StackRuntime::new();

    runtime.block_on(|_| {
        let first = stack_sqlite::open(&path).unwrap();
        let second = stack_sqlite::open(&path).unwrap();
        first
            .execute_batch(
                "PRAGMA journal_mode=DELETE;
                 PRAGMA busy_timeout=0;
                 CREATE TABLE items(id INTEGER PRIMARY KEY, value TEXT NOT NULL);",
            )
            .unwrap();
        second.execute_batch("PRAGMA busy_timeout=0").unwrap();

        first.execute_batch("BEGIN IMMEDIATE").unwrap();
        first
            .execute("INSERT INTO items(value) VALUES ('held')", [])
            .unwrap();
        let error = second
            .execute("INSERT INTO items(value) VALUES ('blocked')", [])
            .expect_err("second writer should be blocked while first transaction is held");
        assert!(matches!(
            error,
            rusqlite::Error::SqliteFailure(ref err, _)
                if matches!(err.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
        ));

        first.execute_batch("COMMIT").unwrap();
        second
            .execute("INSERT INTO items(value) VALUES ('after')", [])
            .unwrap();
        assert_eq!(integrity(&first), "ok");
    });
}

#[test]
fn validation_cross_runtime_reopen_same_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cross-runtime.db");

    let mut stack = StackRuntime::new();
    stack.block_on(|_| {
        let conn = stack_sqlite::open(&path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE items(id INTEGER PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO items(value) VALUES ('stack');",
        )
        .unwrap();
        assert_eq!(integrity(&conn), "ok");
    });

    let mut steal = StealRuntime::new();
    steal.block_on(|cx| {
        cx.scope(|scope| {
            let path = path.clone();
            scope
                .spawn_local(move |_| {
                    let conn = stack_sqlite::open(&path).unwrap();
                    let value: String = conn
                        .query_row("SELECT value FROM items WHERE id = 1", [], |row| row.get(0))
                        .unwrap();
                    assert_eq!(value, "stack");
                    conn.execute("INSERT INTO items(value) VALUES ('steal')", [])
                        .unwrap();
                    assert_eq!(integrity(&conn), "ok");
                })
                .join(cx);
        });
    });

    let mut stack = StackRuntime::new();
    stack.block_on(|_| {
        let conn = stack_sqlite::open(&path).unwrap();
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 2);
        assert_eq!(integrity(&conn), "ok");
    });
}

fn validate_journal_mode(path: &Path, mode: &str) {
    let conn = stack_sqlite::open(path).unwrap();
    let actual: String = conn
        .pragma_update_and_check(None, "journal_mode", mode, |row| row.get(0))
        .unwrap();
    assert_eq!(actual.to_ascii_lowercase(), mode.to_ascii_lowercase());
    conn.execute_batch(
        "CREATE TABLE items(id INTEGER PRIMARY KEY, value TEXT NOT NULL);
         BEGIN IMMEDIATE;
         INSERT INTO items(value) VALUES ('a'), ('b'), ('c');
         COMMIT;",
    )
    .unwrap();
    assert_eq!(integrity(&conn), "ok");
    drop(conn);

    let conn = stack_sqlite::open(path).unwrap();
    assert_eq!(integrity(&conn), "ok");
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 3);
}

fn integrity(conn: &rusqlite::Connection) -> String {
    conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap()
}
