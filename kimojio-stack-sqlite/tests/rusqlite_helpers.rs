use kimojio_stack::Runtime as StackRuntime;
use kimojio_stack_sqlite::rusqlite as stack_sqlite;
use kimojio_stack_steal::Runtime as StealRuntime;

#[test]
fn rusqlite_helper_opens_stack_connection_and_reopens_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stack.db");
    let mut runtime = StackRuntime::new();

    runtime.block_on(|_| {
        run_rusqlite_workload(&path);
        let count = stack_sqlite::with_connection(&path, |conn| {
            conn.query_row("SELECT COUNT(*) FROM items", [], |row| row.get::<_, i64>(0))
        })
        .unwrap();
        assert_eq!(count, 3);
        let conn = stack_sqlite::open_read_only(&path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 3);
    });
}

#[test]
fn rusqlite_helper_opens_stack_steal_connection() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("steal.db");
    let mut runtime = StealRuntime::new();

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let path = path.clone();
            scope
                .spawn_local(move |_| {
                    run_rusqlite_workload(&path);
                })
                .join(cx);
        });
    });
}

fn run_rusqlite_workload(path: &std::path::Path) {
    let conn = stack_sqlite::open(path).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode=DELETE;
         CREATE TABLE IF NOT EXISTS items(id INTEGER PRIMARY KEY, value TEXT NOT NULL);
         DELETE FROM items;
         BEGIN IMMEDIATE;
         INSERT INTO items(value) VALUES ('alpha'), ('beta'), ('gamma');
         COMMIT;",
    )
    .unwrap();
    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");
    drop(conn);

    let conn = stack_sqlite::open(path).unwrap();
    let value: String = conn
        .query_row("SELECT value FROM items WHERE id = 2", [], |row| row.get(0))
        .unwrap();
    assert_eq!(value, "beta");
}
