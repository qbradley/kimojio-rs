// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::ffi::c_void;
use std::hint::black_box;
use std::mem::size_of;
use std::time::Duration;

use arrayvec::ArrayVec;
use backtrace::Frame;
use criterion::measurement::WallTime;
use criterion::{BenchmarkGroup, BenchmarkId, Criterion, criterion_group, criterion_main};

type FramePointers = (*mut c_void, *mut c_void);
type InstructionPointer = *mut c_void;

fn trace_arrayvec<const HEIGHT: usize>() -> ArrayVec<Frame, HEIGHT> {
    let mut frames = ArrayVec::new();
    backtrace::trace(|frame| {
        frames.push(frame.clone());
        frames.len() < HEIGHT
    });
    frames
}

fn trace_arrayvec_ip_sp<const HEIGHT: usize>() -> ArrayVec<FramePointers, HEIGHT> {
    let mut frames = ArrayVec::new();
    backtrace::trace(|frame| {
        frames.push((frame.ip(), frame.sp()));
        frames.len() < HEIGHT
    });
    frames
}

#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
mod x86_64_linux_frame_pointer {
    use std::arch::asm;

    use super::{ArrayVec, InstructionPointer};

    const MAX_FRAME_SIZE: usize = 1024 * 1024;

    #[inline(always)]
    fn current_frame_pointer() -> *const usize {
        let rbp: *const usize;
        unsafe {
            asm!(
                "mov {}, rbp",
                out(reg) rbp,
                options(nomem, nostack, preserves_flags)
            );
        }
        rbp
    }

    #[inline(always)]
    pub fn trace_ips<const HEIGHT: usize>() -> ArrayVec<InstructionPointer, HEIGHT> {
        let mut ips = ArrayVec::new();
        let mut frame = current_frame_pointer();

        // x86_64 SysV frame layout: [rbp] is previous rbp, [rbp + 8] is return IP.
        while ips.len() < HEIGHT && !frame.is_null() {
            let next_frame = unsafe { frame.read() as *const usize };
            let ip = unsafe { frame.add(1).read() as InstructionPointer };

            if ip.is_null() {
                break;
            }
            ips.push(ip);

            let frame_addr = frame as usize;
            let next_addr = next_frame as usize;
            if next_addr <= frame_addr || next_addr - frame_addr > MAX_FRAME_SIZE {
                break;
            }

            frame = next_frame;
        }

        ips
    }
}

fn trace_vec(height: usize) -> Vec<Frame> {
    let mut frames = Vec::with_capacity(height);
    backtrace::trace(|frame| {
        frames.push(frame.clone());
        frames.len() < height
    });
    frames
}

fn bench_height<const HEIGHT: usize>(group: &mut BenchmarkGroup<'_, WallTime>) {
    // group.bench_function(BenchmarkId::new("arrayvec", HEIGHT), |b| {
    //     b.iter(|| black_box(trace_arrayvec::<HEIGHT>()))
    // });

    group.bench_function(BenchmarkId::new("arrayvec_ip_sp", HEIGHT), |b| {
        b.iter(|| black_box(trace_arrayvec_ip_sp::<HEIGHT>()))
    });

    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    group.bench_function(BenchmarkId::new("frame_pointer_ip", HEIGHT), |b| {
        b.iter(|| black_box(x86_64_linux_frame_pointer::trace_ips::<HEIGHT>()))
    });

    // group.bench_function(BenchmarkId::new("vec", HEIGHT), |b| {
    //     b.iter(|| black_box(trace_vec(HEIGHT)))
    // });
}

pub fn benchmark(c: &mut Criterion) {
    eprintln!("backtrace::Frame size: {} bytes", size_of::<Frame>());

    let mut group = c.benchmark_group("backtrace");
    bench_height::<5>(&mut group);
    bench_height::<10>(&mut group);
    bench_height::<15>(&mut group);
    bench_height::<20>(&mut group);
    group.finish();
}

criterion_group!(
    name = benches;
    config = Criterion::default().warm_up_time(Duration::from_secs(1)).measurement_time(Duration::from_secs(4));
    targets=benchmark
);
criterion_main!(benches);
