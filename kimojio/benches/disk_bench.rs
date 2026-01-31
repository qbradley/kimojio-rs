// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Disk I/O benchmarks measuring the overhead of the dispatch system.
//!
//! These benchmarks read and write a 4KB block at offset 0 repeatedly
//! to measure pure dispatch overhead without involving significant data movement.
//!
//! ## Environment Variables
//!
//! - `BENCH_FILE`: Path to a file for benchmarking. If not set, uses a temp file.
//!   For polled I/O benchmarks to work, this should be on a filesystem that
//!   supports O_DIRECT and polled I/O (e.g., NVMe devices with io_uring polling).
//!
//! - `NVME_DEVICE`: Path to an NVMe generic device (e.g., /dev/ng0n1) for
//!   uring_cmd passthrough benchmarks. Required for io_uring_cmd benchmarks.
//!
//! ## Running the benchmarks
//!
//! ```bash
//! # Basic benchmarks (pread/pwrite without polling)
//! cargo bench --bench disk_bench
//!
//! # With a specific file (needed for polled I/O on NVMe)
//! BENCH_FILE=/path/to/nvme/file cargo bench --bench disk_bench
//!
//! # With NVMe passthrough (requires io_uring_cmd feature)
//! NVME_DEVICE=/dev/ng0n1 cargo bench --bench disk_bench --features io_uring_cmd
//! ```

use std::cell::Cell;
use std::env;
use std::ffi::CString;
use std::io::{Seek, Write};
use std::rc::Rc;
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use rustix::fs::Mode;

use kimojio::operations::{self, OFlags};

const BLOCK_SIZE: usize = 4096;

/// Get the benchmark file path from environment or create a temp file.
/// Returns the path and an optional temp file handle (to keep it alive).
fn get_bench_file() -> (String, Option<tempfile::NamedTempFile>) {
    if let Ok(path) = env::var("BENCH_FILE") {
        // Ensure the file exists and has content
        if let Ok(metadata) = std::fs::metadata(&path)
            && metadata.len() >= BLOCK_SIZE as u64
        {
            return (path, None);
        }
        // File doesn't exist or is too small, create/extend it
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .expect("Failed to open BENCH_FILE");
        let data = vec![0xAAu8; BLOCK_SIZE];
        file.write_all(&data).expect("Failed to write initial data");
        file.flush().expect("Failed to flush file");
        (path, None)
    } else {
        let mut file = tempfile::NamedTempFile::new().expect("Failed to create temp file");
        let data = vec![0xAAu8; BLOCK_SIZE];
        file.write_all(&data).expect("Failed to write initial data");
        file.flush().expect("Failed to flush file");
        file.seek(std::io::SeekFrom::Start(0))
            .expect("Failed to seek");
        let path = file.path().to_str().unwrap().to_string();
        (path, Some(file))
    }
}

/// Benchmark pread_polled with polled=false (interrupt-driven completion)
pub fn benchmark_pread_not_polled(c: &mut Criterion) {
    let mut group = c.benchmark_group("disk_pread");

    group.bench_function(BenchmarkId::new("pread", "polled=false"), |b| {
        b.iter_custom(|iters| {
            let (path, _temp_file) = get_bench_file();
            let duration = Rc::new(Cell::new(Duration::new(1, 1)));
            let dur_copy = duration.clone();

            kimojio::run(0, async move {
                let f = async move {
                    let c_path = CString::new(path).unwrap();
                    let fd = operations::open(&c_path, OFlags::RDONLY, Mode::empty())
                        .await
                        .expect("Failed to open file");

                    // Allocate aligned buffer for O_DIRECT compatibility
                    let layout = std::alloc::Layout::from_size_align(BLOCK_SIZE, 512).unwrap();
                    let ptr = unsafe { std::alloc::alloc(layout) };
                    let buf = unsafe { std::slice::from_raw_parts_mut(ptr, BLOCK_SIZE) };

                    let start = Instant::now();
                    for _ in 0..iters {
                        let _amount = operations::pread_polled(&fd, buf, 0, false)
                            .await
                            .expect("pread failed");
                    }
                    dur_copy.set(start.elapsed());

                    unsafe { std::alloc::dealloc(ptr, layout) };
                };
                operations::spawn_task(f);
            });
            duration.get()
        });
    });

    group.finish();
}

/// Benchmark pread_polled with polled=true (polling completion)
///
/// Note: Polled I/O requires hardware/kernel support. This benchmark will
/// print a warning and skip if the operation is not supported.
pub fn benchmark_pread_polled(c: &mut Criterion) {
    let mut group = c.benchmark_group("disk_pread");

    // Check if polled I/O is supported by doing a test operation
    let polled_supported = {
        let (path, _temp_file) = get_bench_file();
        let supported = Rc::new(Cell::new(false));
        let supported_copy = supported.clone();

        kimojio::run(0, async move {
            let f = async move {
                let c_path = CString::new(path).unwrap();
                let fd =
                    match operations::open(&c_path, OFlags::RDONLY | OFlags::DIRECT, Mode::empty())
                        .await
                    {
                        Ok(fd) => fd,
                        Err(_) => return,
                    };

                let layout = std::alloc::Layout::from_size_align(BLOCK_SIZE, 512).unwrap();
                let ptr = unsafe { std::alloc::alloc(layout) };
                let buf = unsafe { std::slice::from_raw_parts_mut(ptr, BLOCK_SIZE) };

                if operations::pread_polled(&fd, buf, 0, true).await.is_ok() {
                    supported_copy.set(true);
                }

                unsafe { std::alloc::dealloc(ptr, layout) };
            };
            operations::spawn_task(f);
        });
        supported.get()
    };

    if !polled_supported {
        eprintln!(
            "Skipping pread polled=true benchmark: polled I/O not supported.\n\
             Set BENCH_FILE to a file on an NVMe device with polled I/O support."
        );
        group.finish();
        return;
    }

    group.bench_function(BenchmarkId::new("pread", "polled=true"), |b| {
        b.iter_custom(|iters| {
            let (path, _temp_file) = get_bench_file();
            let duration = Rc::new(Cell::new(Duration::new(1, 1)));
            let dur_copy = duration.clone();

            kimojio::run(0, async move {
                let f = async move {
                    let c_path = CString::new(path).unwrap();
                    let fd =
                        operations::open(&c_path, OFlags::RDONLY | OFlags::DIRECT, Mode::empty())
                            .await
                            .expect("Failed to open file with O_DIRECT");

                    let layout = std::alloc::Layout::from_size_align(BLOCK_SIZE, 512).unwrap();
                    let ptr = unsafe { std::alloc::alloc(layout) };
                    let buf = unsafe { std::slice::from_raw_parts_mut(ptr, BLOCK_SIZE) };

                    let start = Instant::now();
                    for _ in 0..iters {
                        let _amount = operations::pread_polled(&fd, buf, 0, true)
                            .await
                            .expect("pread polled failed");
                    }
                    dur_copy.set(start.elapsed());

                    unsafe { std::alloc::dealloc(ptr, layout) };
                };
                operations::spawn_task(f);
            });
            duration.get()
        });
    });

    group.finish();
}

/// Benchmark pwrite_polled with polled=false (interrupt-driven completion)
pub fn benchmark_pwrite_not_polled(c: &mut Criterion) {
    let mut group = c.benchmark_group("disk_pwrite");

    group.bench_function(BenchmarkId::new("pwrite", "polled=false"), |b| {
        b.iter_custom(|iters| {
            let (path, _temp_file) = get_bench_file();
            let duration = Rc::new(Cell::new(Duration::new(1, 1)));
            let dur_copy = duration.clone();

            kimojio::run(0, async move {
                let f = async move {
                    let c_path = CString::new(path).unwrap();
                    let fd = operations::open(&c_path, OFlags::WRONLY, Mode::empty())
                        .await
                        .expect("Failed to open file");

                    let layout = std::alloc::Layout::from_size_align(BLOCK_SIZE, 512).unwrap();
                    let ptr = unsafe { std::alloc::alloc(layout) };
                    let buf = unsafe { std::slice::from_raw_parts(ptr, BLOCK_SIZE) };

                    let start = Instant::now();
                    for _ in 0..iters {
                        let _amount = operations::pwrite_polled(&fd, buf, 0, false)
                            .await
                            .expect("pwrite failed");
                    }
                    dur_copy.set(start.elapsed());

                    unsafe { std::alloc::dealloc(ptr, layout) };
                };
                operations::spawn_task(f);
            });
            duration.get()
        });
    });

    group.finish();
}

/// Benchmark pwrite_polled with polled=true (polling completion)
///
/// Note: Polled I/O requires hardware/kernel support. This benchmark will
/// print a warning and skip if the operation is not supported.
pub fn benchmark_pwrite_polled(c: &mut Criterion) {
    let mut group = c.benchmark_group("disk_pwrite");

    // Check if polled I/O is supported by doing a test operation
    let polled_supported = {
        let (path, _temp_file) = get_bench_file();
        let supported = Rc::new(Cell::new(false));
        let supported_copy = supported.clone();

        kimojio::run(0, async move {
            let f = async move {
                let c_path = CString::new(path).unwrap();
                let fd =
                    match operations::open(&c_path, OFlags::WRONLY | OFlags::DIRECT, Mode::empty())
                        .await
                    {
                        Ok(fd) => fd,
                        Err(_) => return,
                    };

                let layout = std::alloc::Layout::from_size_align(BLOCK_SIZE, 512).unwrap();
                let ptr = unsafe { std::alloc::alloc(layout) };
                let buf = unsafe { std::slice::from_raw_parts(ptr, BLOCK_SIZE) };

                if operations::pwrite_polled(&fd, buf, 0, true).await.is_ok() {
                    supported_copy.set(true);
                }

                unsafe { std::alloc::dealloc(ptr, layout) };
            };
            operations::spawn_task(f);
        });
        supported.get()
    };

    if !polled_supported {
        eprintln!(
            "Skipping pwrite polled=true benchmark: polled I/O not supported.\n\
             Set BENCH_FILE to a file on an NVMe device with polled I/O support."
        );
        group.finish();
        return;
    }

    group.bench_function(BenchmarkId::new("pwrite", "polled=true"), |b| {
        b.iter_custom(|iters| {
            let (path, _temp_file) = get_bench_file();
            let duration = Rc::new(Cell::new(Duration::new(1, 1)));
            let dur_copy = duration.clone();

            kimojio::run(0, async move {
                let f = async move {
                    let c_path = CString::new(path).unwrap();
                    let fd =
                        operations::open(&c_path, OFlags::WRONLY | OFlags::DIRECT, Mode::empty())
                            .await
                            .expect("Failed to open file with O_DIRECT");

                    let layout = std::alloc::Layout::from_size_align(BLOCK_SIZE, 512).unwrap();
                    let ptr = unsafe { std::alloc::alloc(layout) };
                    let buf = unsafe { std::slice::from_raw_parts(ptr, BLOCK_SIZE) };

                    let start = Instant::now();
                    for _ in 0..iters {
                        let _amount = operations::pwrite_polled(&fd, buf, 0, true)
                            .await
                            .expect("pwrite polled failed");
                    }
                    dur_copy.set(start.elapsed());

                    unsafe { std::alloc::dealloc(ptr, layout) };
                };
                operations::spawn_task(f);
            });
            duration.get()
        });
    });

    group.finish();
}

/// Benchmark uring_cmd passthrough read (requires io_uring_cmd feature and NVMe device)
#[cfg(feature = "io_uring_cmd")]
pub fn benchmark_uring_cmd_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("disk_uring_cmd");

    // Skip if no NVMe device path is provided
    let nvme_path = match env::var("NVME_DEVICE") {
        Ok(path) => path,
        Err(_) => {
            eprintln!(
                "Skipping uring_cmd benchmarks: NVME_DEVICE environment variable not set.\n\
                 Set NVME_DEVICE to an NVMe generic device path like /dev/ng0n1"
            );
            group.finish();
            return;
        }
    };

    group.bench_function(BenchmarkId::new("uring_cmd", "read"), |b| {
        b.iter_custom(|iters| {
            let nvme_path = nvme_path.clone();
            let duration = Rc::new(Cell::new(Duration::new(1, 1)));
            let dur_copy = duration.clone();

            kimojio::run(0, async move {
                let f = async move {
                    let c_path = CString::new(nvme_path).unwrap();
                    let fd = operations::open(&c_path, OFlags::RDWR, Mode::empty())
                        .await
                        .expect("Failed to open NVMe device");

                    // Allocate aligned buffer for NVMe I/O
                    let layout = std::alloc::Layout::from_size_align(BLOCK_SIZE, 4096).unwrap();
                    let ptr = unsafe { std::alloc::alloc(layout) };

                    // Build NVMe read command
                    // This is a simplified NVMe read command structure
                    let start = Instant::now();
                    for _ in 0..iters {
                        let mut cmd = [0u8; 80];
                        // NVMe Read command opcode in CDW0
                        cmd[0] = 0x02; // NVMe Read opcode
                        // Set data pointer (DPTR) - offset 24-31 for PRP1
                        let ptr_bytes = (ptr as u64).to_le_bytes();
                        cmd[24..32].copy_from_slice(&ptr_bytes);
                        // Set LBA in CDW10-11 (offset 40-47)
                        let lba: u64 = 0;
                        let lba_bytes = lba.to_le_bytes();
                        cmd[40..48].copy_from_slice(&lba_bytes);
                        // Set number of blocks - 1 in CDW12 (offset 48-51)
                        let nlb: u32 = 0; // 0 means 1 block
                        let nlb_bytes = nlb.to_le_bytes();
                        cmd[48..52].copy_from_slice(&nlb_bytes);

                        // NVME_URING_CMD_IO = 0 for passthrough I/O
                        let _ = operations::uring_cmd(&fd, 0, cmd)
                            .await
                            .expect("uring_cmd read failed");
                    }
                    dur_copy.set(start.elapsed());

                    unsafe { std::alloc::dealloc(ptr, layout) };
                };
                operations::spawn_task(f);
            });
            duration.get()
        });
    });

    group.finish();
}

/// Benchmark uring_cmd passthrough write (requires io_uring_cmd feature and NVMe device)
#[cfg(feature = "io_uring_cmd")]
pub fn benchmark_uring_cmd_write(c: &mut Criterion) {
    let mut group = c.benchmark_group("disk_uring_cmd");

    // Skip if no NVMe device path is provided
    let nvme_path = match env::var("NVME_DEVICE") {
        Ok(path) => path,
        Err(_) => {
            // Already warned in read benchmark
            group.finish();
            return;
        }
    };

    group.bench_function(BenchmarkId::new("uring_cmd", "write"), |b| {
        b.iter_custom(|iters| {
            let nvme_path = nvme_path.clone();
            let duration = Rc::new(Cell::new(Duration::new(1, 1)));
            let dur_copy = duration.clone();

            kimojio::run(0, async move {
                let f = async move {
                    let c_path = CString::new(nvme_path).unwrap();
                    let fd = operations::open(&c_path, OFlags::RDWR, Mode::empty())
                        .await
                        .expect("Failed to open NVMe device");

                    // Allocate aligned buffer for NVMe I/O
                    let layout = std::alloc::Layout::from_size_align(BLOCK_SIZE, 4096).unwrap();
                    let ptr = unsafe { std::alloc::alloc(layout) };
                    // Fill buffer with test pattern
                    unsafe {
                        std::ptr::write_bytes(ptr, 0xBB, BLOCK_SIZE);
                    }

                    // Build NVMe write command
                    let start = Instant::now();
                    for _ in 0..iters {
                        let mut cmd = [0u8; 80];
                        // NVMe Write command opcode in CDW0
                        cmd[0] = 0x01; // NVMe Write opcode
                        // Set data pointer (DPTR) - offset 24-31 for PRP1
                        let ptr_bytes = (ptr as u64).to_le_bytes();
                        cmd[24..32].copy_from_slice(&ptr_bytes);
                        // Set LBA in CDW10-11 (offset 40-47)
                        let lba: u64 = 0;
                        let lba_bytes = lba.to_le_bytes();
                        cmd[40..48].copy_from_slice(&lba_bytes);
                        // Set number of blocks - 1 in CDW12 (offset 48-51)
                        let nlb: u32 = 0; // 0 means 1 block
                        let nlb_bytes = nlb.to_le_bytes();
                        cmd[48..52].copy_from_slice(&nlb_bytes);

                        // NVME_URING_CMD_IO = 0 for passthrough I/O
                        let _ = operations::uring_cmd(&fd, 0, cmd)
                            .await
                            .expect("uring_cmd write failed");
                    }
                    dur_copy.set(start.elapsed());

                    unsafe { std::alloc::dealloc(ptr, layout) };
                };
                operations::spawn_task(f);
            });
            duration.get()
        });
    });

    group.finish();
}

/// Benchmark uring_cmd_polled passthrough read (requires io_uring_cmd feature and NVMe device)
#[cfg(feature = "io_uring_cmd")]
pub fn benchmark_uring_cmd_polled_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("disk_uring_cmd_polled");

    // Skip if no NVMe device path is provided
    let nvme_path = match env::var("NVME_DEVICE") {
        Ok(path) => path,
        Err(_) => {
            group.finish();
            return;
        }
    };

    group.bench_function(BenchmarkId::new("uring_cmd_polled", "read"), |b| {
        b.iter_custom(|iters| {
            let nvme_path = nvme_path.clone();
            let duration = Rc::new(Cell::new(Duration::new(1, 1)));
            let dur_copy = duration.clone();

            kimojio::run(0, async move {
                let f = async move {
                    let c_path = CString::new(nvme_path).unwrap();
                    let fd = operations::open(&c_path, OFlags::RDWR, Mode::empty())
                        .await
                        .expect("Failed to open NVMe device");

                    // Allocate aligned buffer for NVMe I/O
                    let layout = std::alloc::Layout::from_size_align(BLOCK_SIZE, 4096).unwrap();
                    let ptr = unsafe { std::alloc::alloc(layout) };

                    let start = Instant::now();
                    for _ in 0..iters {
                        let mut cmd = [0u8; 80];
                        cmd[0] = 0x02; // NVMe Read opcode
                        let ptr_bytes = (ptr as u64).to_le_bytes();
                        cmd[24..32].copy_from_slice(&ptr_bytes);
                        let lba: u64 = 0;
                        let lba_bytes = lba.to_le_bytes();
                        cmd[40..48].copy_from_slice(&lba_bytes);
                        let nlb: u32 = 0;
                        let nlb_bytes = nlb.to_le_bytes();
                        cmd[48..52].copy_from_slice(&nlb_bytes);

                        let _ = operations::uring_cmd_polled(&fd, 0, cmd)
                            .await
                            .expect("uring_cmd_polled read failed");
                    }
                    dur_copy.set(start.elapsed());

                    unsafe { std::alloc::dealloc(ptr, layout) };
                };
                operations::spawn_task(f);
            });
            duration.get()
        });
    });

    group.finish();
}

/// Benchmark uring_cmd_polled passthrough write (requires io_uring_cmd feature and NVMe device)
#[cfg(feature = "io_uring_cmd")]
pub fn benchmark_uring_cmd_polled_write(c: &mut Criterion) {
    let mut group = c.benchmark_group("disk_uring_cmd_polled");

    // Skip if no NVMe device path is provided
    let nvme_path = match env::var("NVME_DEVICE") {
        Ok(path) => path,
        Err(_) => {
            group.finish();
            return;
        }
    };

    group.bench_function(BenchmarkId::new("uring_cmd_polled", "write"), |b| {
        b.iter_custom(|iters| {
            let nvme_path = nvme_path.clone();
            let duration = Rc::new(Cell::new(Duration::new(1, 1)));
            let dur_copy = duration.clone();

            kimojio::run(0, async move {
                let f = async move {
                    let c_path = CString::new(nvme_path).unwrap();
                    let fd = operations::open(&c_path, OFlags::RDWR, Mode::empty())
                        .await
                        .expect("Failed to open NVMe device");

                    // Allocate aligned buffer for NVMe I/O
                    let layout = std::alloc::Layout::from_size_align(BLOCK_SIZE, 4096).unwrap();
                    let ptr = unsafe { std::alloc::alloc(layout) };
                    unsafe {
                        std::ptr::write_bytes(ptr, 0xBB, BLOCK_SIZE);
                    }

                    let start = Instant::now();
                    for _ in 0..iters {
                        let mut cmd = [0u8; 80];
                        cmd[0] = 0x01; // NVMe Write opcode
                        let ptr_bytes = (ptr as u64).to_le_bytes();
                        cmd[24..32].copy_from_slice(&ptr_bytes);
                        let lba: u64 = 0;
                        let lba_bytes = lba.to_le_bytes();
                        cmd[40..48].copy_from_slice(&lba_bytes);
                        let nlb: u32 = 0;
                        let nlb_bytes = nlb.to_le_bytes();
                        cmd[48..52].copy_from_slice(&nlb_bytes);

                        let _ = operations::uring_cmd_polled(&fd, 0, cmd)
                            .await
                            .expect("uring_cmd_polled write failed");
                    }
                    dur_copy.set(start.elapsed());

                    unsafe { std::alloc::dealloc(ptr, layout) };
                };
                operations::spawn_task(f);
            });
            duration.get()
        });
    });

    group.finish();
}

// Conditional compilation for criterion groups based on features
#[cfg(feature = "io_uring_cmd")]
criterion_group!(
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(4));
    targets =
        benchmark_pread_not_polled,
        benchmark_pread_polled,
        benchmark_pwrite_not_polled,
        benchmark_pwrite_polled,
        benchmark_uring_cmd_read,
        benchmark_uring_cmd_write,
        benchmark_uring_cmd_polled_read,
        benchmark_uring_cmd_polled_write
);

#[cfg(not(feature = "io_uring_cmd"))]
criterion_group!(
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(4));
    targets =
        benchmark_pread_not_polled,
        benchmark_pread_polled,
        benchmark_pwrite_not_polled,
        benchmark_pwrite_polled
);

criterion_main!(benches);
