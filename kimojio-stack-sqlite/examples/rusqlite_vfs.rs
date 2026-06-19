use std::path::PathBuf;
use std::time::Instant;

use kimojio_stack::{BusyPoll, Runtime as StackRuntime};
use kimojio_stack_sqlite::diagnostics;
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
    if args.counters {
        diagnostics::reset_counters();
    }
    diagnostics::set_counters_enabled(args.counters);
    let (result, elapsed) = run_mode(
        args.runtime,
        &args.path,
        args.iterations,
        args.busy_poll,
        args.sqpoll_idle,
        args.synchronous,
    )?;
    let counters = diagnostics::counters();

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
        let (default_result, default_elapsed) = run_mode(
            RuntimeMode::DefaultVfs,
            &default_path,
            args.iterations,
            false,
            None,
            args.synchronous,
        )?;
        let ratio = elapsed.as_secs_f64() / default_elapsed.as_secs_f64().max(f64::EPSILON);
        print!(
            " default_vfs_path={} default_vfs_rows={} default_vfs_ms={:.3} ratio_vs_default={:.3}",
            default_path.display(),
            default_result.rows,
            default_elapsed.as_secs_f64() * 1000.0,
            ratio
        );
    }
    if args.counters {
        print!(
            " counters={{open:{},close:{},read:{},write:{},truncate:{},sync:{},file_size:{},lock:{},unlock:{},check_reserved_lock:{},access:{},delete:{},shm_map:{},shm_lock:{},shm_unmap:{}}}",
            counters.open,
            counters.close,
            counters.read,
            counters.write,
            counters.truncate,
            counters.sync,
            counters.file_size,
            counters.lock,
            counters.unlock,
            counters.check_reserved_lock,
            counters.access,
            counters.delete,
            counters.shm_map,
            counters.shm_lock,
            counters.shm_unmap
        );
    }
    println!();
    Ok(())
}

fn run_mode(
    runtime: RuntimeMode,
    path: &std::path::Path,
    iterations: usize,
    busy_poll: bool,
    sqpoll_idle: Option<u32>,
    synchronous: Synchronous,
) -> Result<(Summary, std::time::Duration), Box<dyn std::error::Error>> {
    let result = match runtime {
        RuntimeMode::Stack => {
            let mut runtime = if busy_poll || sqpoll_idle.is_some() {
                let mut config = kimojio_stack::RuntimeConfig::default();
                if busy_poll {
                    config.busy_poll = BusyPoll::Always;
                }
                config.sqpoll_idle = sqpoll_idle;
                StackRuntime::with_config(config)
            } else {
                StackRuntime::new()
            };
            runtime.block_on(|_| {
                let started = Instant::now();
                run_kimojio_workload(path, iterations, synchronous)
                    .map(|summary| (summary, started.elapsed()))
            })
        }
        RuntimeMode::Steal => {
            let mut runtime = if busy_poll || sqpoll_idle.is_some() {
                let mut config = kimojio_stack_steal::RuntimeConfig::default();
                if busy_poll {
                    config.busy_poll = kimojio_stack_steal::BusyPoll::Always;
                }
                config.sqpoll_idle = sqpoll_idle;
                StealRuntime::with_config(config)
            } else {
                StealRuntime::new()
            };
            runtime.block_on(|cx| {
                cx.scope(|scope| {
                    let path = path.to_path_buf();
                    scope
                        .spawn_local(move |_| {
                            let started = Instant::now();
                            run_kimojio_workload(&path, iterations, synchronous)
                                .map(|summary| (summary, started.elapsed()))
                        })
                        .join(cx)
                })
            })
        }
        RuntimeMode::DefaultVfs => {
            let started = Instant::now();
            run_default_workload(path, iterations, synchronous)
                .map(|summary| (summary, started.elapsed()))
        }
    }?;
    Ok(result)
}

fn run_kimojio_workload(
    path: &std::path::Path,
    iterations: usize,
    synchronous: Synchronous,
) -> rusqlite::Result<Summary> {
    let conn = stack_sqlite::open(path)?;
    run_workload(&conn, iterations, synchronous)?;
    drop(conn);

    let conn = stack_sqlite::open(path)?;
    summarize(&conn)
}

fn run_default_workload(
    path: &std::path::Path,
    iterations: usize,
    synchronous: Synchronous,
) -> rusqlite::Result<Summary> {
    let conn = Connection::open(path)?;
    run_workload(&conn, iterations, synchronous)?;
    drop(conn);

    let conn = Connection::open(path)?;
    summarize(&conn)
}

fn run_workload(
    conn: &Connection,
    iterations: usize,
    synchronous: Synchronous,
) -> rusqlite::Result<()> {
    conn.execute_batch(&format!(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous={};
         CREATE TABLE IF NOT EXISTS items(id INTEGER PRIMARY KEY, value TEXT NOT NULL);
         DELETE FROM items;",
        synchronous.as_pragma()
    ))?;
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
    counters: bool,
    busy_poll: bool,
    sqpoll_idle: Option<u32>,
    synchronous: Synchronous,
}

impl Args {
    fn parse() -> Result<Self, Box<dyn std::error::Error>> {
        let mut runtime = RuntimeMode::Stack;
        let mut path = None;
        let mut iterations = 100_usize;
        let mut counters = false;
        let mut busy_poll = false;
        let mut sqpoll_idle = None;
        let mut synchronous = Synchronous::Full;
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
                "--counters" => {
                    counters = true;
                }
                "--busy-poll" => {
                    busy_poll = true;
                }
                "--sqpoll-idle-ms" => {
                    let Some(value) = args.next() else {
                        return Err("--sqpoll-idle-ms requires an integer".into());
                    };
                    sqpoll_idle = Some(value.parse()?);
                }
                "--synchronous" => {
                    let Some(value) = args.next() else {
                        return Err("--synchronous requires full, normal, or off".into());
                    };
                    synchronous = parse_synchronous(&value)?;
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
            counters,
            busy_poll,
            sqpoll_idle,
            synchronous,
        })
    }
}

#[derive(Clone, Copy)]
enum Synchronous {
    Full,
    Normal,
    Off,
}

impl Synchronous {
    fn as_pragma(self) -> &'static str {
        match self {
            Self::Full => "FULL",
            Self::Normal => "NORMAL",
            Self::Off => "OFF",
        }
    }
}

fn parse_synchronous(value: &str) -> Result<Synchronous, Box<dyn std::error::Error>> {
    match value {
        "full" | "FULL" => Ok(Synchronous::Full),
        "normal" | "NORMAL" => Ok(Synchronous::Normal),
        "off" | "OFF" => Ok(Synchronous::Off),
        _ => {
            Err(format!("unsupported synchronous mode {value:?}; use full, normal, or off").into())
        }
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
         --runtime <stack|steal|default-vfs> [--path PATH] [--iterations N] \\
         [--busy-poll] [--sqpoll-idle-ms N] [--synchronous full|normal|off] [--counters]"
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
