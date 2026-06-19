use std::path::PathBuf;
use std::time::Instant;

use kimojio_stack::Runtime as StackRuntime;
use kimojio_stack_sqlite::rusqlite as stack_sqlite;
use kimojio_stack_steal::Runtime as StealRuntime;
use rusqlite::Connection;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeMode {
    Stack,
    Steal,
    DefaultVfs,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse()?;
    let (result, elapsed) = run_mode(args.runtime, &args.path, args.iterations)?;

    print!(
        "runtime={:?} path={} rows={} integrity={} elapsed_ms={:.3}",
        args.runtime,
        args.path.display(),
        result.rows,
        result.integrity,
        elapsed.as_secs_f64() * 1000.0
    );
    if args.runtime != RuntimeMode::DefaultVfs {
        let default_path = comparison_path(&args.path);
        let (default_result, default_elapsed) =
            run_mode(RuntimeMode::DefaultVfs, &default_path, args.iterations)?;
        let ratio = elapsed.as_secs_f64() / default_elapsed.as_secs_f64().max(f64::EPSILON);
        print!(
            " default_vfs_path={} default_vfs_rows={} default_vfs_ms={:.3} ratio_vs_default={:.3}",
            default_path.display(),
            default_result.rows,
            default_elapsed.as_secs_f64() * 1000.0,
            ratio
        );
    }
    println!();
    Ok(())
}

fn run_mode(
    runtime: RuntimeMode,
    path: &std::path::Path,
    iterations: usize,
) -> Result<(Summary, std::time::Duration), Box<dyn std::error::Error>> {
    let result = match runtime {
        RuntimeMode::Stack => {
            let mut runtime = StackRuntime::new();
            runtime.block_on(|_| {
                let started = Instant::now();
                run_kimojio_workload(path, iterations).map(|summary| (summary, started.elapsed()))
            })
        }
        RuntimeMode::Steal => {
            let mut runtime = StealRuntime::new();
            runtime.block_on(|cx| {
                cx.scope(|scope| {
                    let path = path.to_path_buf();
                    scope
                        .spawn_local(move |_| {
                            let started = Instant::now();
                            run_kimojio_workload(&path, iterations)
                                .map(|summary| (summary, started.elapsed()))
                        })
                        .join(cx)
                })
            })
        }
        RuntimeMode::DefaultVfs => {
            let started = Instant::now();
            run_default_workload(path, iterations).map(|summary| (summary, started.elapsed()))
        }
    }?;
    Ok(result)
}

fn run_kimojio_workload(path: &std::path::Path, iterations: usize) -> rusqlite::Result<Summary> {
    let conn = stack_sqlite::open(path)?;
    run_workload(&conn, iterations)?;
    drop(conn);

    let conn = stack_sqlite::open(path)?;
    summarize(&conn)
}

fn run_default_workload(path: &std::path::Path, iterations: usize) -> rusqlite::Result<Summary> {
    let conn = Connection::open(path)?;
    run_workload(&conn, iterations)?;
    drop(conn);

    let conn = Connection::open(path)?;
    summarize(&conn)
}

fn run_workload(conn: &Connection, iterations: usize) -> rusqlite::Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         CREATE TABLE IF NOT EXISTS items(id INTEGER PRIMARY KEY, value TEXT NOT NULL);
         DELETE FROM items;",
    )?;
    let mut insert = conn.prepare("INSERT INTO items(value) VALUES (?1)")?;
    conn.execute_batch("BEGIN IMMEDIATE")?;
    for index in 0..iterations {
        insert.execute([format!("value-{index}")])?;
    }
    drop(insert);
    conn.execute_batch("COMMIT")?;
    Ok(())
}

fn summarize(conn: &Connection) -> rusqlite::Result<Summary> {
    let rows = conn.query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))?;
    let integrity = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    Ok(Summary { rows, integrity })
}

struct Summary {
    rows: i64,
    integrity: String,
}

struct Args {
    runtime: RuntimeMode,
    path: PathBuf,
    iterations: usize,
}

impl Args {
    fn parse() -> Result<Self, Box<dyn std::error::Error>> {
        let mut runtime = RuntimeMode::Stack;
        let mut path = None;
        let mut iterations = 100_usize;
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--runtime" => {
                    let Some(value) = args.next() else {
                        return Err("--runtime requires stack, steal, or default-vfs".into());
                    };
                    runtime = parse_runtime(&value)?;
                }
                "--path" => {
                    let Some(value) = args.next() else {
                        return Err("--path requires a filesystem path".into());
                    };
                    path = Some(PathBuf::from(value));
                }
                "--iterations" => {
                    let Some(value) = args.next() else {
                        return Err("--iterations requires a positive integer".into());
                    };
                    iterations = value.parse()?;
                }
                "--help" | "-h" => {
                    print_usage();
                    std::process::exit(0);
                }
                other => return Err(format!("unknown argument: {other}").into()),
            }
        }

        let path = path.unwrap_or_else(|| {
            std::env::temp_dir().join(format!(
                "kimojio-stack-sqlite-example-{}-{:?}.db",
                std::process::id(),
                runtime
            ))
        });
        Ok(Self {
            runtime,
            path,
            iterations,
        })
    }
}

fn parse_runtime(value: &str) -> Result<RuntimeMode, Box<dyn std::error::Error>> {
    match value {
        "stack" => Ok(RuntimeMode::Stack),
        "steal" => Ok(RuntimeMode::Steal),
        "default-vfs" | "default" => Ok(RuntimeMode::DefaultVfs),
        _ => Err(format!("unsupported runtime {value:?}; use stack, steal, or default-vfs").into()),
    }
}

fn print_usage() {
    println!(
        "Usage: cargo run -p kimojio-stack-sqlite --example rusqlite_vfs -- \\
         --runtime <stack|steal|default-vfs> [--path PATH] [--iterations N]"
    );
}

fn comparison_path(path: &std::path::Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| "sqlite-default-vfs.db".into());
    name.push(".default-vfs");
    path.with_file_name(name)
}
