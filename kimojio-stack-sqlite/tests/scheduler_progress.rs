use std::cell::Cell;
use std::path::Path;
use std::rc::Rc;

use kimojio_stack::Runtime as StackRuntime;
use kimojio_stack_sqlite::rusqlite as stack_sqlite;
use kimojio_stack_steal::Runtime as StealRuntime;

#[test]
fn scheduler_progress_stack_runtime_during_sqlite_file_io() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("progress-stack.db");
    let mut runtime = StackRuntime::new();

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let done = Rc::new(Cell::new(false));
            let active = Rc::new(Cell::new(false));
            let progress = Rc::new(Cell::new(0usize));
            let progress_task = {
                let done = Rc::clone(&done);
                let active = Rc::clone(&active);
                let progress = Rc::clone(&progress);
                scope.spawn(move |cx| {
                    while !done.get() {
                        if active.get() {
                            progress.set(progress.get() + 1);
                        }
                        cx.yield_now();
                    }
                    progress.get()
                })
            };
            let db_task = {
                let done = Rc::clone(&done);
                let active = Rc::clone(&active);
                scope.spawn(move |_| {
                    active.set(true);
                    run_many_transactions(&path, 64);
                    done.set(true);
                })
            };

            db_task.join(cx);
            assert!(progress_task.join(cx) > 0);
        });
    });
}

#[test]
fn scheduler_progress_stack_steal_runtime_during_sqlite_file_io() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("progress-steal.db");
    let mut runtime = StealRuntime::new();

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let done = Rc::new(Cell::new(false));
            let active = Rc::new(Cell::new(false));
            let progress = Rc::new(Cell::new(0usize));
            let progress_task = {
                let done = Rc::clone(&done);
                let active = Rc::clone(&active);
                let progress = Rc::clone(&progress);
                scope.spawn_local(move |cx| {
                    while !done.get() {
                        if active.get() {
                            progress.set(progress.get() + 1);
                        }
                        cx.yield_now();
                    }
                    progress.get()
                })
            };
            let db_task = {
                let done = Rc::clone(&done);
                let active = Rc::clone(&active);
                scope.spawn_local(move |_| {
                    active.set(true);
                    run_many_transactions(&path, 64);
                    done.set(true);
                })
            };

            db_task.join(cx);
            assert!(progress_task.join(cx) > 0);
        });
    });
}

fn run_many_transactions(path: &Path, iterations: usize) {
    let conn = stack_sqlite::open(path).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=FULL;
         CREATE TABLE IF NOT EXISTS items(id INTEGER PRIMARY KEY, value TEXT NOT NULL);",
    )
    .unwrap();
    for index in 0..iterations {
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        conn.execute(
            "INSERT INTO items(value) VALUES (?1)",
            [format!("value-{index}")],
        )
        .unwrap();
        conn.execute_batch("COMMIT").unwrap();
    }
    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");
}
